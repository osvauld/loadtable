//! The daemon's own cache cleanup. Once a minute, when a cache disk has less free than its
//! reserve, the oldest idle cache entries go until it is `PRESSURE_HEADROOM` above it
//! (`gc::relieve`; policy `gc::` `auto_when_full: false` turns it off). With `gc::` `rotate:
//! true` it also applies the idle limits (`*_idle_hours` / `*_idle_days`) every ten minutes, so
//! caches rotate without anyone running `tasks gc`. Every removal is logged and appended to
//! `gc-audit.jsonl` in the state directory.
use crate::gc::{self, PRESSURE_HEADROOM, Roots};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(60);
/// A rotation sweep runs on every this many passes.
const ROTATE_EVERY: u32 = 10;

pub fn start(state: PathBuf, socket: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("gc-auto".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            let mut tick = 0u32;
            loop {
                if let Err(e) = pass(&state, &socket, tick.is_multiple_of(ROTATE_EVERY)) {
                    eprintln!("gc-auto: {e}");
                }
                tick = tick.wrapping_add(1);
                std::thread::sleep(INTERVAL);
            }
        });
    if let Err(e) = spawned {
        eprintln!("gc-auto: thread not started: {e}");
    }
}

fn free(path: &Path) -> Option<u64> {
    crate::cache::disk_space_raw(path)
        .ok()
        .map(|d| d.available_bytes)
}

fn pass(state: &Path, socket: &Path, rotation_due: bool) -> crate::Result<()> {
    let policy = crate::policy::effective().policy.gc;
    let rotate = policy.rotate() && rotation_due;
    if !policy.auto_when_full() && !rotate {
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let roots = Roots::of_home(Path::new(&home));
    let reserve = crate::admission::thresholds()?.min_cache_free_bytes;
    // The cheap check first: scanning the caches is only worth it under pressure or on rotation.
    let pressed = policy.auto_when_full()
        && ![&roots.outputs, &roots.offload]
            .iter()
            .all(|r| !r.exists() || free(r).is_none_or(|f| f >= reserve));
    if !pressed && !rotate {
        return Ok(());
    }
    let (using, answered) = gc::in_use_via(socket);
    if !answered {
        return Err("daemon did not answer, so live jobs are unknown; nothing removed".into());
    }
    let now = crate::execution::now();
    let mut removed = vec![];
    if rotate {
        let limits = policy.limits()?;
        let plan = gc::plan(&roots, limits, &using, now);
        let before: Vec<_> = plan.candidates.iter().map(|c| free(&c.path)).collect();
        let outcomes = gc::apply(&plan, &roots);
        for ((c, o), was) in plan.candidates.iter().zip(outcomes).zip(before) {
            removed.push(gc::Relieved {
                path: c.path.clone(),
                kind: c.kind,
                bytes: c.bytes,
                idle_s: c.idle_s,
                free_before: was.unwrap_or(0),
                free_after: free(&c.path).unwrap_or(0),
                error: o.error,
            });
        }
    }
    if pressed {
        removed.extend(gc::relieve(
            &roots,
            &using,
            now,
            reserve,
            reserve + PRESSURE_HEADROOM,
            &free,
        ));
    }
    for r in &removed {
        let line = match &r.error {
            None => format!(
                "gc-auto: removed {} ({}, idle {}); {} -> {} free (reserve {})",
                r.path.display(),
                gc::gib(r.bytes),
                gc::age(r.idle_s),
                gc::gib(r.free_before),
                gc::gib(r.free_after),
                gc::gib(reserve)
            ),
            Some(e) => format!("gc-auto: could not remove {}: {e}", r.path.display()),
        };
        eprintln!("{} {line}", crate::execution::now());
        append_audit(state, r);
    }
    Ok(())
}

fn append_audit(state: &Path, r: &gc::Relieved) {
    let Ok(mut line) = serde_json::to_value(r) else {
        return;
    };
    line["at"] = crate::execution::now().into();
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("gc-audit.jsonl"))
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}
