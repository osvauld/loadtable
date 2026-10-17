//! `tasks peer-forward`: the command of a forwarded job. It runs under this machine's worker in
//! the offload slot, so the job's log, phase, exit code and cancellation work like any job's.
//! It names only its own job id (`TASKS_JOB_ID`) to the daemon's peer runtime; the daemon reads
//! what to forward from the approved ledger record.
//!
//! * stdout: the peer job's log, byte for byte (re-attaching never repeats or skips bytes);
//!   stderr: `tasks peer:` progress notes (sync, submission, UNCERTAIN/re-attached, result).
//! * exit: the peer job's exit code (0 passed, its code if failed, 130 cancelled on the peer),
//!   2 refused (unknown peer, validation, device denylist on the peer), 255 peer offline before
//!   submission / interrupted / still uncertain after the re-attach window, 1 fetch failure.
//! * SIGTERM/SIGINT (a local `tasks cancel`): asks the daemon to cancel the peer job, then exits;
//!   the daemon's watcher also cancels it if this process is killed first.
//! * If the daemon goes away (restart), the bridge keeps the job alive and re-attaches through
//!   the restarted daemon from the last byte it printed.
use super::forward::{BridgeEvent, BridgeRequest};
use crate::peer_link::ForwardArgs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: i32) {
    SIGNAL.store(signal, Ordering::SeqCst);
}

fn note(text: &str) {
    eprintln!("tasks peer: {text}");
}

enum Msg {
    Event(BridgeEvent),
    Log(Vec<u8>),
    Closed(String),
}

fn reader(stream: UnixStream, tx: mpsc::Sender<Msg>) {
    let mut r = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(Msg::Closed("daemon closed the bridge".into()));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Msg::Closed(e.to_string()));
                return;
            }
        }
        let event: BridgeEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Msg::Closed(format!("bad event: {e}")));
                return;
            }
        };
        if let BridgeEvent::Log { size, .. } = &event {
            let mut buf = vec![0u8; *size as usize];
            if let Err(e) = r.read_exact(&mut buf) {
                let _ = tx.send(Msg::Closed(e.to_string()));
                return;
            }
            if tx.send(Msg::Log(buf)).is_err() {
                return;
            }
            continue;
        }
        if tx.send(Msg::Event(event)).is_err() {
            return;
        }
    }
}

fn send(socket: &Path, request: &BridgeRequest) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    Ok(stream)
}

fn cancel(socket: &Path, job_id: i64) {
    match send(socket, &BridgeRequest::Cancel { job_id }) {
        Ok(stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            let _ = BufReader::new(stream).read_line(&mut line);
            note(&format!(
                "local cancel: asked the daemon to cancel the peer job ({})",
                line.trim()
            ));
        }
        Err(e) => note(&format!(
            "local cancel: daemon unreachable ({e}); its watcher cancels the peer job when it sees this job cancelled"
        )),
    }
}

/// Entry point; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let forward = match ForwardArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            note(&format!("usage: {e}"));
            return 2;
        }
    };
    let job_id: i64 = match std::env::var("TASKS_JOB_ID")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(id) if id > 0 => id,
        _ => {
            note("runs only as the command of a tasks job (use: tasks run --peer NAME -- COMMAND)");
            return 2;
        }
    };
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores into an atomic.
        unsafe {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
    let socket = forward.state.join("peer.sock");
    let mut offset = 0u64;
    let mut reattach = Duration::from_secs(3600);
    let mut lost_since: Option<Instant> = None;
    let mut ever_connected = false;
    let first_try = Instant::now();
    let mut stdout = std::io::stdout().lock();
    loop {
        if SIGNAL.load(Ordering::SeqCst) != 0 {
            cancel(&socket, job_id);
            return 128 + SIGNAL.load(Ordering::SeqCst);
        }
        let stream = match send(&socket, &BridgeRequest::Forward { job_id, offset }) {
            Ok(s) => s,
            Err(e) => {
                if !ever_connected && first_try.elapsed() > Duration::from_secs(10) {
                    note(&format!(
                        "the local daemon has no peer runtime at {} ({e}): is the peer config set up and the daemon restarted with it? (tasks peer list)",
                        socket.display()
                    ));
                    return 255;
                }
                if let Some(since) = lost_since
                    && since.elapsed() > reattach
                {
                    note(&format!(
                        "UNCERTAIN: the local daemon did not come back within {reattach:?}; the peer job may still be running (tasks peer list)"
                    ));
                    return 255;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if ever_connected && lost_since.is_some() {
            note(&format!(
                "re-attaching through the local daemon from log offset {offset}"
            ));
        }
        ever_connected = true;
        let (tx, rx) = mpsc::channel();
        let read_half = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                note(&format!("bridge: {e}"));
                return 255;
            }
        };
        std::thread::spawn(move || reader(read_half, tx));
        let closed = loop {
            if SIGNAL.load(Ordering::SeqCst) != 0 {
                cancel(&socket, job_id);
                let _ = stdout.flush();
                return 128 + SIGNAL.load(Ordering::SeqCst);
            }
            let msg = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break "reader stopped".to_string(),
            };
            match msg {
                Msg::Log(bytes) => {
                    offset += bytes.len() as u64;
                    let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
                }
                Msg::Event(BridgeEvent::Hello { reattach_s }) => {
                    reattach = Duration::from_secs(reattach_s.max(1));
                    lost_since = None;
                }
                Msg::Event(BridgeEvent::Note { text }) => note(&text),
                Msg::Event(BridgeEvent::State {
                    remote_job_id,
                    remote_state,
                    remote_phase,
                }) => note(&format!(
                    "remote job {} {remote_state} ({})",
                    remote_job_id.map_or("-".into(), |i| i.to_string()),
                    remote_phase.unwrap_or_default()
                )),
                Msg::Event(BridgeEvent::Done {
                    exit_code,
                    remote_state,
                    message,
                    ..
                }) => {
                    let _ = stdout.flush();
                    note(&format!(
                        "{message} [remote_state={remote_state}, exit {exit_code}]"
                    ));
                    return exit_code;
                }
                Msg::Event(BridgeEvent::Error { message }) => {
                    let _ = stdout.flush();
                    note(&format!("refused by the local daemon: {message}"));
                    return 2;
                }
                Msg::Event(_) => {}
                Msg::Closed(why) => break why,
            }
        };
        note(&format!(
            "lost the local daemon's peer bridge ({closed}); the peer job is unaffected; waiting to re-attach"
        ));
        lost_since.get_or_insert_with(Instant::now);
        std::thread::sleep(Duration::from_millis(500));
    }
}
