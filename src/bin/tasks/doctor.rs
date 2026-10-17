//! `tasks doctor`: the node-setup checks (docs/design/node-setup-policy.md) in one report with
//! fix lines: this daemon's latest health pass (slot-lock holders, compiler-cache server,
//! declared capabilities, disks and the bounded disk scan, every other alert) plus this
//! machine's capability probes run now, and each peer's alerts. Read-only. Exit 1 when this
//! machine has a warn/critical alert or a declared capability is missing.
use tinline_task_manager::health::Severity;
use tinline_task_manager::health::cluster;
use tinline_task_manager::health::node;
use tinline_task_manager::{Result, state_dir};

const USAGE: &str = "usage: tasks doctor [--json]";

pub fn run(args: &[String]) -> Result<bool> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return Err(USAGE.into()),
    };
    let socket = state_dir().join("taskd.sock");
    crate::ensure_daemon(&socket)?;
    let view = cluster::gather(&socket);
    let now = view.generated_at;
    let local = view.nodes.iter().find(|n| n.name == "local");
    // The daemon's probes (its environment is what jobs get); this process's as a fallback.
    let (probes, probe_source) = match local.map(|n| n.probes.clone()) {
        Some(p) if !p.is_empty() => (p, "daemon environment"),
        _ => (
            node::live_probes(&|n| std::env::var_os(n)),
            "this shell (the daemon published no probes: older release or first pass pending)",
        ),
    };
    let local_ids: Vec<&str> = ["local"]
        .into_iter()
        .chain(local.and_then(|n| n.machine_id.as_deref()))
        .collect();
    let bad: Vec<&cluster::ClusterAlert> = view
        .alerts
        .iter()
        .filter(|a| a.alert.severity >= Severity::Warn)
        .filter(|a| a.reported_by == "local" || local_ids.contains(&a.alert.node.as_str()))
        .collect();
    let missing_declared = probes.iter().filter(|p| p.declared && !p.ok).count();
    let healthy = bad.is_empty() && missing_declared == 0;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "healthy": healthy,
                "probes": probes,
                "probe_source": probe_source,
                "cluster": view,
            }))?
        );
        return Ok(healthy);
    }
    println!(
        "capabilities ({probe_source}; declare with {}):",
        node::DECLARED_ENV
    );
    for p in &probes {
        let mark = match (p.ok, p.declared) {
            (true, _) => "ok  ",
            (false, true) => "FAIL",
            (false, false) => "--  ",
        };
        let declared = if p.declared { " [declared]" } else { "" };
        println!("  {mark} {}{declared}: {}", p.name, p.detail);
        if !p.ok && p.declared {
            println!("       fix: {}", p.fix);
        }
    }
    if let Some(scan) = local.and_then(|n| n.scan.as_ref()) {
        println!(
            "disk scan: bounded to {} entries / {:.0}s, idle I/O priority, no symlinks; last {:.1}s wall, \
             {:.2}s CPU, {} entries{}",
            scan.max_entries,
            scan.max_s,
            scan.elapsed_s,
            scan.cpu_s,
            scan.entries,
            if scan.truncated {
                " (budget reached)"
            } else {
                ""
            }
        );
    }
    println!();
    print!("{}", view.render(now));
    println!(
        "\n{}",
        if healthy {
            "doctor: this machine is healthy (no warn/critical alert, declared capabilities present)"
        } else {
            "doctor: NOT healthy: see the alerts and fix lines above"
        }
    );
    Ok(healthy)
}
