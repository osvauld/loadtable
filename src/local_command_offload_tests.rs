//! Offload resource: command allowlist, stored representation (ledger round-trip)
//! and class-aware claiming. Synthetic owners, argv and temporary ledgers only.
use super::*;

fn recipe(argv: &[&str], resource: Resource) -> Recipe {
    Recipe {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        resource,
    }
}

#[test]
fn offload_admits_only_offload_clients() {
    for argv in [
        &[
            "tasks-offload",
            "--worker",
            "u@h",
            "--",
            "cargo",
            "test",
            "-j",
            "8",
        ][..],
        &[
            "/home/u/.local/bin/tasks-offload",
            "--who",
            "x",
            "--",
            "true",
        ],
        &["/abs/scripts/tasks-offload", "--dry-run", "--", "true"],
        &["offload-portable-desktop.sh", "--who", "x"],
        &[
            "/abs/scripts/offload-portable-desktop.sh",
            "--check-only",
            "/b",
        ],
        &[
            "env",
            "TASKS_OFFLOAD_SSH=ssh",
            "tasks-offload",
            "--",
            "true",
        ],
    ] {
        let r = recipe(argv, Resource::Offload);
        r.validate_with_max(2)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        assert_eq!(r.stages(), ["run"]);
        assert_eq!(r.argv_for("run").unwrap(), r.argv);
        assert!(r.argv_for("build").is_err());
    }
    for argv in [
        &["cargo", "test", "-j", "2"][..],
        &["sh", "-c", "tasks-offload -- true"],
        &["python3", "scripts/tasks_offload.py", "--", "true"],
        &["tasks-offload-local", "--", "true"],
        &["/abs/tasks-offload/cargo", "build"],
        &["env", "-i", "tasks-offload", "--", "true"],
        &["nice", "tasks-offload", "--", "true"],
    ] {
        let error = recipe(argv, Resource::Offload)
            .validate_with_max(2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--offload"), "{argv:?}: {error}");
        assert!(error.contains("tasks-offload"), "{argv:?}: {error}");
        assert!(error.contains("--run"), "{argv:?}: {error}");
    }
}

#[test]
fn run_slot_offloads_stay_valid() {
    let r = recipe(
        &["tasks-offload", "--", "cargo", "test", "-j", "8"],
        Resource::Run,
    );
    r.validate_with_max(2).unwrap();
    assert_eq!(r.stages(), ["run"]);
}

#[test]
fn offload_is_stored_as_an_additive_resource_string() {
    let r = recipe(&["tasks-offload", "--", "true"], Resource::Offload);
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(
        json,
        r#"{"argv":["tasks-offload","--","true"],"resource":"offload"}"#
    );
    let back: Recipe = serde_json::from_str(&json).unwrap();
    assert_eq!(back.resource, Resource::Offload);
    // Earlier stored shapes are unchanged.
    for (text, resource) in [
        (r#"{"argv":["x"],"resource":"run"}"#, Resource::Run),
        (r#"{"argv":["x"],"resource":"compile"}"#, Resource::Compile),
        (
            r#"{"argv":["x"],"resource":{"device":"S"}}"#,
            Resource::Device("S".into()),
        ),
    ] {
        let parsed: Recipe = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.resource, resource);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), text);
    }
}

struct Ledger(PathBuf);
impl Ledger {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "task-manager-offload-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Ledger {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn submit(store: &mut crate::Store, dir: &Path, r: &Recipe) -> i64 {
    let spec = JobSpec {
        suite: SUITE.into(),
        configuration: serde_json::to_string(r).unwrap(),
        cost: crate::Cost::CpuSimulation,
        owner: "offload-test".into(),
        source: dir.to_str().unwrap().into(),
        revision: "live-workspace".into(),
    };
    let id = store.submit(&spec).unwrap().id;
    store.approve_command(id).unwrap();
    id
}

#[test]
fn ledger_round_trip_and_class_aware_claims() {
    let ledger = Ledger::new("claims");
    let mut store = crate::Store::open(&ledger.0.join("tasks.sqlite")).unwrap();
    let compile = submit(
        &mut store,
        &ledger.0,
        &recipe(&["/bin/true"], Resource::Compile),
    );
    let offload = submit(
        &mut store,
        &ledger.0,
        &recipe(&["tasks-offload", "--", "true"], Resource::Offload),
    );
    let run = submit(
        &mut store,
        &ledger.0,
        &recipe(&["/bin/true"], Resource::Run),
    );
    let job = store.job(offload).unwrap();
    assert!(is_offload(&job.spec));
    assert_eq!(
        Recipe::from_spec(&job.spec).unwrap().resource,
        Resource::Offload
    );
    assert!(!is_offload(&store.job(run).unwrap().spec));
    let me = crate::worker::identity(std::process::id()).unwrap();
    // Only offload capacity left: the offload is claimed past the older compile job.
    let claimed = store.claim_class(me.clone(), false, true).unwrap().unwrap();
    assert_eq!(claimed.id, offload);
    assert!(
        store
            .claim_class(me.clone(), false, true)
            .unwrap()
            .is_none()
    );
    // General capacity keeps FIFO among the other jobs.
    assert_eq!(
        store
            .claim_class(me.clone(), true, false)
            .unwrap()
            .unwrap()
            .id,
        compile
    );
    assert_eq!(store.claim(me.clone()).unwrap().unwrap().id, run);
    assert!(store.claim(me).unwrap().is_none());
}
