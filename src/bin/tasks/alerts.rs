//! `tasks alerts`: active and recent health alerts of this machine and its peers, their
//! disks and missing-capability marks (feature/health-alerts). Read-only, except `clear`,
//! which asks the daemon to drop missing-capability marks.
use std::time::Duration;
use tinline_task_manager::health::Severity;
use tinline_task_manager::health::capability::{ClearRequest, request_clear};
use tinline_task_manager::health::cluster::{self, Filter};
use tinline_task_manager::{Result, state_dir};

const USAGE: &str =
    "usage: tasks alerts [--json] [--all] [--severity info|warn|critical] [--node NAME]
       tasks alerts clear NODE [CAPABILITY]
  Active health alerts of this machine and its peers (most severe first), with evidence and a
  suggested fix, each machine's disks (free space, growth, top consumers from the periodic
  bounded scan) and missing-capability marks. --all adds recently cleared alerts. `clear`
  removes a node's missing-capability mark(s) (e.g. after installing libclang there).";

pub fn run(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) == Some("clear") {
        return clear(&args[1..]);
    }
    let mut json = false;
    let mut filter = Filter::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--all" => filter.recent = true,
            "--severity" => {
                let value = it.next().ok_or(USAGE)?;
                filter.min_severity = Some(Severity::parse(value)?);
            }
            "--node" => filter.node = Some(it.next().ok_or(USAGE)?.clone()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let socket = state_dir().join("taskd.sock");
    crate::ensure_daemon(&socket)?;
    let view = cluster::gather(&socket).filtered(&filter);
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print!("{}", view.render(view.generated_at));
    }
    Ok(())
}

fn clear(args: &[String]) -> Result<()> {
    let (node, capability) = match args {
        [node] => (node.clone(), None),
        [node, capability] => (node.clone(), Some(capability.clone())),
        _ => return Err(USAGE.into()),
    };
    let state = state_dir();
    let path = request_clear(
        &state,
        &ClearRequest {
            node: node.clone(),
            capability: capability.clone(),
        },
    )?;
    // The daemon's health monitor consumes the request on its next pass.
    let socket = state.join("taskd.sock");
    crate::ensure_daemon(&socket)?;
    let end = std::time::Instant::now() + Duration::from_secs(90);
    while path.exists() && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(250));
    }
    if path.exists() {
        println!(
            "requested clearing {node} {}; the daemon has not processed it yet (its health monitor runs every TASKS_HEALTH_INTERVAL_S)",
            capability.as_deref().unwrap_or("(all capabilities)")
        );
    } else {
        println!(
            "cleared missing-capability mark(s) of {node} {}",
            capability.as_deref().unwrap_or("(all capabilities)")
        );
    }
    Ok(())
}
