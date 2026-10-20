use super::*;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static ID: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tasks-gc-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Sets the mtime of `path` (a file or a directory) `days` days back.
fn age_days(path: &Path, days: u64) {
    let when = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    fs::File::open(path).unwrap().set_modified(when).unwrap();
}

fn age_tree(path: &Path, days: u64) {
    if fs::symlink_metadata(path).unwrap().is_dir() {
        for e in fs::read_dir(path).unwrap() {
            age_tree(&e.unwrap().path(), days);
        }
    }
    if fs::symlink_metadata(path).unwrap().file_type().is_symlink() {
        // File::open would follow the link: set the link's own times.
        let when = now() - (days * 24 * 3600) as i64;
        let times = [libc::timespec {
            tv_sec: when,
            tv_nsec: 0,
        }; 2];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(rc, 0);
    } else {
        age_days(path, days);
    }
}

fn limits() -> Limits {
    GcPolicy::default().limits().unwrap()
}

fn roots(t: &Temp) -> Roots {
    let roots = Roots::of_home(&t.0);
    fs::create_dir_all(&roots.offload).unwrap();
    fs::create_dir_all(&roots.outputs).unwrap();
    roots
}

fn copy(roots: &Roots, name: &str, days: u64) -> PathBuf {
    let dir = roots.offload.join(name);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    let identity = roots.offload.join(format!("{name}.identity"));
    fs::write(&identity, r#"{"host":"kurukkan","toplevel":"/home/abe/p"}"#).unwrap();
    age_tree(&dir, days);
    age_days(&identity, days);
    dir
}

fn output(roots: &Roots, key: &str, days: u64) -> PathBuf {
    let dir = roots.outputs.join(key);
    fs::create_dir_all(dir.join("target/debug")).unwrap();
    fs::write(dir.join("target/debug/big"), vec![1u8; 2 << 20]).unwrap();
    age_tree(&dir, days);
    dir
}

#[test]
fn an_idle_offload_copy_goes_with_its_identity_and_a_recent_one_stays() {
    let t = Temp::new();
    let roots = roots(&t);
    let old = copy(&roots, "aaaa", 10);
    let fresh = copy(&roots, "bbbb", 1);
    let plan = plan(&roots, limits(), &InUse::default(), now());
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].path, old);
    assert_eq!(
        plan.candidates[0].origin.as_deref(),
        Some("kurukkan:/home/abe/p")
    );
    assert!(apply(&plan, &roots).iter().all(|o| o.error.is_none()));
    assert!(!old.exists() && !roots.offload.join("aaaa.identity").exists());
    assert!(fresh.exists() && roots.offload.join("bbbb.identity").exists());
}

#[test]
fn a_live_job_or_a_working_process_keeps_a_copy() {
    let t = Temp::new();
    let roots = roots(&t);
    let busy = copy(&roots, "aaaa", 10);
    let shell = copy(&roots, "bbbb", 10);
    let using = InUse {
        sources: vec![busy.join("src")],
        cwds: vec![shell.clone()],
    };
    let plan = plan(&roots, limits(), &using, now());
    assert!(plan.candidates.is_empty());
    assert_eq!(plan.skipped.len(), 2);
}

#[test]
fn a_touched_file_inside_a_copy_makes_it_recent() {
    let t = Temp::new();
    let roots = roots(&t);
    let dir = copy(&roots, "aaaa", 10);
    fs::write(dir.join("src/new.rs"), "x").unwrap();
    assert!(
        plan(&roots, limits(), &InUse::default(), now())
            .candidates
            .is_empty()
    );
}

#[test]
fn an_identity_file_without_its_copy_is_cleared() {
    let t = Temp::new();
    let roots = roots(&t);
    let id = roots.offload.join("gone.identity");
    fs::write(&id, "{}").unwrap();
    age_days(&id, 10);
    let plan = plan(&roots, limits(), &InUse::default(), now());
    assert_eq!(plan.candidates.len(), 1);
    apply(&plan, &roots);
    assert!(!id.exists());
}

#[test]
fn an_output_cache_is_emptied_but_its_directories_stay() {
    let t = Temp::new();
    let roots = roots(&t);
    let dir = output(&roots, "k1", 30);
    let plan = plan(&roots, limits(), &InUse::default(), now());
    assert_eq!(plan.candidates.len(), 1);
    assert!(plan.bytes() >= 2 << 20);
    assert!(apply(&plan, &roots).iter().all(|o| o.error.is_none()));
    assert!(dir.join("target").is_dir());
    assert_eq!(fs::read_dir(dir.join("target")).unwrap().count(), 0);
    // Emptied, it is no longer worth listing.
    age_tree(&dir, 30);
    assert!(
        self::plan(&roots, limits(), &InUse::default(), now())
            .candidates
            .is_empty()
    );
}

#[test]
fn the_checkout_of_a_live_job_keeps_its_outputs() {
    let t = Temp::new();
    let roots = roots(&t);
    let root = Path::new("/home/abe/project");
    let dir = output(&roots, &output_key(root), 30);
    let using = InUse {
        sources: vec![root.join("crates/x")],
        cwds: vec![],
    };
    let plan = plan(&roots, limits(), &using, now());
    assert!(plan.candidates.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert!(dir.join("target/debug/big").exists());
}

#[test]
fn a_symlink_in_a_copy_is_removed_never_followed() {
    let t = Temp::new();
    let roots = roots(&t);
    let keep = t.0.join("keep");
    fs::create_dir(&keep).unwrap();
    fs::write(keep.join("file"), "data").unwrap();
    let dir = copy(&roots, "aaaa", 10);
    symlink(&keep, dir.join("link")).unwrap();
    age_tree(&dir, 10);
    let plan = plan(&roots, limits(), &InUse::default(), now());
    assert_eq!(plan.candidates.len(), 1);
    apply(&plan, &roots);
    assert!(!dir.exists());
    assert_eq!(fs::read_to_string(keep.join("file")).unwrap(), "data");
}

#[test]
fn a_plan_is_refused_outside_its_root() {
    let t = Temp::new();
    let roots = roots(&t);
    let outside = t.0.join("outside");
    fs::create_dir(&outside).unwrap();
    let plan = Plan {
        candidates: vec![Candidate {
            kind: Kind::OffloadCopy,
            path: outside.clone(),
            also: vec![],
            bytes: 0,
            idle_s: 0,
            origin: None,
        }],
        skipped: vec![],
    };
    let out = apply(&plan, &roots);
    assert!(out[0].error.is_some());
    assert!(outside.exists());
}

#[test]
fn limits_come_from_the_policy_and_reject_nonsense() {
    let set = GcPolicy {
        offload_idle_days: Some(1),
        output_idle_days: Some(30),
        ..GcPolicy::default()
    };
    assert_eq!(set.limits().unwrap().offload_idle_s, DAY);
    assert_eq!(set.limits().unwrap().output_idle_s, 30 * DAY);
    let bad = GcPolicy {
        offload_idle_days: Some(0),
        output_idle_days: None,
        ..GcPolicy::default()
    };
    assert!(bad.limits().unwrap_err().contains("offload_idle_days"));
    assert_eq!(limits().offload_idle_s, 3 * DAY);
}

#[test]
fn under_pressure_the_oldest_idle_caches_go_first_until_the_target_and_live_ones_stay() {
    let t = Temp::new();
    let roots = Roots::of_home(&t.0);
    fs::create_dir_all(&roots.outputs).unwrap();
    let names = ["old", "older", "new", "fresh"];
    for (name, days) in [("old", 9), ("older", 12), ("new", 1), ("fresh", 0)] {
        let dir = roots.outputs.join(name).join("debug");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("big"), vec![1u8; 2 << 20]).unwrap();
        age_tree(&roots.outputs.join(name), days);
    }
    // The disk starts at 10 free and gains 8 for every cache that has been emptied.
    let free = |_: &Path| {
        let emptied = names
            .iter()
            .filter(|n| !roots.outputs.join(n).join("debug/big").exists())
            .count() as u64;
        Some(10 + 8 * emptied)
    };
    let removed = relieve(&roots, &InUse::default(), now(), 20, 25, &free);
    let order: Vec<_> = removed
        .iter()
        .map(|r| r.path.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    // Oldest first, stopping once the target is met; 'fresh' (touched minutes ago) never goes.
    assert_eq!(order, ["older", "old"]);
    assert!(removed.iter().all(|r| r.error.is_none()));
    assert!(removed[0].free_before == 10 && removed[1].free_after == 26);
    assert!(roots.outputs.join("new/debug/big").exists());
    assert!(roots.outputs.join("fresh/debug/big").exists());
}

#[test]
fn nothing_is_removed_while_the_disk_is_above_its_reserve() {
    let t = Temp::new();
    let roots = Roots::of_home(&t.0);
    fs::create_dir_all(roots.outputs.join("a/debug")).unwrap();
    fs::write(roots.outputs.join("a/debug/big"), vec![1u8; 2 << 20]).unwrap();
    age_tree(&roots.outputs.join("a"), 30);
    let removed = relieve(&roots, &InUse::default(), now(), 20, 25, &|_| Some(100));
    assert!(removed.is_empty());
    assert!(roots.outputs.join("a/debug/big").exists());
}

#[test]
fn a_live_checkout_is_not_taken_under_pressure() {
    let t = Temp::new();
    let roots = Roots::of_home(&t.0);
    let checkout = PathBuf::from("/work/live-checkout");
    let key = output_key(&checkout);
    fs::create_dir_all(roots.outputs.join(&key).join("debug")).unwrap();
    fs::write(
        roots.outputs.join(&key).join("debug/big"),
        vec![1u8; 2 << 20],
    )
    .unwrap();
    age_tree(&roots.outputs.join(&key), 30);
    let using = InUse {
        sources: vec![checkout],
        cwds: vec![],
    };
    let removed = relieve(&roots, &using, now(), 20, 25, &|_| Some(1));
    assert!(removed.is_empty());
}
