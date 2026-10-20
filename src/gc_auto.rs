//! The daemon's disk-pressure cleanup (`gc::relieve`): once a minute, when a cache disk has
//! less free than its reserve, the oldest idle cache entries go until it is `PRESSURE_HEADROOM`
//! above it. Policy `gc::` `auto_when_full: false` turns it off. Every removal is logged and
//! appended to `gc-audit.jsonl` in the state directory.
use crate::gc::{self, PRESSURE_HEADROOM, Roots};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(60);

pub fn start(state: PathBuf, socket: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("gc-auto".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            loop {
                if let Err(e) = pass(&state, &socket) {
                    eprintln!("gc-auto: {e}");
                }
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

fn pass(state: &Path, socket: &Path) -> crate::Result<()> {
    if !crate::policy::effective().policy.gc.auto_when_full() {
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let roots = Roots::of_home(Path::new(&home));
    let reserve = crate::admission::thresholds()?.min_cache_free_bytes;
    // The cheap check first: scanning the caches is only worth it under pressure.
    if [&roots.outputs, &roots.offload]
        .iter()
        .all(|r| !r.exists() || free(r).is_none_or(|f| f >= reserve))
    {
        return Ok(());
    }
    let (using, answered) = gc::in_use_via(socket);
    if !answered {
        return Err("daemon did not answer, so live jobs are unknown; nothing removed".into());
    }
    let removed = gc::relieve(
        &roots,
        &using,
        crate::execution::now(),
        reserve,
        reserve + PRESSURE_HEADROOM,
        &free,
    );
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
