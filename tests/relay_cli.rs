//! Local config-only CLI tests. No endpoint is bound and no relay is contacted.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};
use task_manager_peer::{PeerConfig, Transport, config::Approve};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tasks-relay-cli-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> PathBuf {
        self.0.join("config/peer.toml")
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tasks"))
            .env("HOME", &self.0)
            .env("TASK_MANAGER_STATE_DIR", self.0.join("state"))
            .env("TASKS_PEER_CONFIG", self.config())
            .args(args)
            .output()
            .unwrap()
    }
    fn load(&self) -> PeerConfig {
        PeerConfig::parse(
            &std::fs::read_to_string(self.config()).unwrap(),
            &self.config(),
            Some(&self.0),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn passed(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn relay_pair_config_is_opt_in_manual_and_does_not_start_a_daemon() {
    let f = Fixture::new();
    passed(&f.run(&["peer", "init", "--relay", "https://relay.example.invalid/"]));
    let c = f.load();
    assert_eq!(c.transport, Transport::Relay);
    assert!(c.bind.ip().is_unspecified());
    let key = c.secret_key().unwrap().public();
    let id = task_manager_peer::iroh::SecretKey::from_bytes(&[9; 32])
        .public()
        .to_string();
    passed(&f.run(&["peer", "add", "linux", &id, "-", "--role", "host"]));
    let c = f.load();
    assert_eq!(c.peer("linux").unwrap().approve, Approve::Manual);
    assert!(c.can_dial(c.peer("linux").unwrap()));
    assert!(!f.0.join("state/taskd.sock").exists());
    assert!(!f.0.join("state/peer.sock").exists());
    let before = std::fs::read(f.config()).unwrap();
    passed(&f.run(&["peer", "init", "--relay", "https://relay.example.invalid/"]));
    assert!(
        !f.run(&[
            "peer",
            "init",
            "--relay",
            "https://different.example.invalid/"
        ])
        .status
        .success()
    );
    assert_eq!(std::fs::read(f.config()).unwrap(), before);
    assert_eq!(f.load().secret_key().unwrap().public(), key);
}

#[test]
fn invalid_relay_does_not_create_identity_and_dash_cannot_enable_lan_relay() {
    let f = Fixture::new();
    assert!(
        !f.run(&["peer", "init", "--relay", "http://relay.example.invalid/"])
            .status
            .success()
    );
    assert!(!f.config().exists());
    assert!(!f.config().parent().unwrap().join("peer.key").exists());
    passed(&f.run(&["peer", "init", "--bind", "127.0.0.1:7842"]));
    let before = std::fs::read(f.config()).unwrap();
    let id = task_manager_peer::iroh::SecretKey::from_bytes(&[10; 32])
        .public()
        .to_string();
    assert!(!f.run(&["peer", "add", "mac", &id, "-"]).status.success());
    assert_eq!(std::fs::read(f.config()).unwrap(), before);
    assert_eq!(f.load().transport, Transport::Lan);
    assert!(!f.0.join("state/taskd.sock").exists());
}
