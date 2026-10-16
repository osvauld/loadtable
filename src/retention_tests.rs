use super::*;
use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::symlink};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tasks-retention-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
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
// Parallel tests fork Git/Python. CLOEXEC descriptors can retain flock briefly
// between fork and exec even after this thread drops its copy. Only idle-fixture
// expectations retry; production planning remains conservatively active.
fn eventually<T>(mut probe: impl FnMut() -> T, settled: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let value = probe();
        if settled(&value) || Instant::now() >= deadline {
            return value;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn inspect(path: &Path) -> TargetPlan {
    eventually(
        || super::inspect(path),
        |plan| plan.status != Status::ProtectedActive,
    )
}
fn managed(path: &Path) {
    let lease = eventually(
        || TargetLease::try_acquire(path).unwrap(),
        |lease| lease.is_some(),
    )
    .unwrap();
    let usage = begin_use(&lease, path).unwrap().unwrap();
    drop(usage);
    drop(lease);
}
fn fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn missing_and_legacy_plans_do_not_create_or_adopt_anything() {
    let root = Temp::new();
    let missing = root.0.join("missing");
    assert_eq!(inspect(&missing).status, Status::Missing);
    assert!(!missing.exists());
    fs::write(root.0.join("user-artifact"), "preserve").unwrap();
    assert_eq!(inspect(&root.0).status, Status::ProtectedUnknown);
    assert!(!root.0.join(LOCK).exists());
    let lease = TargetLease::try_acquire(&root.0).unwrap().unwrap();
    assert!(begin_use(&lease, &root.0).unwrap().is_none());
    drop(lease);
    assert_eq!(inspect(&root.0).status, Status::ProtectedUnknown);
    assert!(!root.0.join(OWNER).exists());
    assert_eq!(
        fs::read_to_string(root.0.join("user-artifact")).unwrap(),
        "preserve"
    );
}

#[test]
fn clean_use_records_provenance_and_protects_active_and_recent_targets() {
    let target = Temp::new();
    let lease = TargetLease::try_acquire(&target.0).unwrap().unwrap();
    let usage = begin_use(&lease, &target.0).unwrap().unwrap();
    let bytes = fs::read(target.0.join(USAGE)).unwrap();
    let active = super::inspect(&target.0);
    assert_eq!(active.status, Status::ProtectedActive);
    assert!(active.inventory.is_none());
    assert_eq!(bytes, fs::read(target.0.join(USAGE)).unwrap());
    drop(usage);
    assert!(
        read_json::<Usage>(&target.0.join(USAGE))
            .unwrap()
            .unwrap()
            .finished_at
            .is_some()
    );
    assert_eq!(super::inspect(&target.0).status, Status::ProtectedActive);
    drop(lease);
    let recent = inspect(&target.0);
    assert_eq!(recent.status, Status::RetainRecent);
    assert!(recent.ownership_verified);
    assert!(recent.inventory.unwrap().complete);
}

#[test]
fn old_clean_target_is_only_a_review_candidate_and_plan_preserves_lock_inode() {
    let target = Temp::new();
    managed(&target.0);
    fs::write(target.0.join("artifact"), vec![1u8; 8192]).unwrap();
    let metadata = fs::metadata(target.0.join(LOCK)).unwrap();
    let owner = fs::read(target.0.join(OWNER)).unwrap();
    let usage = fs::read(target.0.join(USAGE)).unwrap();
    let plan = eventually(
        || {
            let mut plan = TargetPlan::new(&target.0);
            inspect_inner(
                &target.0,
                &mut plan,
                now() + IDLE_SECONDS as i64 + 5,
                IDLE_SECONDS,
            )
            .unwrap();
            plan
        },
        |plan| plan.status != Status::ProtectedActive,
    );
    assert_eq!(plan.status, Status::ReviewCandidate);
    let inventory = plan.inventory.unwrap();
    assert!(inventory.disposable_allocated_bytes >= 8192);
    assert!(inventory.disposable_allocated_bytes < inventory.allocated_bytes);
    let after = fs::metadata(target.0.join(LOCK)).unwrap();
    assert_eq!((metadata.dev(), metadata.ino()), (after.dev(), after.ino()));
    assert_eq!(owner, fs::read(target.0.join(OWNER)).unwrap());
    assert_eq!(usage, fs::read(target.0.join(USAGE)).unwrap());
    assert_eq!(
        fs::read(target.0.join("artifact")).unwrap(),
        vec![1u8; 8192]
    );
    assert_eq!(inspect(&target.0).status, Status::RetainRecent);
}

#[test]
fn unfinished_use_is_protected_until_a_clean_managed_use() {
    let target = Temp::new();
    let lease = TargetLease::try_acquire(&target.0).unwrap().unwrap();
    let usage = begin_use(&lease, &target.0).unwrap().unwrap();
    std::mem::forget(usage); // Model worker death: no end-of-use metadata write.
    drop(lease);
    let plan = inspect(&target.0);
    assert_eq!(plan.status, Status::ProtectedReview);
    assert!(plan.reason.contains("unfinished usage"));
    managed(&target.0);
    assert_eq!(inspect(&target.0).status, Status::RetainRecent);
}

#[test]
fn ownership_markers_cannot_be_copied_to_claim_another_populated_target() {
    let original = Temp::new();
    managed(&original.0);
    let other = Temp::new();
    fs::write(other.0.join("user-data"), "preserve").unwrap();
    fs::copy(original.0.join(OWNER), other.0.join(OWNER)).unwrap();
    let lease = TargetLease::try_acquire(&other.0).unwrap().unwrap();
    assert!(begin_use(&lease, &other.0).is_err());
    drop(lease);
    let plan = inspect(&other.0);
    assert_eq!(plan.status, Status::ProtectedUnknown);
    assert!(!plan.ownership_verified);
    assert_eq!(
        fs::read_to_string(other.0.join("user-data")).unwrap(),
        "preserve"
    );
}

#[test]
fn replaced_lease_inode_invalidates_provenance_without_adopting_the_target() {
    let target = Temp::new();
    managed(&target.0);
    fs::write(target.0.join("artifact"), "preserve").unwrap();
    let old_inode = File::open(target.0.join(LOCK)).unwrap();
    // Deliberate corruption of this idle, test-owned fixture only.
    fs::remove_file(target.0.join(LOCK)).unwrap();
    let replacement = TargetLease::try_acquire(&target.0).unwrap().unwrap();
    assert_ne!(
        old_inode.metadata().unwrap().ino(),
        replacement.file().metadata().unwrap().ino()
    );
    assert!(begin_use(&replacement, &target.0).is_err());
    drop(replacement);
    assert_eq!(inspect(&target.0).status, Status::ProtectedUnknown);
    assert_eq!(
        fs::read_to_string(target.0.join("artifact")).unwrap(),
        "preserve"
    );
}

#[test]
fn inventory_deduplicates_internal_hardlinks_and_never_follows_symlinks() {
    let target = Temp::new();
    let outside = Temp::new();
    fs::write(target.0.join("one"), vec![1u8; 8192]).unwrap();
    fs::hard_link(target.0.join("one"), target.0.join("two")).unwrap();
    let first = inventory(&target.0).unwrap();
    assert!(first.complete);
    assert_eq!(first.external_hardlinks, 0);
    assert_eq!(
        first.allocated_bytes,
        fs::metadata(target.0.join("one")).unwrap().blocks() * 512
    );
    assert!(require_budget_for(&first, first.allocated_bytes + 1).is_ok());
    assert!(require_budget_for(&first, first.allocated_bytes).is_err());
    fs::hard_link(target.0.join("one"), outside.0.join("external-link")).unwrap();
    fs::write(outside.0.join("large"), vec![2u8; 128 * 1024]).unwrap();
    symlink(&outside.0, target.0.join("alias")).unwrap();
    let linked = inventory(&target.0).unwrap();
    assert!(linked.complete);
    assert_eq!(linked.symlinks, 1);
    assert_eq!(linked.external_hardlinks, 1);
    assert!(
        linked.allocated_bytes < 128 * 1024,
        "must not walk alias into outside data"
    );
}

#[test]
fn inventory_limits_and_special_files_fail_budget_closed() {
    let target = Temp::new();
    fs::create_dir(target.0.join("nested")).unwrap();
    fs::write(target.0.join("nested/file"), "data").unwrap();
    for inventory in [
        inventory_with_limits(&target.0, 0, 64, MAX_SCAN_TIME).unwrap(),
        inventory_with_limits(&target.0, 50, 0, MAX_SCAN_TIME).unwrap(),
        inventory_with_limits(&target.0, 50, 64, Duration::ZERO).unwrap(),
    ] {
        assert!(!inventory.complete);
        assert!(require_budget_for(&inventory, u64::MAX).is_err());
    }
    fifo(&target.0.join("special"));
    let report = inventory(&target.0).unwrap();
    assert!(!report.complete);
    assert!(require_budget_for(&report, u64::MAX).is_err());
}

#[test]
fn malformed_and_symlinked_control_files_are_protected_without_blocking() {
    let target = Temp::new();
    managed(&target.0);
    fs::write(target.0.join(USAGE), "invalid JSON").unwrap();
    assert_eq!(inspect(&target.0).status, Status::ProtectedReview);
    fs::remove_file(target.0.join(USAGE)).unwrap();
    fifo(&target.0.join(USAGE));
    assert_eq!(inspect(&target.0).status, Status::ProtectedReview);
    fs::remove_file(target.0.join(USAGE)).unwrap();
    let outside = Temp::new();
    fs::write(outside.0.join("private"), "preserve").unwrap();
    symlink(outside.0.join("private"), target.0.join(USAGE)).unwrap();
    assert_eq!(inspect(&target.0).status, Status::ProtectedReview);
    assert_eq!(
        fs::read_to_string(outside.0.join("private")).unwrap(),
        "preserve"
    );
    symlink(&target.0, outside.0.join("target-alias")).unwrap();
    assert_eq!(
        inspect(&outside.0.join("target-alias")).status,
        Status::ProtectedReview
    );
}

#[test]
fn planning_lists_legacy_roots_without_recursive_scans_or_new_files() {
    let root = Temp::new();
    let managed_path = root.0.join("tasks/targets/example");
    fs::create_dir_all(&managed_path).unwrap();
    managed(&managed_path);
    let legacy = root.0.join("old-agent-target");
    fs::create_dir(&legacy).unwrap();
    fifo(&legacy.join("not-read"));
    let alias = root.0.join("tasks/targets/alias");
    symlink(&legacy, &alias).unwrap();
    let plan = eventually(
        || plan_at(&root.0, &managed_path).unwrap(),
        |plan| {
            plan.targets
                .iter()
                .all(|target| target.status != Status::ProtectedActive)
        },
    );
    assert!(plan.dry_run && !plan.deletion_enabled && plan.enumeration_complete);
    assert!(plan.legacy_roots_owner_unknown.contains(&legacy));
    assert!(
        plan.targets
            .iter()
            .any(|t| t.path == alias && t.status == Status::ProtectedReview)
    );
    assert!(
        plan.targets
            .iter()
            .any(|t| t.path == managed_path && t.status == Status::RetainRecent)
    );
    assert!(!legacy.join(LOCK).exists());
    assert!(!root.0.join("tasks/targets/self-tests").exists());
}
