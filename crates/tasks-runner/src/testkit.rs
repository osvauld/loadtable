//! Local fakes for the runner's tests.

use crate::{Clock, Lease, Outcome, RunEvents, TimedLine};
use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tasks_platform::{Child, GroupId, PlatformError, Processes, Signal, SpawnSpec, Usage};

/// A directory removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "tasks-runner-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A clock whose `sleep` just advances time.
#[derive(Debug, Default)]
pub(crate) struct ManualClock(Cell<Duration>);

impl ManualClock {
    pub(crate) fn advance(&self, d: Duration) {
        self.0.set(self.0.get() + d);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

/// A scripted process group driven by a [`ManualClock`].
pub(crate) struct FakeProcs {
    pub(crate) clock: Rc<ManualClock>,
    /// The leader exits by itself at this time with this code.
    pub(crate) exit_at: Option<(Duration, i32)>,
    /// Output appended to the log once the clock reaches the time.
    pub(crate) output: Mutex<Vec<(Duration, Vec<u8>)>>,
    /// When true `Term` is ignored and only `Kill` stops the leader.
    pub(crate) ignores_term: bool,
    /// When true signals fail with `Gone`.
    pub(crate) gone: bool,
    /// When true `peek_exit` and `try_wait` fail (lost reaping ownership).
    pub(crate) lost_wait: bool,
    /// Descendants that outlive the leader until signalled.
    pub(crate) strays: u32,
    /// When true the strays ignore `Term` and only `Kill` stops them.
    pub(crate) strays_ignore_term: bool,
    pub(crate) signals: Mutex<Vec<Signal>>,
    /// The `cgroup.procs` file the last spawn was told to join.
    pub(crate) joined: Mutex<Option<PathBuf>>,
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    log: Option<PathBuf>,
    termed: bool,
    killed: bool,
    reaped: bool,
    /// Strays still alive; set at spawn.
    strays: u32,
    /// The leader's code, once known.
    exit: Option<i32>,
}

impl FakeProcs {
    pub(crate) fn new(clock: Rc<ManualClock>) -> FakeProcs {
        FakeProcs {
            clock,
            exit_at: None,
            output: Mutex::new(Vec::new()),
            ignores_term: false,
            gone: false,
            lost_wait: false,
            strays: 0,
            strays_ignore_term: false,
            signals: Mutex::new(Vec::new()),
            joined: Mutex::new(None),
            state: Mutex::new(FakeState::default()),
        }
    }

    pub(crate) fn sent(&self) -> Vec<Signal> {
        self.signals.lock().unwrap().clone()
    }
}

impl Processes for FakeProcs {
    fn spawn_group(&self, cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        let mut st = self.state.lock().unwrap();
        st.log = Some(cmd.log_path.clone());
        *self.joined.lock().unwrap() = cmd.cgroup_procs.clone();
        st.strays = self.strays;
        drop(st);
        std::fs::File::create(&cmd.log_path).unwrap();
        Ok(Child {
            pid: 4242,
            group: GroupId {
                pgid: 4242,
                start_time: 7,
            },
        })
    }
    fn signal_group(&self, _g: GroupId, sig: Signal) -> Result<(), PlatformError> {
        if self.gone {
            return Err(PlatformError::Gone { pgid: 4242 });
        }
        self.signals.lock().unwrap().push(sig);
        let mut st = self.state.lock().unwrap();
        match sig {
            Signal::Term => {
                st.termed = true;
                if !self.strays_ignore_term {
                    st.strays = 0;
                }
            }
            Signal::Kill => {
                st.killed = true;
                st.strays = 0;
            }
            _ => {}
        }
        Ok(())
    }
    fn usage(&self, _g: GroupId) -> Result<Usage, PlatformError> {
        let st = self.state.lock().unwrap();
        let leader = u32::from(st.exit.is_none() && !st.reaped);
        Ok(Usage {
            cpu_user_s: self.clock.now().as_secs_f64(),
            cpu_sys_s: 0.0,
            rss_bytes: 1,
            processes: leader + st.strays,
        })
    }
    fn try_wait(&self, c: &Child) -> Result<Option<i32>, PlatformError> {
        let code = self.peek_exit(c)?;
        if code.is_some() {
            self.state.lock().unwrap().reaped = true;
        }
        Ok(code)
    }
    fn peek_exit(&self, _c: &Child) -> Result<Option<i32>, PlatformError> {
        if self.lost_wait {
            return Err(PlatformError::Gone { pgid: 4242 });
        }
        let now = self.clock.now();
        let mut st = self.state.lock().unwrap();
        if st.reaped {
            return Err(PlatformError::Gone { pgid: 4242 });
        }
        if let Some(log) = st.log.clone() {
            let mut out = self.output.lock().unwrap();
            let (due, later): (Vec<_>, Vec<_>) = out.drain(..).partition(|(t, _)| *t <= now);
            *out = later;
            let mut f = std::fs::OpenOptions::new().append(true).open(log).unwrap();
            for (_, b) in due {
                f.write_all(&b).unwrap();
            }
        }
        if st.exit.is_none() {
            st.exit = if st.killed {
                Some(137)
            } else if st.termed && !self.ignores_term {
                Some(143)
            } else {
                self.exit_at.filter(|(t, _)| *t <= now).map(|(_, c)| c)
            };
        }
        Ok(st.exit)
    }
}

/// Records everything the runner reports.
#[derive(Default)]
pub(crate) struct RecEvents {
    pub(crate) started: Vec<Lease>,
    pub(crate) lines: Vec<TimedLine>,
    pub(crate) finished: Vec<Outcome>,
}

impl RunEvents for RecEvents {
    fn started(&mut self, _c: &Child, lease: &Lease) {
        self.started.push(*lease);
    }
    fn lines(&mut self, lines: &[TimedLine]) {
        self.lines.extend_from_slice(lines);
    }
    fn finished(&mut self, outcome: &Outcome) {
        self.finished.push(outcome.clone());
    }
}
