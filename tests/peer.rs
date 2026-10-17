#![cfg(target_os = "linux")]
//! Daemon-to-daemon forwarding over iroh, end to end and hermetic: two REAL `tasks daemon`
//! processes (A forwards, B runs) with their own HOME, state, socket, admission lock directory
//! and peer config, two iroh endpoints on 127.0.0.1 (no relay, no discovery), short link
//! timeouts. Jobs run through each daemon's real worker and admission (isolated lock dirs; the
//! fixtures relax only the memory/disk reserves and the run-slot CPU gate, within the limits the
//! environment allows; memory/IO PSI gates are unchanged, so a heavily loaded host makes these
//! tests wait, not fail early).
use std::io::Read;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::peer::net;
use tinline_task_manager::{Action, Job, Phase, request};

const BIN: &str = env!("CARGO_BIN_EXE_tasks");
const WAIT: Duration = Duration::from_secs(180);

struct Node {
    name: &'static str,
    root: PathBuf,
    port: u16,
    id: String,
    daemon: Option<Child>,
    extra_env: Vec<(String, String)>,
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Node {
    fn new(test: &str, name: &'static str) -> Self {
        let root =
            std::env::temp_dir().join(format!("tasks-peer-{test}-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home/.config/tasks", "state", "locks"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut node = Self {
            name,
            root,
            port: free_port(),
            id: String::new(),
            daemon: None,
            extra_env: vec![],
        };
        let out = node.cli(
            &[
                "peer",
                "init",
                "--bind",
                &format!("127.0.0.1:{}", node.port),
            ],
            &node.root.clone(),
        );
        assert!(out.status.success(), "{}", stderr(&out));
        node.id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        assert_eq!(node.id.len(), 64, "endpoint id: {:?}", node.id);
        // Short link timeouts so loss/re-attach is observable quickly.
        node.append_config(
            "reattach_timeout_s = 90\nconnect_timeout_s = 3\nheartbeat_s = 0.5\nlink_stall_s = 2\n",
            true,
        );
        node
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn config(&self) -> PathBuf {
        self.home().join(".config/tasks/peer.toml")
    }
    fn socket(&self) -> PathBuf {
        self.state().join("taskd.sock")
    }
    fn checkouts(&self) -> PathBuf {
        self.home().join("offload/checkouts")
    }
    /// Top-level keys must precede the first `[[peer]]` table.
    fn append_config(&self, text: &str, top_level: bool) {
        let mut current = std::fs::read_to_string(self.config()).unwrap();
        if top_level {
            let at = current.find("# [[peer]]").unwrap_or(current.len());
            current.insert_str(at, text);
        } else {
            current.push_str(text);
        }
        std::fs::write(self.config(), current).unwrap();
    }
    fn allow(&self, other: &Node, role: &str) {
        self.append_config(
            &format!(
                "\n[[peer]]\nname = \"{}\"\nendpoint_id = \"{}\"\naddr = \"127.0.0.1:{}\"\nrole = \"{role}\"\n",
                other.name, other.id, other.port
            ),
            false,
        );
    }
    fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("HOME".into(), self.home().display().to_string()),
            (
                "TASK_MANAGER_STATE_DIR".into(),
                self.state().display().to_string(),
            ),
            (
                "TASKS_LOCK_DIR".into(),
                self.root.join("locks").display().to_string(),
            ),
            (
                "TASKS_PEER_CONFIG".into(),
                self.config().display().to_string(),
            ),
            ("TASKS_MIN_AVAILABLE_MEMORY_MIB".into(), "1024".into()),
            ("TASKS_MIN_CACHE_FREE_GIB".into(), "1".into()),
            ("TASKS_MIN_STATE_FREE_MIB".into(), "64".into()),
            ("TASKS_RUN_CPU_SOME_MAX_PCT".into(), "100".into()),
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ];
        env.extend(self.extra_env.iter().cloned());
        env
    }
    fn command(&self, args: &[&str], cwd: &Path) -> Command {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env())
            .stdin(Stdio::null());
        // This test binary runs inside an admitted job and holds its slot locks; fixture
        // daemons/workers must not inherit them (they would keep the shared slot if leaked).
        // SAFETY: one async-signal-safe syscall between fork and exec.
        unsafe {
            c.pre_exec(|| {
                libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                );
                Ok(())
            });
        }
        c
    }
    /// Every process of this fixture: daemons (also ones a client autostarted), workers and
    /// stage supervisors carry its state dir in their environment. Admitted commands die with
    /// their supervisor (their watchdog kills the job's own group).
    fn reap(&self) {
        let needle = format!("TASK_MANAGER_STATE_DIR={}", self.state().display());
        for _ in 0..3 {
            let mut found = false;
            for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                let Ok(env) = std::fs::read(entry.path().join("environ")) else {
                    continue;
                };
                if env.split(|b| *b == 0).any(|v| v == needle.as_bytes()) {
                    found = true;
                    // SAFETY: plain kill(2) of a process of this fixture.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
            if !found {
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    fn cli(&self, args: &[&str], cwd: &Path) -> Output {
        let mut child = self
            .command(args, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        finish(&mut child, WAIT)
    }
    fn start(&mut self) {
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(self.state().join("fixture-daemon.log"))
            .unwrap();
        let child = self
            .command(&["daemon"], &self.root.clone())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(r) = tinline_task_manager::peer_link::request_peers(
                &self.socket(),
                Duration::from_secs(5),
            ) && (r.enabled || r.error.is_some())
            {
                assert!(
                    r.enabled,
                    "{} peer runtime: {r:?}\n{}",
                    self.name,
                    self.daemon_log()
                );
                return;
            }
            assert!(
                Instant::now() < end,
                "{} daemon did not start\n{}",
                self.name,
                self.daemon_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn kill(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
    }
    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.state().join("fixture-daemon.log")).unwrap_or_default()
    }
    fn jobs(&self) -> Vec<Job> {
        request(&self.socket(), Action::List).unwrap().jobs
    }
    fn job(&self, id: i64) -> Job {
        self.jobs().into_iter().find(|j| j.id == id).unwrap()
    }
    fn job_log(&self, id: i64) -> String {
        std::fs::read_to_string(self.state().join(format!("jobs/{id}/job.log"))).unwrap_or_default()
    }
    fn link(&self, id: i64) -> serde_json::Value {
        std::fs::read(self.state().join(format!("peer/links/{id}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // Cancel anything still live so detached workers exit, then stop the daemon.
        if self.daemon.is_some()
            && let Ok(r) = request(&self.socket(), Action::List)
        {
            for j in r.jobs.iter().filter(|j| !j.phase.terminal()) {
                let _ = request(&self.socket(), Action::Cancel(j.id));
            }
            // Give the workers time to stop the cancelled jobs' process groups.
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end
                && request(&self.socket(), Action::List)
                    .is_ok_and(|r| r.jobs.iter().any(|j| !j.phase.terminal()))
            {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.kill();
        self.reap();
        if std::env::var_os("TASKS_PEER_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn finish(child: &mut Child, limit: Duration) -> Output {
    let end = Instant::now() + limit;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut v = vec![];
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut v);
        }
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = vec![];
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_end(&mut v);
        }
        v
    });
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > end {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Output {
        status,
        stdout: t_out.join().unwrap(),
        stderr: t_err.join().unwrap(),
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Everything a failed wait needs: both ledgers, job logs, link records, daemon logs.
fn diagnose(nodes: &[&Node]) -> String {
    let mut out = String::new();
    for n in nodes {
        out.push_str(&format!("===== {} =====\n", n.name));
        if let Ok(r) = request(&n.socket(), Action::List) {
            for j in &r.jobs {
                out.push_str(&format!(
                    "--- job {} {:?} owner={} cwd={}\n{}\nlink: {}\n",
                    j.id,
                    j.phase,
                    j.spec.owner,
                    j.spec.source,
                    n.job_log(j.id),
                    n.link(j.id)
                ));
            }
        }
        out.push_str(&format!("--- daemon log\n{}\n", n.daemon_log()));
        out.push_str(&format!(
            "--- worker log\n{}\n",
            std::fs::read_to_string(n.state().join("worker.log")).unwrap_or_default()
        ));
    }
    out
}

fn until<T>(what: &str, nodes: &[&Node], limit: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= end {
            panic!("timed out waiting for {what}\n{}", diagnose(nodes));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A forwards to B (B is A's worker; A is B's host with auto approval).
fn pair(test: &str) -> (Node, Node) {
    let mut a = Node::new(test, "alpha");
    let mut b = Node::new(test, "beta");
    a.allow(&b, "worker");
    b.allow(&a, "host");
    b.start();
    a.start();
    (a, b)
}

/// A git checkout (no commit needed: untracked, not-ignored files are selected).
fn repo(node: &Node) -> PathBuf {
    let repo = node.root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .env("HOME", node.home())
        .status()
        .unwrap();
    assert!(git.success());
    std::fs::write(repo.join("src/a.txt"), "alpha\n").unwrap();
    std::fs::write(repo.join("src/b.txt"), "bravo\n").unwrap();
    std::fs::write(repo.join("data.txt"), "same\n").unwrap();
    std::fs::write(repo.join("copy.txt"), "same\n").unwrap();
    // Path rules: a top-level output dir and an env file are never sent; .gitignore'd neither.
    std::fs::create_dir_all(repo.join("target")).unwrap();
    std::fs::write(repo.join("target/out.bin"), "built\n").unwrap();
    std::fs::write(repo.join(".env.local"), "TOKEN=synthetic\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(repo.join("ignored.txt"), "local only\n").unwrap();
    repo
}

fn forward(a: &Node, cwd: &Path, extra: &[&str], command: &str) -> Output {
    let mut args = vec!["run", "--who", "fixture-agent", "--peer", "beta"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "/bin/sh", "-c", command]);
    a.cli(&args, cwd)
}

fn only_job(node: &Node) -> Job {
    let jobs = node.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    jobs.into_iter().next().unwrap()
}

fn exit_of(job: &Job) -> Option<i32> {
    job.execution.as_ref()?.stages.last()?.exit_code
}

fn note_count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

// ---------------------------------------------------------------------------------------------

#[test]
fn status_stream_reaches_an_allowlisted_peer_and_refuses_an_unknown_one() {
    let (a, b) = pair("status");
    // A's daemon subscribes to B's status stream (role worker may be watched by its host).
    let mut watch = a
        .command(&["peer", "status-watch", "beta"], &a.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = std::io::BufReader::new(watch.stdout.take().unwrap());
    let mut line = String::new();
    use std::io::BufRead;
    reader.read_line(&mut line).unwrap();
    let _ = watch.kill();
    let _ = watch.wait();
    let snap: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(snap["version"], 1, "{line}");
    assert_eq!(snap["machine"]["online"], true, "{line}");
    assert_eq!(snap["stream"]["transport"], "iroh");
    assert_eq!(snap["stream"]["peer"], b.id.as_str());
    assert!(snap["jobs"].is_array());

    // An endpoint B does not know is refused before any challenge, and B logs it.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let refused = rt.block_on(async {
        let ep = net::lan_endpoint(
            net::iroh::SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
        )
        .await
        .unwrap();
        let to = net::peer_addr(
            b.id.parse().unwrap(),
            format!("127.0.0.1:{}", b.port).parse().unwrap(),
        );
        let r = net::status::watch_status(&ep, to.clone(), Duration::from_secs(5), |_| false).await;
        let j = net::jobs::call(
            &ep,
            to,
            &net::jobs::JobsRequest::Lookup { key: "x".into() },
            Duration::from_secs(5),
        )
        .await;
        ep.close().await;
        (r, j)
    });
    assert_eq!(
        refused.0.unwrap_err(),
        net::ClientError::Refused("NotAllowlisted".into())
    );
    assert_eq!(
        refused.1.unwrap_err(),
        net::ClientError::Refused("NotAllowlisted".into())
    );
    until(
        "refusal in B's log",
        &[&a, &b],
        Duration::from_secs(10),
        || {
            b.daemon_log()
                .contains("REFUSED unknown endpoint")
                .then_some(())
        },
    );
    // A worker-role peer may watch, but not submit jobs to its host.
    let report =
        tinline_task_manager::peer_link::request_peers(&b.socket(), Duration::from_secs(5))
            .unwrap();
    assert_eq!(report.peers[0].role, "host");
}

#[test]
fn forwarded_jobs_run_on_the_peer_stream_logs_mirror_exit_codes_and_link_both_ledgers() {
    let (a, b) = pair("forward");
    let repo = repo(&a);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    // Passing job, run from a subdirectory, with an artifact fetched back.
    let out = forward(
        &a,
        &repo.join("sub"),
        &["--run", "--fetch", "out"],
        "echo hello-from-beta; pwd; cat ../src/a.txt; mkdir -p ../out && echo artifact > ../out/result.txt",
    );
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let a_job = only_job(&a);
    let b_job = only_job(&b);
    assert_eq!(a_job.phase, Phase::Passed);
    assert_eq!(b_job.phase, Phase::Passed);
    // B runs it in its synced checkout, under ITS ledger, owned by WHO@A's host.
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    assert_eq!(b_job.spec.owner, format!("fixture-agent@{}", host.trim()));
    assert!(
        b_job.spec.source.contains("/offload/checkouts/"),
        "{}",
        b_job.spec.source
    );
    assert!(b_job.spec.source.ends_with("/sub"), "{}", b_job.spec.source);
    // Logs arrive at A, in A's job log, exactly once.
    let log = a.job_log(a_job.id);
    // (The argv lines also name it; the output line itself appears exactly once.)
    assert_eq!(note_count(&log, "\nhello-from-beta\n"), 1, "{log}");
    assert!(log.contains("alpha"), "{log}");
    assert!(log.contains(&b_job.spec.source), "{log}");
    // The artifact came back into a new directory.
    let fetched = a.home().join(format!(
        "build-cache/offload/fixture-agent/{}/out/result.txt",
        a_job.id
    ));
    assert_eq!(std::fs::read_to_string(&fetched).unwrap(), "artifact\n");
    // Linked both ways: A's link record and ledger, B's ledger.
    let link = a.link(a_job.id);
    assert_eq!(link["remote_job_id"], b_job.id);
    assert_eq!(link["remote_phase"], "passed");
    assert_eq!(link["stage"], "done");
    until("ledger links", &[&a, &b], Duration::from_secs(15), || {
        let a_peer = a.job(a_job.id).execution?.peer?;
        let b_peer = b.job(b_job.id).execution?.peer?;
        assert_eq!(a_peer.direction, "outbound");
        assert_eq!(a_peer.peer_job_id, Some(b_job.id));
        assert_eq!(a_peer.owner, b_job.spec.owner);
        assert_eq!(b_peer.direction, "inbound");
        assert_eq!(b_peer.peer, "alpha");
        assert_eq!(b_peer.peer_job_id, Some(a_job.id));
        assert_eq!(a_peer.key, b_peer.key);
        Some(())
    });
    // The snapshot's offload field carries the link (UI pairs by remote_job_id).
    let status = tinline_task_manager::request_status(&a.socket(), Duration::from_secs(30))
        .unwrap()
        .status
        .unwrap();
    let view = status.jobs.iter().find(|j| j.id == a_job.id).unwrap();
    let offload = view.offload.as_ref().unwrap();
    assert_eq!(
        (offload.worker.as_str(), offload.remote_job_id),
        ("beta", Some(b_job.id))
    );
    assert_eq!(offload.transport, "iroh");
    assert!(view.command.starts_with("[offload]"), "{}", view.command);
    let b_status = tinline_task_manager::request_status(&b.socket(), Duration::from_secs(30))
        .unwrap()
        .status
        .unwrap();
    let b_view = b_status.jobs.iter().find(|j| j.id == b_job.id).unwrap();
    assert_eq!(
        b_view.checkout,
        repo.canonicalize()
            .unwrap()
            .join("sub")
            .display()
            .to_string()
    );

    // A failing job: A's job fails with B's non-zero exit code.
    let out = forward(&a, &repo, &["--run"], "echo failing-now; exit 3");
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let a_failed = a.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    let b_failed = b.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    assert_eq!(a_failed.phase, Phase::Failed);
    assert_eq!(exit_of(&a_failed), Some(3));
    assert_eq!(b_failed.phase, Phase::Failed);
    assert_eq!(exit_of(&b_failed), Some(3));
    assert!(a.job_log(a_failed.id).contains("failing-now"));
    assert_eq!(a.link(a_failed.id)["remote_job_id"], b_failed.id);
}

#[test]
fn source_sync_sends_only_missing_blobs_prunes_deletions_and_honours_path_rules() {
    let (a, b) = pair("sync");
    let repo = repo(&a);
    let blobs = |a: &Node, id: i64| a.link(id)["blobs_sent"].as_u64();
    let run = |a: &Node| {
        let out = forward(a, &repo, &["--run"], "true");
        assert!(out.status.success(), "{}", stderr(&out));
        a.jobs().into_iter().max_by_key(|j| j.id).unwrap().id
    };
    let first = run(&a);
    // Distinct non-empty contents: a.txt, b.txt, "same" (twice), .gitignore.
    assert_eq!(blobs(&a, first), Some(4), "{}", a.job_log(first));
    let cid = a.link(first)["checkout"].as_str().unwrap().to_owned();
    let co = b.checkouts().join(&cid);
    assert_eq!(
        std::fs::read_to_string(co.join("src/a.txt")).unwrap(),
        "alpha\n"
    );
    for absent in ["target/out.bin", ".env.local", "ignored.txt"] {
        assert!(!co.join(absent).exists(), "{absent} must not be synced");
    }
    let identity: serde_json::Value = serde_json::from_slice(
        &std::fs::read(b.checkouts().join(format!("{cid}.identity"))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        identity["toplevel"],
        repo.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(identity["peer"], a.id.as_str());
    // Unchanged checkout: nothing to send.
    let second = run(&a);
    assert_eq!(blobs(&a, second), Some(0), "{}", a.job_log(second));
    // One changed file: exactly that blob.
    std::fs::write(repo.join("src/b.txt"), "bravo, changed\n").unwrap();
    let third = run(&a);
    assert_eq!(blobs(&a, third), Some(1));
    assert_eq!(
        std::fs::read_to_string(co.join("src/b.txt")).unwrap(),
        "bravo, changed\n"
    );
    // A deleted file is pruned on B (worker-created build output beside it is kept).
    std::fs::remove_file(repo.join("src/a.txt")).unwrap();
    std::fs::create_dir_all(co.join("target")).unwrap();
    std::fs::write(co.join("target/keep.o"), "warm").unwrap();
    let fourth = run(&a);
    assert_eq!(blobs(&a, fourth), Some(0));
    assert!(
        !co.join("src/a.txt").exists(),
        "deleted file must be pruned"
    );
    assert!(
        co.join("target/keep.o").exists(),
        "build outputs are never pruned"
    );
    assert!(
        a.job_log(fourth).contains("1 deleted (src/a.txt)"),
        "{}",
        a.job_log(fourth)
    );
}

#[test]
fn cancel_on_either_side_propagates() {
    let (a, b) = pair("cancel");
    let repo = repo(&a);
    let pidfile = a.root.join("b-pid");
    let script = format!("echo $$ > {}; exec sleep 120", pidfile.display());
    // 1. Cancel on A: B's job is cancelled and its process group killed.
    let mut client = a
        .command(
            &[
                "run",
                "--who",
                "fixture-agent",
                "--peer",
                "beta",
                "--run",
                "--",
                "/bin/sh",
                "-c",
                &script,
            ],
            &repo,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid: i32 = until("B's process", &[&a, &b], WAIT, || {
        std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
    });
    let a_id = only_job(&a).id;
    let b_id = until("B's job running", &[&a, &b], WAIT, || {
        b.jobs()
            .into_iter()
            .find(|j| j.phase == Phase::Running)
            .map(|j| j.id)
    });
    let out = a.cli(&["cancel", &a_id.to_string()], &repo);
    assert!(out.status.success(), "{}", stderr(&out));
    let client_out = finish(&mut client, WAIT);
    assert_ne!(client_out.status.code(), Some(0));
    until(
        "B's job cancelled",
        &[&a, &b],
        Duration::from_secs(60),
        || (b.job(b_id).phase == Phase::Cancelled).then_some(()),
    );
    until(
        "B's process group gone",
        &[&a, &b],
        Duration::from_secs(30),
        || {
            // SAFETY: signal 0 only probes existence.
            (unsafe { libc::kill(pid, 0) } != 0).then_some(())
        },
    );
    assert_eq!(a.job(a_id).phase, Phase::Cancelled);
    assert_eq!(a.link(a_id)["cancel_sent"], true);

    // 2. Cancel on B: A's job fails with a cancelled status (exit 130).
    std::fs::remove_file(&pidfile).unwrap();
    let mut client = a
        .command(
            &[
                "run",
                "--who",
                "fixture-agent",
                "--peer",
                "beta",
                "--run",
                "--",
                "/bin/sh",
                "-c",
                &script,
            ],
            &repo,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    until("B's second process", &[&a, &b], WAIT, || {
        std::fs::read_to_string(&pidfile)
            .ok()
            .filter(|s| !s.is_empty())
    });
    let b_id = until("B's second job running", &[&a, &b], WAIT, || {
        b.jobs()
            .into_iter()
            .find(|j| j.phase == Phase::Running)
            .map(|j| j.id)
    });
    let out = b.cli(&["cancel", &b_id.to_string()], &b.root);
    assert!(out.status.success(), "{}", stderr(&out));
    let client_out = finish(&mut client, WAIT);
    assert_eq!(
        client_out.status.code(),
        Some(130),
        "{}",
        stderr(&client_out)
    );
    let a_job = a.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    assert_eq!(a_job.phase, Phase::Failed);
    assert_eq!(exit_of(&a_job), Some(130));
    assert_eq!(a.link(a_job.id)["remote_phase"], "cancelled");
    assert!(
        a.job_log(a_job.id).contains("CANCELLED there"),
        "{}",
        a.job_log(a_job.id)
    );
}

#[test]
fn lost_link_is_uncertain_then_reattaches_and_restarts_reattach() {
    let (mut a, mut b) = pair("reattach");
    let repo = repo(&a);
    let gate = a.root.join("gate");
    let script = format!(
        "echo started; while [ ! -e {} ]; do sleep 0.2; done; echo finished; exit 0",
        gate.display()
    );
    let mut client = a
        .command(
            &[
                "run",
                "--who",
                "fixture-agent",
                "--peer",
                "beta",
                "--run",
                "--",
                "/bin/sh",
                "-c",
                &script,
            ],
            &repo,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let a_id = until("A's job", &[&a, &b], WAIT, || {
        a.jobs().first().map(|j| j.id)
    });
    until("first log line at A", &[&a, &b], WAIT, || {
        a.job_log(a_id).contains("\nstarted\n").then_some(())
    });
    let b_id = only_job(&b).id;

    // B's daemon dies (its worker keeps the job running): the link is lost -> UNCERTAIN.
    b.kill();
    until("UNCERTAIN", &[&a, &b], Duration::from_secs(30), || {
        (a.link(a_id)["remote_state"] == "uncertain").then_some(())
    });
    assert!(!a.job(a_id).phase.terminal(), "UNCERTAIN is not failed");
    assert!(a.job_log(a_id).contains("UNCERTAIN"), "{}", a.job_log(a_id));
    // B restarts: A re-attaches by id; nothing is resubmitted.
    b.start();
    until("re-attached", &[&a, &b], Duration::from_secs(60), || {
        (a.link(a_id)["remote_state"] == "attached").then_some(())
    });
    // A's daemon restarts too: the bridge (A's job) re-attaches through the new daemon.
    a.kill();
    std::thread::sleep(Duration::from_millis(500));
    a.start();
    std::fs::write(&gate, "").unwrap();
    let out = finish(&mut client, WAIT);
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let log = a.job_log(a_id);
    assert_eq!(note_count(&log, "started\n"), 1, "{log}");
    assert_eq!(note_count(&log, "finished\n"), 1, "{log}");
    assert!(log.contains("re-attached to job"), "{log}");
    assert!(
        log.contains("re-attaching through the local daemon"),
        "{log}"
    );
    assert_eq!(b.jobs().len(), 1, "never double-run");
    assert_eq!(b.job(b_id).phase, Phase::Passed);
    assert_eq!(a.job(a_id).phase, Phase::Passed);
}

#[test]
fn denylisted_devices_are_refused_by_the_peer_and_unreachable_peers_are_clear_errors() {
    let mut a = Node::new("deny", "alpha");
    let mut b = Node::new("deny", "beta");
    a.allow(&b, "worker");
    b.allow(&a, "host");
    // Denied on B only: A's own checks pass, B's admission refuses.
    b.extra_env
        .push(("TASKS_DENIED_DEVICES".into(), "TESTDENY01".into()));
    a.start();
    let repo = repo(&a);

    // B is not running yet: a clear offline error, nothing submitted anywhere.
    let out = forward(&a, &repo, &["--run"], "true");
    assert_eq!(out.status.code(), Some(255), "{}", stderr(&out));
    let job = only_job(&a);
    assert!(
        a.job_log(job.id).contains("offline or unreachable"),
        "{}",
        a.job_log(job.id)
    );
    assert!(a.job_log(job.id).contains("nothing was submitted"));
    assert_eq!(a.link(job.id)["remote_state"], "not_submitted");

    b.start();
    let out = forward(&a, &repo, &["--device", "TESTDENY01"], "adb devices");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let job = a.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    let log = a.job_log(job.id);
    assert!(
        log.contains("refused by peer beta") && log.contains("TESTDENY01"),
        "{log}"
    );
    assert!(b.jobs().is_empty(), "B must not record a denied job");
    // The built-in denylist is refused on A before anything is forwarded.
    let out = forward(&a, &repo, &["--device", "ZD22257P6G"], "true");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("ZD22257P6G") || stdout(&out).contains("ZD22257P6G"));
    assert!(b.jobs().is_empty());
}

#[test]
fn submit_is_idempotent_per_key_and_scoped_to_the_submitting_peer() {
    use net::jobs::{JobsReply, JobsRequest, RemoteResource, SubmitRequest};
    // Two in-process host endpoints (charlie, delta) talk to a real daemon B.
    let mut b = Node::new("idem", "beta");
    let charlie = net::iroh::SecretKey::generate();
    let delta = net::iroh::SecretKey::generate();
    for (name, key) in [("charlie", &charlie), ("delta", &delta)] {
        b.append_config(
            &format!(
                "\n[[peer]]\nname = \"{name}\"\nendpoint_id = \"{}\"\nrole = \"host\"\n",
                key.public()
            ),
            false,
        );
    }
    b.start();
    let src = b.root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), "a\n").unwrap();
    let to = net::peer_addr(
        b.id.parse().unwrap(),
        format!("127.0.0.1:{}", b.port).parse().unwrap(),
    );
    let identity = net::source::CheckoutIdentity {
        host: "synthetic-host".into(),
        toplevel: "/synthetic/repo".into(),
    };
    let cid = net::select::checkout_id(&identity.host, &identity.toplevel);
    let submit = |key: &str| {
        JobsRequest::Submit(SubmitRequest {
            key: key.into(),
            origin_host: identity.host.clone(),
            origin_job_id: Some(1),
            owner: "fixture-agent@synthetic-host".into(),
            checkout: cid.clone(),
            sub: String::new(),
            argv: vec!["true".into()],
            resource: RemoteResource::Run,
            broad_test: false,
        })
    };
    let t = Duration::from_secs(10);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let bind = "127.0.0.1:0".parse().unwrap();
        let c = net::lan_endpoint(charlie.clone(), bind, vec![]).await.unwrap();
        let d = net::lan_endpoint(delta.clone(), bind, vec![]).await.unwrap();
        // Submitting into a checkout nobody synced is refused.
        let r = net::jobs::call(&c, to.clone(), &submit("k1"), t).await.unwrap();
        assert!(
            matches!(&r, JobsReply::Refused { reason } if reason.contains("UnknownCheckout")),
            "{r:?}"
        );
        let manifest = net::source::manifest_from_files(
            &src,
            &["a.txt".to_string()],
            &net::source::Limits::default(),
        )
        .unwrap();
        net::source::sync_push(&c, to.clone(), &src, &manifest, &cid, &identity, t)
            .await
            .unwrap();
        // The same key twice: one job.
        let first = net::jobs::call(&c, to.clone(), &submit("k1"), t).await.unwrap();
        let second = net::jobs::call(&c, to.clone(), &submit("k1"), t).await.unwrap();
        let (JobsReply::Submitted { remote_id: id1, existing: false, .. }, JobsReply::Submitted { remote_id: id2, existing: true, .. }) = (&first, &second) else {
            panic!("{first:?} / {second:?}");
        };
        assert_eq!(id1, id2);
        // Another peer: cannot see, attach to or cancel it, nor use its checkout.
        let lookup = net::jobs::call(&d, to.clone(), &JobsRequest::Lookup { key: "k1".into() }, t)
            .await
            .unwrap();
        assert_eq!(lookup, JobsReply::Found { remote_id: None });
        let cancel = net::jobs::call(&d, to.clone(), &JobsRequest::Cancel { remote_id: *id1 }, t)
            .await
            .unwrap();
        assert!(matches!(cancel, JobsReply::Refused { .. }), "{cancel:?}");
        let attach = net::jobs::attach(&d, to.clone(), *id1, 0, t, t, |_| Ok(())).await;
        assert!(matches!(attach, Err(net::ClientError::Refused(_))), "{attach:?}");
        let theirs = net::jobs::call(&d, to.clone(), &submit("k2"), t).await.unwrap();
        assert!(
            matches!(&theirs, JobsReply::Refused { reason } if reason.contains("CheckoutOwnedByAnotherPeer")),
            "{theirs:?}"
        );
        let found = net::jobs::call(&c, to.clone(), &JobsRequest::Lookup { key: "k1".into() }, t)
            .await
            .unwrap();
        assert_eq!(found, JobsReply::Found { remote_id: Some(*id1) });
        // The owner attaches and sees it finish.
        let done = net::jobs::attach(&c, to.clone(), *id1, 0, t, Duration::from_secs(60), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!((done.phase.as_str(), done.exit_code), ("passed", Some(0)));
        c.close().await;
        d.close().await;
    });
    assert_eq!(b.jobs().len(), 1, "never two jobs for one key");
}

// ---------------------------------------------------------------------------------------------
// MCP placement over the peer link: `tasks mcp` on A, the peer runtime's snapshots of B.

/// `tasks mcp --no-start-daemon` with a node's environment (its stderr goes to the state dir).
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    out: std::io::BufReader<std::process::ChildStdout>,
    next: i64,
}

impl Mcp {
    fn start(node: &Node) -> Self {
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(node.state().join("fixture-mcp.log"))
            .unwrap();
        let mut child = node
            .command(&["mcp", "--no-start-daemon"], &node.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            stdin,
            out,
            next: 1,
        };
        let init = mcp.request(
            "initialize",
            serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {},
                               "clientInfo": {"name": "fixture", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "tasks", "{init}");
        mcp.send(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }
    fn send(&mut self, message: &serde_json::Value) {
        use std::io::Write;
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::BufRead;
        let id = self.next;
        self.next += 1;
        self.send(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).unwrap();
            assert!(n > 0, "tasks mcp closed its stdout");
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }
    /// A tools/call result (a result, not a JSON-RPC error).
    fn tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let reply = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(result: &serde_json::Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// A's peer runtime's view of B.
fn peer_view(a: &Node, name: &str) -> Option<tinline_task_manager::peer_link::PeerSnapshot> {
    tinline_task_manager::peer_link::request_peers(&a.socket(), Duration::from_secs(5))
        .ok()?
        .snapshots
        .into_iter()
        .find(|s| s.name == name)
}

fn wait_peer_online(a: &Node, b: &Node) {
    until("B online in A's peer runtime", &[a, b], WAIT, || {
        peer_view(a, b.name).filter(|s| s.online() && !s.stale && s.snapshot.is_some())
    });
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_owned()
}

/// Holds a node's compile slot (its isolated `p2p-build.lock`) like a running build would,
/// until dropped. Close-on-exec (std default), so no fixture process inherits it.
fn hold_compile_slot(node: &Node) -> std::fs::File {
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(node.root.join("locks/p2p-build.lock"))
        .unwrap();
    file.lock().unwrap();
    file
}

fn compile_slot_busy(snapshot: &serde_json::Value) -> bool {
    snapshot["machine"]["slots"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["kind"] == "compile" && s["busy"] == true)
}

fn local_status(node: &Node) -> serde_json::Value {
    let status = tinline_task_manager::request_status(&node.socket(), Duration::from_secs(30))
        .unwrap()
        .status
        .unwrap();
    serde_json::to_value(status).unwrap()
}

#[test]
fn mcp_submit_to_a_named_peer_runs_there_and_names_it() {
    let (a, b) = pair("mcp-named");
    let repo = repo(&a);
    wait_peer_online(&a, &b);
    let mut mcp = Mcp::start(&a);
    let result = mcp.tool(
        "submit",
        serde_json::json!({"who": "fixture-agent", "cwd": repo, "class": "run",
                           "argv": ["/bin/sh", "-c", "echo hello-mcp-beta"],
                           "placement": "beta", "wait": true, "timeout_s": 170,
                           "tail_lines": 50}),
    );
    assert_eq!(
        result["isError"],
        false,
        "{result}\n{}",
        diagnose(&[&a, &b])
    );
    let s = &result["structuredContent"];
    assert_eq!(s["phase"], "passed", "{result}\n{}", diagnose(&[&a, &b]));
    let b_job = only_job(&b);
    assert_eq!(b_job.phase, Phase::Passed);
    // The result names the machine, why, and the remote job id.
    assert_eq!(s["machine"], "beta", "{result}");
    assert_eq!(s["remote_job_id"], b_job.id, "{result}");
    assert_eq!(s["placement"]["forwarded"], true);
    assert_eq!(s["decision"]["requested"], "beta");
    assert!(
        s["decision"]["reason"]
            .as_str()
            .unwrap()
            .contains("named explicitly")
    );
    let text = text_of(&result);
    assert!(
        text.contains(&format!("remote job {} on beta", b_job.id)),
        "{text}"
    );
    assert!(
        s["log_tail"].as_str().unwrap().contains("hello-mcp-beta"),
        "{result}"
    );
    // The `job` tool says the same later.
    let a_id = s["job_id"].as_i64().unwrap();
    let job = mcp.tool("job", serde_json::json!({"id": a_id}));
    assert_eq!(job["structuredContent"]["machine"], "beta", "{job}");
    assert_eq!(job["structuredContent"]["remote_job_id"], b_job.id, "{job}");
    assert!(
        text_of(&job).contains("named explicitly"),
        "{}",
        text_of(&job)
    );
}

#[test]
fn mcp_cluster_status_lists_both_machines() {
    let (a, b) = pair("mcp-status");
    wait_peer_online(&a, &b);
    let mut mcp = Mcp::start(&a);
    let result = mcp.tool("cluster_status", serde_json::json!({}));
    assert_eq!(result["isError"], false, "{result}");
    let machines = result["structuredContent"]["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 2, "{result}");
    assert_eq!(machines[0]["local"], true);
    assert_eq!(machines[0]["name"], hostname());
    assert_eq!(machines[0]["cargo_jobs_max"], 2);
    assert_eq!(machines[1]["name"], "beta");
    assert_eq!(machines[1]["local"], false);
    assert_eq!(machines[1]["online"], true);
    assert_eq!(machines[1]["stale"], false);
    assert!(machines[1]["age_s"].as_f64().unwrap() < 30.0, "{result}");
    assert_eq!(machines[1]["machine_id"], hostname());
    assert!(machines[1]["snapshot"]["machine"]["slots"].is_array());
    assert!(machines[1]["snapshot"]["machine"]["history"].is_null());
    let text = text_of(&result);
    assert!(
        text.contains("=== peer beta (worker): online [online]"),
        "{text}"
    );
}

#[test]
fn mcp_anywhere_goes_where_the_compile_slot_is_free() {
    let (a, b) = pair("mcp-anywhere");
    let repo = repo(&a);
    wait_peer_online(&a, &b);
    let mut mcp = Mcp::start(&a);
    let submit = |mcp: &mut Mcp, tag: &str| {
        // No placement argument: a worker peer is configured, so the default is anywhere.
        mcp.tool(
            "submit",
            serde_json::json!({"who": "fixture-agent", "cwd": repo,
                               "argv": ["/bin/sh", "-c", format!("echo placed-{tag}")],
                               "wait": true, "timeout_s": 170, "tail_lines": 20}),
        )
    };

    // 1. A's compile slot held, B idle: B.
    let held = hold_compile_slot(&a);
    until("A's compile slot busy", &[&a, &b], WAIT, || {
        compile_slot_busy(&local_status(&a)).then_some(())
    });
    let result = submit(&mut mcp, "one");
    drop(held);
    // A sample exchange for the record (visible with --nocapture).
    eprintln!(
        "sample submit result:\n{}\n{}",
        text_of(&result),
        serde_json::to_string_pretty(&result["structuredContent"]["decision"]).unwrap()
    );
    let s = &result["structuredContent"];
    assert_eq!(
        result["isError"],
        false,
        "{result}\n{}",
        diagnose(&[&a, &b])
    );
    assert_eq!(s["phase"], "passed", "{result}\n{}", diagnose(&[&a, &b]));
    assert_eq!(s["machine"], "beta", "{result}");
    assert_eq!(s["decision"]["requested"], "anywhere");
    let b_job = only_job(&b);
    assert_eq!(s["remote_job_id"], b_job.id, "{result}");
    assert!(
        text_of(&result).contains("placement anywhere -> beta"),
        "{result}"
    );
    let here = s["decision"]["scores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["target"]["kind"] == "local")
        .unwrap();
    assert!(
        here["wait_basis"].as_str().unwrap().contains("slot held"),
        "{here}"
    );

    // 2. B's compile slot held, A idle: A (no forwarding, B gets no new job).
    let held = hold_compile_slot(&b);
    until("B's held slot seen by A", &[&a, &b], WAIT, || {
        let view = peer_view(&a, "beta")?;
        (!view.stale && compile_slot_busy(view.snapshot.as_ref()?)).then_some(())
    });
    until("A's compile slot free", &[&a, &b], WAIT, || {
        (!compile_slot_busy(&local_status(&a))).then_some(())
    });
    let result = submit(&mut mcp, "two");
    drop(held);
    let s = &result["structuredContent"];
    assert_eq!(
        result["isError"],
        false,
        "{result}\n{}",
        diagnose(&[&a, &b])
    );
    assert_eq!(s["phase"], "passed", "{result}\n{}", diagnose(&[&a, &b]));
    assert_eq!(s["machine"], hostname(), "{result}");
    assert_eq!(s["placement"]["forwarded"], false, "{result}");
    assert_eq!(b.jobs().len(), 1, "nothing new was forwarded");
    let a_job = a.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    assert!(
        a.job_log(a_job.id).contains("placed-two"),
        "{}",
        a.job_log(a_job.id)
    );
}

#[test]
fn mcp_anywhere_falls_back_when_the_peer_is_down_and_a_named_peer_is_a_clean_error() {
    let mut a = Node::new("mcp-down", "alpha");
    let b = Node::new("mcp-down", "beta");
    a.allow(&b, "worker");
    a.start();
    let repo = repo(&a);
    // B never starts: A's runtime reports it offline once its first connect fails.
    until("B offline in A's peer runtime", &[&a], WAIT, || {
        peer_view(&a, "beta").filter(|s| s.state == "offline")
    });
    let mut mcp = Mcp::start(&a);
    let result = mcp.tool(
        "submit",
        serde_json::json!({"who": "fixture-agent", "cwd": repo, "placement": "anywhere",
                           "argv": ["/bin/sh", "-c", "echo fell-back"],
                           "wait": true, "timeout_s": 170}),
    );
    let s = &result["structuredContent"];
    assert_eq!(result["isError"], false, "{result}\n{}", diagnose(&[&a]));
    assert_eq!(s["phase"], "passed", "{result}\n{}", diagnose(&[&a]));
    assert_eq!(s["machine"], hostname(), "{result}");
    let notes = s["placement"]["notes"].to_string();
    assert!(
        notes.contains("beta skipped: offline") && notes.contains("fell back to local"),
        "{result}"
    );
    assert!(
        text_of(&result).contains("beta skipped: offline"),
        "{result}"
    );
    // Named explicitly: a clean tool error, nothing submitted.
    let before = a.jobs().len();
    let result = mcp.tool(
        "submit",
        serde_json::json!({"who": "fixture-agent", "cwd": repo, "placement": "beta",
                           "argv": ["/bin/sh", "-c", "true"]}),
    );
    assert_eq!(result["isError"], true, "{result}");
    let text = text_of(&result);
    assert!(
        text.contains("peer beta is offline") && text.contains("nothing was submitted"),
        "{text}"
    );
    assert_eq!(a.jobs().len(), before, "nothing was submitted");
    // cluster_status shows it offline.
    let status = mcp.tool("cluster_status", serde_json::json!({}));
    let machines = status["structuredContent"]["machines"].as_array().unwrap();
    assert_eq!(machines[1]["name"], "beta");
    assert_eq!(machines[1]["online"], false);
    assert_eq!(machines[1]["state"], "offline");
}

// ---------------------------------------------------------------------------------------------
// feature/log-search: compile attribution, log search and the submission policy over MCP.

/// One daemon with a peer config but no peers (the peer runtime runs; placement is local).
fn single(test: &str) -> Node {
    let mut a = Node::new(test, "alpha");
    a.start();
    a
}

/// A script named like the compiler: its process's comm is `rustc` and its cmdline is
/// `/bin/sh .../rustc ARGS`, exactly what the sampler reads for a real rustc.
fn fake_compiler(node: &Node, name: &str, seconds: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = node.root.join("fake-bin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"   Compiling fake-unit v0.1.0\"\nsleep {seconds}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn mcp_who_is_compiling_names_the_job_compiling_a_crate() {
    let a = single("who-compiling");
    let rustc = fake_compiler(&a, "rustc", 40);
    let mut mcp = Mcp::start(&a);
    let submitted = mcp.tool(
        "submit",
        serde_json::json!({"who": "fixture-agent@synthetic", "cwd": a.root, "class": "run",
                           "placement": "local",
                           "argv": [rustc, "--crate-name", "profile_sync", "--edition=2024",
                                    "tests/profile_sync.rs", "--test"]}),
    );
    assert_eq!(submitted["isError"], false, "{submitted}");
    let id = submitted["structuredContent"]["job_id"].as_i64().unwrap();
    // Admission may hold the job under host memory pressure (the fixture keeps the PSI gates).
    let found = until("profile_sync in who_is_compiling", &[&a], WAIT * 3, || {
        std::thread::sleep(Duration::from_millis(900));
        let r = mcp.tool(
            "who_is_compiling",
            serde_json::json!({"crate": "profile-sync"}),
        );
        let jobs = r["structuredContent"]["machines"][0]["jobs"]
            .as_array()?
            .clone();
        (!jobs.is_empty()).then_some(r)
    });
    eprintln!(
        "who_is_compiling {{\"crate\": \"profile-sync\"}} ->\n{}",
        text_of(&found)
    );
    let s = &found["structuredContent"];
    let m = &s["machines"][0];
    assert_eq!(m["machine"], hostname(), "{found}");
    assert_eq!(m["local"], true);
    let job = &m["jobs"][0];
    assert_eq!(job["id"], id, "{found}");
    assert_eq!(job["owner"], "fixture-agent@synthetic");
    assert_eq!(job["agent"], "fixture-agent");
    let c = &job["compiling"][0];
    assert_eq!(
        (
            c["tool"].as_str(),
            c["unit"].as_str(),
            c["crate"].as_str(),
            c["kind"].as_str(),
            c["proc"].as_str()
        ),
        (
            Some("cargo"),
            Some("profile_sync"),
            Some("profile_sync"),
            Some("test"),
            Some("rustc")
        ),
        "{found}"
    );
    assert!(
        c["pid"].as_u64().unwrap() > 1 && c["rss_bytes"].as_u64().unwrap() > 0,
        "{found}"
    );
    let text = text_of(&found);
    assert!(
        text.contains("fixture-agent@synthetic") && text.contains("profile_sync (test via rustc)"),
        "{text}"
    );
    // Another crate: nobody. cluster_status carries the same `compiling` list.
    let other = mcp.tool("who_is_compiling", serde_json::json!({"crate": "video"}));
    assert!(
        other["structuredContent"]["machines"][0]["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(text_of(&other).contains("nobody is compiling video"));
    let status = mcp.tool("cluster_status", serde_json::json!({}));
    let jobs = status["structuredContent"]["jobs"].as_array().unwrap();
    let live = jobs.iter().find(|j| j["id"] == id).unwrap();
    assert!(live["compiling"].is_array(), "{live}");
    // job_cost of a live job reports live values.
    let cost = mcp.tool("job_cost", serde_json::json!({"id": id}));
    assert_eq!(cost["structuredContent"]["source"], "live", "{cost}");
    let cancelled = mcp.tool(
        "cancel",
        serde_json::json!({"id": id, "who": "fixture-agent@synthetic"}),
    );
    assert_eq!(cancelled["isError"], false, "{cancelled}");
    until("job cancelled", &[&a], WAIT, || {
        a.job(id).phase.terminal().then_some(())
    });
}

#[test]
fn mcp_search_logs_finds_lines_of_finished_jobs_within_bounds() {
    let a = single("search");
    let mut mcp = Mcp::start(&a);
    let run = |mcp: &mut Mcp, script: &str| {
        let r = mcp.tool(
            "submit",
            serde_json::json!({"who": "fixture-agent", "cwd": a.root, "class": "run",
                               "placement": "local", "argv": ["/bin/sh", "-c", script],
                               "wait": true, "timeout_s": 170}),
        );
        assert_eq!(
            r["structuredContent"]["phase"],
            "passed",
            "{r}\n{}",
            diagnose(&[&a])
        );
        r["structuredContent"]["job_id"].as_i64().unwrap()
    };
    // The needle is assembled at run time, so the job log's argv line does not contain it.
    let first = run(
        &mut mcp,
        "n=needle; echo alpha; echo $n-xyz-1; echo omega; echo $n-xyz-2",
    );
    let second = run(&mut mcp, "echo nothing to see");
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "needle-xyz", "context": 1}),
    );
    eprintln!(
        "search_logs {{\"pattern\": \"needle-xyz\", \"context\": 1}} ->\n{}",
        text_of(&r)
    );
    let s = &r["structuredContent"];
    assert_eq!(r["isError"], false, "{r}");
    let matches = s["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "{r}");
    let m = &matches[0];
    assert_eq!(
        (m["job_id"].as_i64(), m["owner"].as_str()),
        (Some(first), Some("fixture-agent"))
    );
    assert_eq!(m["machine"], hostname());
    assert_eq!(m["line"], "needle-xyz-1");
    assert_eq!(m["before"], serde_json::json!(["alpha"]));
    assert_eq!(m["after"], serde_json::json!(["omega"]));
    assert_eq!(
        matches[1]["line_number"].as_u64().unwrap(),
        m["line_number"].as_u64().unwrap() + 2
    );
    assert_eq!(s["truncated"], false);
    assert!(
        text_of(&r).contains(&format!("#{first} fixture-agent (passed) line")),
        "{}",
        text_of(&r)
    );
    // Bounds: one match; newest job only.
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "needle-xyz", "max_matches": 1}),
    );
    assert_eq!(
        r["structuredContent"]["matches"].as_array().unwrap().len(),
        1
    );
    assert_eq!(r["structuredContent"]["truncated"], true);
    assert_eq!(
        r["structuredContent"]["machines"][0]["report"]["truncated_reason"],
        "max_matches"
    );
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "needle-xyz", "max_jobs": 1}),
    );
    let report = &r["structuredContent"]["machines"][0]["report"];
    assert_eq!(
        (
            report["jobs_scanned"].as_u64(),
            report["truncated_reason"].as_str()
        ),
        (Some(1), Some("max_jobs")),
        "{r}"
    );
    assert!(
        r["structuredContent"]["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Regex subset, filters and time.
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "^needle-x[a-z]z-\\d$", "regex": true, "owner": "fixture-agent",
                           "job_ids": [first, second], "since": "1h", "phases": ["passed"]}),
    );
    assert_eq!(
        r["structuredContent"]["matches"].as_array().unwrap().len(),
        2,
        "{r}"
    );
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "needle", "owner": "somebody-else"}),
    );
    assert!(
        r["structuredContent"]["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let bad = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "(x", "regex": true}),
    );
    assert_eq!(bad["isError"], true, "{bad}");
    let bad = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "x", "machine": "nowhere"}),
    );
    assert!(text_of(&bad).contains("unknown machine"), "{bad}");
    // job_cost of a finished job.
    let cost = mcp.tool("job_cost", serde_json::json!({"id": first}));
    let c = &cost["structuredContent"];
    assert_eq!(
        (c["source"].as_str(), c["machine"].as_str()),
        (Some("ledger"), Some(hostname().as_str())),
        "{cost}"
    );
    assert!(
        c["wall_s"].as_f64().is_some() && c["hint"].is_string(),
        "{cost}"
    );
}

#[test]
fn mcp_search_logs_finds_forwarded_and_peer_only_jobs_and_names_the_machine() {
    let (a, b) = pair("search-peer");
    let repo = repo(&a);
    let out = forward(&a, &repo, &["--run"], "echo forwarded-needle-42");
    assert!(
        out.status.success(),
        "{}\n{}",
        stderr(&out),
        diagnose(&[&a, &b])
    );
    let a_job = only_job(&a);
    let b_forwarded = only_job(&b);
    // A job that exists only on B (never forwarded from A).
    let out = b.cli(
        &[
            "run",
            "--who",
            "beta-local",
            "--run",
            "--",
            "/bin/sh",
            "-c",
            "echo beta-only-needle-7",
        ],
        &b.root,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let b_local = b.jobs().into_iter().map(|j| j.id).max().unwrap();
    let mut mcp = Mcp::start(&a);
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "forwarded-needle-42"}),
    );
    eprintln!(
        "search_logs {{\"pattern\": \"forwarded-needle-42\"}} ->\n{}",
        text_of(&r)
    );
    let matches = r["structuredContent"]["matches"]
        .as_array()
        .unwrap()
        .clone();
    // Found once (A's mirror; B skips the job A mirrors), named by the peer.
    let hits: Vec<_> = matches
        .iter()
        .filter(|m| m["line"] == "forwarded-needle-42")
        .collect();
    assert_eq!(hits.len(), 1, "{r}\n{}", diagnose(&[&a, &b]));
    assert_eq!(hits[0]["machine"], "beta", "{r}");
    assert_eq!(hits[0]["job_id"], a_job.id);
    assert_eq!(hits[0]["remote_job_id"], b_forwarded.id, "{r}");
    assert!(
        text_of(&r).contains(&format!("beta #{} (remote #{})", a_job.id, b_forwarded.id)),
        "{}",
        text_of(&r)
    );
    // A job that lives only on B is searched there over the peer link.
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "beta-only-needle-7"}),
    );
    let matches = r["structuredContent"]["matches"].as_array().unwrap();
    let hit = matches
        .iter()
        .find(|m| m["line"] == "beta-only-needle-7")
        .unwrap_or_else(|| panic!("{r}\n{}", diagnose(&[&a, &b])));
    assert_eq!(
        (
            hit["machine"].as_str(),
            hit["job_id"].as_i64(),
            hit["owner"].as_str()
        ),
        (Some("beta"), Some(b_local), Some("beta-local")),
        "{r}"
    );
    // Machine filters.
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "forwarded-needle-42", "machine": "local"}),
    );
    assert!(
        r["structuredContent"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["line"] != "forwarded-needle-42"),
        "{r}"
    );
    let r = mcp.tool(
        "search_logs",
        serde_json::json!({"pattern": "needle", "machine": "beta"}),
    );
    let lines: Vec<&str> = r["structuredContent"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["line"].as_str())
        .collect();
    assert!(
        lines.contains(&"forwarded-needle-42") && lines.contains(&"beta-only-needle-7"),
        "{r}"
    );
    assert!(
        b.daemon_log().contains("log search for alpha"),
        "{}",
        b.daemon_log()
    );
}

#[test]
fn broad_cargo_tests_are_refused_at_submit_unless_escaped() {
    let a = single("policy");
    let checkout = a.root.join("ws");
    std::fs::create_dir_all(checkout.join("tests")).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::write(
        checkout.join("Cargo.toml"),
        "[package]\nname = \"p2pcore\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    for f in ["src/lib.rs", "tests/video_calls.rs", "tests/audio.rs"] {
        std::fs::write(checkout.join(f), "").unwrap();
    }
    let broad = ["cargo", "test", "--offline", "-p", "p2pcore", "video_call"];
    // CLI: exit 2, the message names the count and the matching targets, nothing queued.
    let mut args = vec!["run", "--who", "fixture-agent", "--run", "--"];
    args.extend_from_slice(&broad);
    let out = a.cli(&args, &checkout);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refused: this builds all 3 test targets of -p p2pcore; your filter 'video_call' matches: --lib, --test video_calls. Re-run with those, or pass --broad-test"),
        "{}",
        stderr(&out)
    );
    assert!(a.jobs().is_empty(), "nothing queued");
    // A full suite (no filter) and a narrowed one are not affected (enqueue: never run).
    for cmd in [
        vec!["cargo", "test", "-p", "p2pcore"],
        vec![
            "cargo",
            "test",
            "-p",
            "p2pcore",
            "--test",
            "video_calls",
            "video_call",
        ],
    ] {
        let mut args = vec!["enqueue", "--who", "fixture-agent", "--run", "--"];
        args.extend_from_slice(&cmd);
        let out = a.cli(&args, &checkout);
        assert!(out.status.success(), "{cmd:?}: {}", stderr(&out));
    }
    // The escape hatch is recorded on the job.
    let mut args = vec![
        "enqueue",
        "--who",
        "fixture-agent",
        "--run",
        "--broad-test",
        "--",
    ];
    args.extend_from_slice(&broad);
    let out = a.cli(&args, &checkout);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--broad-test: building every target deliberately"),
        "{}",
        stderr(&out)
    );
    let escaped = a.jobs().into_iter().max_by_key(|j| j.id).unwrap();
    let recipe: serde_json::Value = serde_json::from_str(&escaped.spec.configuration).unwrap();
    assert_eq!(recipe["broad_test"], true, "{recipe}");
    // MCP: a tool error and nothing submitted; broad_test: true is accepted and recorded.
    let before = a.jobs().len();
    let mut mcp = Mcp::start(&a);
    let r = mcp.tool(
        "submit",
        serde_json::json!({"who": "fixture-agent", "cwd": checkout, "class": "run",
                           "placement": "local", "argv": broad}),
    );
    assert_eq!(r["isError"], true, "{r}");
    assert!(
        text_of(&r).contains("refused: this builds all 3 test targets")
            && text_of(&r).contains("Nothing was submitted"),
        "{r}"
    );
    assert_eq!(a.jobs().len(), before);
    // The policy is visible in `tasks admission` with its layers.
    let out = a.cli(&["admission"], &a.root);
    let admission: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let policy = &admission["policy"];
    assert_eq!(policy["policy"]["broad_test"], "refuse", "{admission}");
    assert_eq!(
        (
            policy["broad_test_source"].as_str(),
            policy["file_present"].as_bool()
        ),
        (Some("default"), Some(false))
    );
    assert!(
        policy["path"]
            .as_str()
            .unwrap()
            .ends_with(".config/tasks/policy.huml"),
        "{policy}"
    );
    assert!(
        stderr(&out).contains("policy: broad_test=refuse (from default)"),
        "{}",
        stderr(&out)
    );
    // A strict, malformed policy file: loud on submit, defaults (refuse) still apply.
    std::fs::write(
        a.config().with_file_name("policy.huml"),
        "%HUML v0.2.0
broad_tset: \"warn\"\n",
    )
    .unwrap();
    let mut args = vec!["enqueue", "--who", "fixture-agent", "--run", "--"];
    args.extend_from_slice(&broad);
    let out = a.cli(&args, &checkout);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("broad_tset"), "{}", stderr(&out));
    let out = a.cli(&["admission"], &a.root);
    let admission: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        admission["policy"]["source"], "default (file invalid)",
        "{admission}"
    );
    assert!(stderr(&out).contains("policy ERROR"), "{}", stderr(&out));
    // Queued jobs are never re-evaluated: cancel what we enqueued.
    for j in a.jobs() {
        let _ = tinline_task_manager::request(&a.socket(), Action::Cancel(j.id));
    }
}
