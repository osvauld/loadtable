//! `tasks gc`: age-based cleanup of offload copies and checkout output caches (src/gc.rs).
use std::path::Path;
use tinline_task_manager::gc::{self, Kind, Roots};
use tinline_task_manager::{Result, policy, state_dir};

const USAGE: &str = "usage: tasks gc [--apply] [--json]
  Lists what is idle past the limits of the gc:: policy section (offload copies 3 days,
  checkout output caches 7 days by default) and how much it would give back. Removes nothing
  without --apply. Skips whatever a live job or a process of yours is using, and anything it
  could not scan completely. Offload copies go whole with their .identity file; output caches
  are emptied and their directories stay (the checkout's target link points at them).";

pub fn run(args: &[String]) -> Result<()> {
    let (mut apply, mut json) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            "--json" => json = true,
            _ => return Err(USAGE.into()),
        }
    }
    let effective = policy::effective();
    for error in &effective.errors {
        eprintln!("tasks gc: {error}");
    }
    let limits = effective.policy.gc.limits()?;
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let roots = Roots::of_home(Path::new(&home));
    let (using, daemon_answered) = gc::in_use_via(&state_dir().join("taskd.sock"));
    if apply && !daemon_answered {
        return Err(
            "the daemon did not answer, so live jobs are unknown; nothing removed \
                    (tasks list starts it, then rerun)"
                .into(),
        );
    }
    let plan = gc::plan(&roots, limits, &using, now());
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        for c in &plan.candidates {
            println!(
                "{:<7} {:>9}  idle {:<5} {}{}",
                match c.kind {
                    Kind::OffloadCopy => "copy",
                    Kind::OutputCache => "output",
                },
                gc::gib(c.bytes),
                gc::age(c.idle_s),
                c.path.display(),
                c.origin
                    .as_deref()
                    .map(|o| format!("  ({o})"))
                    .unwrap_or_default()
            );
        }
        for s in &plan.skipped {
            println!("kept    {}  ({})", s.path.display(), s.reason);
        }
        println!(
            "{} idle, {} to give back{}",
            plan.candidates.len(),
            gc::gib(plan.bytes()),
            if apply {
                ""
            } else {
                "; nothing removed (--apply removes)"
            }
        );
    }
    if apply {
        let mut failed = 0;
        for outcome in gc::apply(&plan, &roots) {
            match outcome.error {
                None if !json => println!("removed {}", outcome.path.display()),
                None => {}
                Some(e) => {
                    failed += 1;
                    eprintln!("tasks gc: {}: {e}", outcome.path.display());
                }
            }
        }
        if failed > 0 {
            return Err(format!("{failed} entries could not be removed").into());
        }
    }
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
