//! `tasks status [--json]`: the daemon's local machine snapshot (v1). The
//! JSON is the product (one line, for pollers); the text form is a summary.
use std::fmt::Write as _;
use std::time::Duration;
use tinline_task_manager::metrics::{JobView, Snapshot};
use tinline_task_manager::{Result, request_status, state_dir};

pub fn run(json: bool) -> Result<()> {
    let socket = state_dir().join("taskd.sock");
    crate::ensure_daemon(&socket)?;
    let reply = request_status(&socket, Duration::from_secs(60))?;
    let Some(snapshot) = reply.status else {
        return Err(reply
            .error
            .unwrap_or_else(|| "daemon returned no status".into())
            .into());
    };
    if json {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        print!("{}", render(&snapshot));
    }
    Ok(())
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

fn seconds(s: f64) -> String {
    let s = s.round() as u64;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn job_line(j: &JobView) -> String {
    let phase = serde_json::to_value(&j.phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut line = format!(
        "#{} {phase} {} [{}] {}",
        j.id, j.owner, j.checkout_name, j.command
    );
    if let Some(live) = &j.live {
        match live.cpu_cores {
            Some(c) => write!(line, " | {c:.1} cores").unwrap(),
            None => line.push_str(" | - cores"),
        }
        write!(
            line,
            " {} GiB {} procs",
            gib(live.rss_bytes),
            live.processes
        )
        .unwrap();
    }
    if let Some(p) = &j.progress {
        match p.crates_expected {
            Some(e) => write!(line, " | {}/{e} crates", p.crates_compiled).unwrap(),
            None => write!(line, " | {} crates", p.crates_compiled).unwrap(),
        }
    }
    write!(line, " | {}", j.warmth.as_str()).unwrap();
    if let Some(c) = j.compiling.as_ref().filter(|c| !c.is_empty()) {
        let shown: Vec<String> = c
            .iter()
            .take(4)
            .map(|c| format!("{} ({} {:.0}s)", c.unit, c.kind, c.elapsed_s))
            .collect();
        let more = c.len().saturating_sub(4);
        write!(line, " | compiling {}", shown.join(", ")).unwrap();
        if more > 0 {
            write!(line, " +{more}").unwrap();
        }
    }
    if j.broad_test_build == Some(true) {
        line.push_str(" | BROAD test build");
    }
    if let Some(e) = &j.estimate {
        write!(
            line,
            " | ~{} left of {} ({})",
            seconds(e.remaining_s),
            seconds(e.total_s),
            e.basis
        )
        .unwrap();
    }
    if let Some(q) = &j.queue {
        if let Some(p) = q.position {
            write!(line, " | queue #{p}").unwrap();
        }
        if let Some(w) = &q.waiting_for {
            write!(line, " | waiting for {w}").unwrap();
        }
    }
    if let Some(r) = &j.result {
        if let Some(w) = r.wall_s {
            write!(line, " | {}", seconds(w)).unwrap();
        }
        if let (Some(u), Some(s)) = (r.cpu_user_s, r.cpu_sys_s) {
            write!(line, " cpu {}", seconds(u + s)).unwrap();
        }
        if let Some(p) = r.peak_rss_bytes {
            write!(line, " peak {} GiB", gib(p)).unwrap();
        }
    }
    line
}

pub fn render(s: &Snapshot) -> String {
    let m = &s.machine;
    let mut out = String::new();
    let role = serde_json::to_value(m.role)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    write!(out, "{} ({role}): ", m.id).unwrap();
    match m.cpu_busy_cores {
        Some(c) => write!(out, "cpu {c:.1}/{} cores", m.logical_cpus).unwrap(),
        None => write!(out, "cpu -/{} cores", m.logical_cpus).unwrap(),
    }
    if let (Some(t), Some(a)) = (m.mem_total_bytes, m.mem_available_bytes) {
        write!(
            out,
            ", mem {}/{} GiB used",
            gib(t.saturating_sub(a)),
            gib(t)
        )
        .unwrap();
    }
    if let (Some(t), Some(u)) = (m.swap_total_bytes, m.swap_used_bytes) {
        write!(out, ", swap {}/{} GiB", gib(u), gib(t)).unwrap();
    }
    if let Some(p) = &m.pressure {
        write!(
            out,
            ", PSI cpu {:.1} mem {:.1}/{:.1} io {:.1}",
            p.cpu_some_avg10, p.memory_some_avg10, p.memory_full_avg10, p.io_some_avg10
        )
        .unwrap();
    }
    out.push('\n');
    writeln!(out, "blocker: {}", m.blocker.as_deref().unwrap_or("none")).unwrap();
    let slots: Vec<String> = m
        .slots
        .iter()
        .filter(|s| s.busy)
        .map(|s| match (s.holder_job, &s.holder_who) {
            (Some(j), Some(w)) => format!("{} #{j} {w}", s.name),
            (_, Some(w)) => format!("{} {w}", s.name),
            _ => format!("{} busy", s.name),
        })
        .collect();
    writeln!(
        out,
        "busy slots: {}",
        if slots.is_empty() {
            "none".into()
        } else {
            slots.join("; ")
        }
    )
    .unwrap();
    let live: Vec<&JobView> = s.jobs.iter().filter(|j| !j.phase.terminal()).collect();
    if live.is_empty() {
        out.push_str("no live jobs\n");
    }
    for j in live {
        writeln!(out, "{}", job_line(j)).unwrap();
    }
    if let Some(j) = s.jobs.iter().rev().find(|j| j.phase.terminal()) {
        writeln!(out, "last finished: {}", job_line(j)).unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_the_fixture_snapshots_without_panicking() {
        for name in ["metrics-laptop-busy", "metrics-worker-busy", "metrics-idle"] {
            let path = format!(
                "{}/tests/fixtures/snapshot/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let snapshot: Snapshot =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let text = render(&snapshot);
            assert!(text.starts_with(&snapshot.machine.id), "{text}");
            assert!(text.contains("blocker:"));
        }
    }
}
