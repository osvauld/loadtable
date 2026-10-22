//! Compile-boundary checkpoint: unsupported local runtime must not create state or jobs.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tasks-macos-boundaries-{}-{unique}",
            std::process::id(),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn runtime_commands_refuse_before_state_creation_or_child_execution() {
    let fixture = Fixture::new();
    let state = fixture.0.join("never-created-state");
    let marker = fixture.0.join("must-not-run");
    let marker = marker.to_str().unwrap();
    let commands: &[&[&str]] = &[
        &[],
        &["ui"],
        &["list"],
        &["admission"],
        &["status"],
        &["daemon"],
        &["worker"],
        &["start-worker"],
        &["mcp"],
        &["gc", "--apply"],
        &[
            "run",
            "--who",
            "macos-boundary-fixture",
            "--",
            "/usr/bin/touch",
            marker,
        ],
    ];
    for args in commands {
        let output = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .args(*args)
            .env("TASK_MANAGER_STATE_DIR", &state)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "args={args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("macOS local runtime is not enabled yet"),
            "{error}"
        );
        assert!(error.contains("no job was submitted or started"), "{error}");
        assert!(
            !state.exists(),
            "unsupported command mutated state: {args:?}"
        );
        assert!(!std::path::Path::new(marker).exists());
    }
}

#[test]
fn catalog_stays_available_without_daemon_or_state() {
    let fixture = Fixture::new();
    let state = fixture.0.join("never-created-state");
    let output = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .arg("catalog")
        .env("TASK_MANAGER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(output.status.success());
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!catalog.is_null());
    assert!(!state.exists());
}

#[test]
fn setup_stays_available_without_probing_or_starting_a_daemon() {
    let fixture = Fixture::new();
    let state = fixture.0.join("never-created-state");
    for args in [["setup", "--help"], ["setup", "--json"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .args(args)
            .env("TASK_MANAGER_STATE_DIR", &state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args[1] == "--json" {
            let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(!plan.is_null());
        }
        assert!(!state.exists());
    }
}

#[test]
fn native_pressure_and_unported_preexec_contracts_fail_closed() {
    let error = tinline_task_manager::admission::Pressure::read().unwrap_err();
    assert!(error.to_string().contains("admission fails closed"));
    assert!(error.to_string().contains("PSI is unavailable"));
    for result in [
        task_manager_platform::process::arm_parent_death_signal(),
        task_manager_platform::process::mark_inherited_fds_cloexec(),
    ] {
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOTSUP));
    }
}
