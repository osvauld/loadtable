//! Slot-lease audit at daemon start (and every few minutes): every process holding a slot
//! flock (`p2p-build.lock`, `p2p-run-N`, `p2p-offload-N`, device locks) must belong to a job
//! this ledger knows. A lease is never left untracked:
//!
//! * **tracked**: the holder is in the process group of the `__admit` leader that the lock's
//!   holder note names (pid + start ticks), for a job that is still active here. Nothing to do;
//!   the job's worker/supervisor own it (they survive a daemon restart, KillMode=process).
//! * **orphan**: same group match, but the ledger already ended that job (interrupted by a
//!   restart, cancelled, ...) more than [`ORPHAN_GRACE`] ago. Its whole process group is
//!   stopped (SIGTERM, SIGKILL after a grace), the same group a cancel would stop.
//! * **untracked**: no note, a note for a job this ledger does not have, or a holder outside
//!   the noted group (a legacy client, or a process that left the job's group, such as a
//!   daemonized compiler-cache server). Named loudly with pid/cmd/note; never signalled.
//!
//! Holders are found from `/proc/PID/fdinfo` (the open file description that owns the flock
//! has a `lock:` line), not from who merely has the file open (waiters do too).
use crate::{Job, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A just-finished job's command may still be exiting with the lease for a moment.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(60);
const KILL_GRACE: Duration = Duration::from_secs(5);
const EVERY: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub lock: String,
    pub pid: u32,
    pub pgid: u32,
    pub cmd: String,
}

/// The lock's holder note, as written by native admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRef {
    pub job: Option<i64>,
    pub leader: u32,
    pub who: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Tracked { job: i64 },
    Orphan { job: i64, pgid: u32 },
    Untracked { why: String },
}

/// `owner:job-N` -> N.
pub fn job_of(who: &str) -> Option<i64> {
    who.rsplit_once(":job-").and_then(|(_, n)| n.parse().ok())
}

/// Pure decision for one holder (tests drive it directly).
pub fn classify(
    holder: &Holder,
    note: Option<&NoteRef>,
    jobs: &HashMap<i64, Job>,
    now: i64,
) -> Verdict {
    let Some(note) = note else {
        return Verdict::Untracked {
            why: "no holder note (legacy client or note removed)".into(),
        };
    };
    let Some(id) = note.job else {
        return Verdict::Untracked {
            why: format!("note names no tasks job (who {:?})", note.who),
        };
    };
    if holder.pgid != note.leader {
        return Verdict::Untracked {
            why: format!(
                "holder is outside job {id}'s admission group {} (left the job's group, e.g. a daemonized helper)",
                note.leader
            ),
        };
    }
    let Some(job) = jobs.get(&id) else {
        return Verdict::Untracked {
            why: format!("job {id} is not in this ledger"),
        };
    };
    if !job.phase.terminal() {
        return Verdict::Tracked { job: id };
    }
    let finished = job
        .execution
        .as_ref()
        .and_then(|e| e.finished_at)
        .unwrap_or(job.submitted_at);
    if now - finished < ORPHAN_GRACE.as_secs() as i64 {
        return Verdict::Tracked { job: id };
    }
    Verdict::Orphan {
        job: id,
        pgid: holder.pgid,
    }
}

fn stat_fields(pid: u32) -> Option<(u32, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (head, rest) = stat.rsplit_once(')')?;
    let comm = head.split_once('(')?.1.to_owned();
    // After ')': state ppid pgrp ...
    let pgid = rest.split_whitespace().nth(2)?.parse().ok()?;
    Some((pgid, comm))
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(200)
                .collect()
        })
        .unwrap_or_default()
}

/// Slot lock files in the admission root.
fn lock_files(root: &Path) -> Vec<(String, PathBuf, (u64, u64))> {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(root) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            (name.starts_with("p2p-") && name.ends_with(".lock")).then_some(())?;
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            meta.is_file()
                .then(|| (name, e.path(), (meta.dev(), meta.ino())))
        })
        .collect()
}

/// Processes whose open file description OWNS a flock on one of the slot lock files.
pub fn holders(root: &Path) -> Vec<Holder> {
    use std::os::unix::fs::MetadataExt;
    let locks = lock_files(root);
    if locks.is_empty() {
        return vec![];
    }
    let mut out = vec![];
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for proc_entry in procs.flatten() {
        let Some(pid) = proc_entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(proc_entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(meta) = std::fs::metadata(fd.path()) else {
                continue;
            };
            let Some((name, _, _)) = locks
                .iter()
                .find(|(_, _, id)| *id == (meta.dev(), meta.ino()))
            else {
                continue;
            };
            let info =
                std::fs::read_to_string(proc_entry.path().join("fdinfo").join(fd.file_name()))
                    .unwrap_or_default();
            if !info
                .lines()
                .any(|l| l.starts_with("lock:") && l.contains("FLOCK"))
            {
                continue; // open but not holding (a waiter)
            }
            let Some((pgid, comm)) = stat_fields(pid) else {
                continue;
            };
            let cmd = match cmdline(pid) {
                c if c.is_empty() => comm,
                c => c,
            };
            let holder = Holder {
                lock: name.clone(),
                pid,
                pgid,
                cmd,
            };
            if !out.contains(&holder) {
                out.push(holder);
            }
        }
    }
    out
}

fn note_of(root: &Path, lock: &str) -> Option<NoteRef> {
    let path = root.join(lock).with_extension("holder");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 16384 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let who = v["who"].as_str().unwrap_or("").to_owned();
    Some(NoteRef {
        job: job_of(&who),
        leader: u32::try_from(v["pid"].as_u64()?).ok()?,
        who,
    })
}

/// Stops one orphaned admission group: SIGTERM, then SIGKILL after [`KILL_GRACE`].
fn reap(pgid: u32) {
    let own = unsafe { libc::getpgrp() } as u32;
    if pgid <= 1 || pgid == own {
        return;
    }
    let group = -(pgid as i32);
    unsafe { libc::kill(group, libc::SIGTERM) };
    let end = Instant::now() + KILL_GRACE;
    while Instant::now() < end {
        if unsafe { libc::kill(group, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(group, libc::SIGKILL) };
}

/// One audit pass; `log` gets one line per holder, BEFORE anything is signalled. `jobs` is the
/// ledger listing. Returns the lines.
pub fn audit(root: &Path, jobs: &[Job], log: &mut dyn FnMut(&str)) -> Vec<String> {
    let by_id: HashMap<i64, Job> = jobs.iter().map(|j| (j.id, j.clone())).collect();
    let now = crate::execution::now();
    let mut lines = vec![];
    let mut reaped = vec![];
    for holder in holders(root) {
        let note = note_of(root, &holder.lock);
        let what = format!(
            "{} held by pid {} (group {}, {})",
            holder.lock, holder.pid, holder.pgid, holder.cmd
        );
        match classify(&holder, note.as_ref(), &by_id, now) {
            Verdict::Tracked { job } => {
                lines.push(format!("lease {what}: job {job}, tracked"));
                log(lines.last().unwrap());
            }
            Verdict::Orphan { job, pgid } => {
                let phase = by_id.get(&job).map(|j| format!("{:?}", j.phase));
                lines.push(format!(
                    "lease {what}: ORPHAN of job {job} (ledger: {}); stopping its process group {pgid}",
                    phase.unwrap_or_default()
                ));
                log(lines.last().unwrap());
                if !reaped.contains(&pgid) {
                    reap(pgid);
                    reaped.push(pgid);
                }
            }
            Verdict::Untracked { why } => {
                lines.push(format!(
                    "lease {what}: UNTRACKED ({why}); not signalled, inspect it (tasks admission)"
                ));
                log(lines.last().unwrap());
            }
        }
    }
    lines
}

/// Audit now (daemon start) and every few minutes, in its own thread.
pub fn start(state: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("lease-audit".into())
        .spawn(move || {
            loop {
                if let Err(e) = pass(&state) {
                    eprintln!("lease audit: {e}");
                }
                std::thread::sleep(EVERY);
            }
        });
}

fn pass(state: &Path) -> Result<()> {
    let root = crate::admission::root()?;
    let jobs = crate::Store::open(&state.join("tasks.sqlite"))?.jobs()?;
    audit(&root, &jobs, &mut |line| eprintln!("taskd: {line}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Execution;
    use crate::{Cost, JobSpec, Phase};

    fn job(id: i64, phase: Phase, finished: Option<i64>) -> Job {
        Job {
            id,
            spec: JobSpec {
                suite: crate::local_command::SUITE.into(),
                owner: "a".into(),
                source: "/".into(),
                revision: "live-workspace".into(),
                configuration: "{}".into(),
                cost: Cost::CpuSimulation,
            },
            phase,
            submitted_at: 0,
            execution: Some(Execution {
                approved_at: 0,
                started_at: Some(0),
                finished_at: finished,
                cancel_requested: false,
                worker: None,
                supervisor: None,
                snapshot: None,
                stages: vec![],
                build_artifact: None,
                android_trial: false,
                local_command: true,
                detail: String::new(),
                metrics: None,
                peer: None,
                remote_result: None,
            }),
        }
    }

    #[test]
    fn holders_are_tracked_reaped_only_as_orphans_or_named_untracked() {
        let holder = Holder {
            lock: "p2p-build.lock".into(),
            pid: 500,
            pgid: 400,
            cmd: "gradle".into(),
        };
        let note = NoteRef {
            job: Some(184),
            leader: 400,
            who: "codec@host:job-184".into(),
        };
        let now = 10_000;
        let jobs = |phase, finished| HashMap::from([(184, job(184, phase, finished))]);
        assert_eq!(
            classify(&holder, Some(&note), &jobs(Phase::Building, None), now),
            Verdict::Tracked { job: 184 }
        );
        assert_eq!(
            classify(
                &holder,
                Some(&note),
                &jobs(Phase::Interrupted, Some(now - 600)),
                now
            ),
            Verdict::Orphan {
                job: 184,
                pgid: 400
            }
        );
        // Just finished: its command may still be exiting.
        assert_eq!(
            classify(
                &holder,
                Some(&note),
                &jobs(Phase::Passed, Some(now - 5)),
                now
            ),
            Verdict::Tracked { job: 184 }
        );
        for (note, jobs) in [
            (None, jobs(Phase::Interrupted, Some(0))),
            (
                Some(NoteRef {
                    leader: 401,
                    ..note.clone()
                }),
                jobs(Phase::Interrupted, Some(0)),
            ),
            (Some(note.clone()), HashMap::new()),
            (
                Some(NoteRef {
                    job: None,
                    who: "legacy-python".into(),
                    ..note.clone()
                }),
                jobs(Phase::Interrupted, Some(0)),
            ),
        ] {
            assert!(
                matches!(
                    classify(&holder, note.as_ref(), &jobs, now),
                    Verdict::Untracked { .. }
                ),
                "{note:?}"
            );
        }
        assert_eq!(job_of("codec-tuning@kurukkan:job-184"), Some(184));
        assert_eq!(job_of("agent"), None);
    }
}
