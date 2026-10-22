//! The client against a real in-process daemon over a real socket: submit, list, get, cancel,
//! status, drain and the error paths. Jobs are queued while execution is stopped, so no job
//! process is started by this test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use tasks_cli::runtime::{NoSignals, SleepPacer};
use tasks_cli::{Deps, real_client, run_cli};
use tasks_daemon::config::{ConfigArgs, DaemonConfig};
use tasks_daemon::{Daemon, DaemonOptions};

struct Rig {
    dir: PathBuf,
}

impl Rig {
    fn start() -> Rig {
        let dir = std::env::temp_dir().join(format!("tcli-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let args = ConfigArgs {
            state_dir: Some(dir.join("state")),
            config_dir: Some(dir.join("config")),
            lock_dir: Some(dir.join("slots")),
            cache_dir: Some(dir.join("cache")),
            machine: Some("testbox".into()),
        };
        let cfg = DaemonConfig::resolve(&args, &|k| {
            (k == "HOME").then(|| "/nonexistent".to_string())
        })
        .unwrap();
        std::fs::create_dir_all(&cfg.state_dir).unwrap();
        // stopped before the daemon starts: nothing is ever launched
        std::fs::write(cfg.intent_path(), "stopped\n").unwrap();
        let daemon = Daemon::start(DaemonOptions::new(cfg)).unwrap();
        std::thread::spawn(move || {
            let _ = daemon.run();
        });
        Rig { dir }
    }

    fn tasks(&self, args: &[&str]) -> (i32, String, String) {
        let mut full = vec![
            "tasks2".to_string(),
            "--state-dir".into(),
            self.dir.join("state").display().to_string(),
        ];
        full.extend(args.iter().map(|s| s.to_string()));
        let env = |k: &str| match k {
            "HOME" => Some("/nonexistent".to_string()),
            "TASKS_WHO" => Some("e2e".to_string()),
            _ => None,
        };
        let now = || 0i64;
        let read = |p: &Path| std::fs::read_to_string(p);
        let client_for = |cli: &tasks_cli::args::Cli| real_client(cli, &env);
        let deps = Deps {
            env: &env,
            cwd: self.dir.clone(),
            now_ms: &now,
            pacer: &SleepPacer,
            signals: &NoSignals,
            read_input: &read,
            policy_fs: &tasks_policy::RealFs,
            history: None,
            client_for: &client_for,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_cli(&full, &mut out, &mut err, &deps);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.tasks(&["status"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_client_talks_to_a_real_daemon() {
    let rig = Rig::start();

    // status and admission: the daemon's directories, not this shell's
    let (code, out, err) = rig.tasks(&["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("queue: STOPPED"), "{out}");
    let (code, out, _) = rig.tasks(&["admission"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("lock dir: {}", rig.dir.join("slots").display())),
        "{out}"
    );
    assert!(out.contains("policy values:"));
    let (_, json, _) = rig.tasks(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["daemon"]["machine"], "testbox");

    // enqueue: the id on stdout, the submit line on stderr
    let (code, out, err) = rig.tasks(&["enqueue", "--", "echo", "hello world"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.starts_with("tasks: submitted job "), "{err}");
    let id: i64 = out.trim().parse().unwrap();

    // list and get
    let (code, out, _) = rig.tasks(&["list", "--active"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("execution is stopped") && out.contains("echo 'hello world'"),
        "{out}"
    );
    let (_, out, _) = rig.tasks(&["get", &id.to_string()]);
    assert!(
        out.contains(&format!("job {id}: ")) && out.contains("execution is stopped"),
        "{out}"
    );
    let (code, _, err) = rig.tasks(&["get", "99999"]);
    assert_eq!(code, 4, "{err}");

    // refusals reach the user with exit 2
    let (code, _, err) = rig.tasks(&["enqueue", "--who", "bad owner", "--", "x"]);
    assert_eq!(code, 2, "{err}");

    // only the owner may cancel
    let (code, _, err) = rig.tasks(&["cancel", &id.to_string(), "--who", "someone-else"]);
    assert_eq!(code, 2, "{err}");
    let (code, out, _) = rig.tasks(&["cancel", &id.to_string()]);
    assert_eq!(code, 0);
    assert!(out.contains("cancelled before it started"), "{out}");
    let (_, out, _) = rig.tasks(&["list", "--limit", "5"]);
    assert!(out.contains("cancelled"), "{out}");

    // drain and resume
    assert!(rig.tasks(&["drain"]).1.contains("draining"));
    assert!(rig.tasks(&["status"]).1.contains("queue: DRAINING"));
    assert!(rig.tasks(&["resume"]).1.contains("resumed"));

    // a plan
    let plan = rig.dir.join("plan.json");
    std::fs::write(
        &plan,
        r#"{"steps":[{"name":"a","argv":["true"]},{"name":"b","argv":["true"],"after":["a"]}]}"#,
    )
    .unwrap();
    let (code, out, err) = rig.tasks(&["steps", plan.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("a -> job") && out.contains("b -> job"),
        "{out}"
    );

    // logs of a job that never started: a named error, not a crash
    let (code, _, _) = rig.tasks(&["logs", &id.to_string(), "--tail", "5"]);
    assert!(code == 0 || code == 4);
    let (code, out, _) = rig.tasks(&["log-search", "zzz-not-there"]);
    assert_eq!(code, 0);
    assert!(out.contains("(no matches)"), "{out}");
}
