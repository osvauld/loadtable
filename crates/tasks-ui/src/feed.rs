//! Background data sources. Each one runs on its own thread, asks the daemon through a
//! [`DaemonLink`] and hands results to the window through a latest-only [`Mailbox`] (R-200).
//! The window never calls the daemon on its own thread.

use crate::link::{DaemonLink, LinkError};
use crate::logview::{self, LogSession, LogSnapshot};
use crate::mailbox::Mailbox;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tasks_daemon::api::{HistoryView, JobView, Request, Response, StatusSnapshot};

/// Everything the dashboard shows, from one refresh.
#[derive(Debug, Clone, PartialEq)]
pub struct Dashboard {
    /// The daemon's status snapshot.
    pub status: StatusSnapshot,
    /// All unfinished jobs plus the newest finished ones.
    pub jobs: Vec<JobView>,
    /// True when the daemon cut the job list at its row limit.
    pub jobs_truncated: bool,
}

/// What a refresh produced: a dashboard, or why there is none.
pub type DashboardResult = Result<Dashboard, String>;

/// Finished jobs a refresh asks for besides the unfinished ones.
pub const RECENT_FINISHED: usize = 40;

/// Calls the daemon for one dashboard refresh: the status snapshot and the job list.
pub fn fetch_dashboard(link: &dyn DaemonLink) -> DashboardResult {
    let status = match link.call(&Request::Status) {
        Ok(Response::Status(s)) => *s,
        Ok(other) => return Err(format!("unexpected reply to status: {}", kind_of(&other))),
        Err(e) => return Err(e.to_string()),
    };
    let (jobs, jobs_truncated) = match link.call(&Request::List {
        owner: None,
        active_only: false,
        recent_finished: Some(RECENT_FINISHED),
        limit: None,
    }) {
        Ok(Response::Jobs { jobs, truncated }) => (jobs, truncated),
        Ok(other) => return Err(format!("unexpected reply to list: {}", kind_of(&other))),
        Err(e) => return Err(e.to_string()),
    };
    Ok(Dashboard {
        status,
        jobs,
        jobs_truncated,
    })
}

/// Asks for the timing history of every recipe.
pub fn fetch_history(link: &dyn DaemonLink) -> Result<HistoryView, String> {
    match link.call(&Request::History {
        recipes: Vec::new(),
        machines: Vec::new(),
        max_samples: None,
    }) {
        Ok(Response::History(h)) => Ok(h),
        Ok(other) => Err(format!("unexpected reply to history: {}", kind_of(&other))),
        Err(e) => Err(e.to_string()),
    }
}

/// A short name for a reply kind, for error text.
pub fn kind_of(r: &Response) -> &'static str {
    match r {
        Response::Ack => "ack",
        Response::Hello(_) => "hello",
        Response::Identity(_) => "identity",
        Response::Invite(_) => "invite",
        Response::Paired(_) => "paired",
        Response::PairReset { .. } => "pair_reset",
        Response::Peers { .. } => "peers",
        Response::Revoked(_) => "revoked",
        Response::Submitted { .. } => "submitted",
        Response::PlanSubmitted { .. } => "plan_submitted",
        Response::Jobs { .. } => "jobs",
        Response::Job { .. } => "job",
        Response::LogTail { .. } => "log_tail",
        Response::LogGrep { .. } => "log_grep",
        Response::LogChunk { .. } => "log_chunk",
        Response::LogSearch { .. } => "log_search",
        Response::Cancelled { .. } => "cancelled",
        Response::Status(_) => "status",
        Response::Estimate(_) => "estimate",
        Response::History(_) => "history",
    }
}

/// Wakes the window so it redraws after a feed delivered something.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// A running feed thread; dropping it asks the thread to stop.
pub struct Feed {
    stop: Arc<AtomicBool>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Sleeps up to `total` in short steps so a stop request is noticed quickly.
fn nap(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(50);
    let mut left = total;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let s = left.min(step);
        std::thread::sleep(s);
        left = left.saturating_sub(s);
    }
}

/// Starts the dashboard refresh loop: fetch, put in `out`, wake the window, wait `every`.
pub fn spawn_dashboard(
    link: Arc<dyn DaemonLink>,
    out: Arc<Mailbox<DashboardResult>>,
    every: Duration,
    wake: Wake,
) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            out.put(fetch_dashboard(link.as_ref()));
            wake();
            nap(every, &flag);
        }
    });
    Feed { stop }
}

/// Runs `job` once on a background thread and hands its result to `out`.
pub fn spawn_once<T: Send + 'static>(
    out: Arc<Mailbox<T>>,
    wake: Wake,
    job: impl FnOnce() -> T + Send + 'static,
) {
    std::thread::spawn(move || {
        out.put(job());
        wake();
    });
}

/// The result of a user action, for the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    /// What was asked, e.g. `cancel #12`.
    pub what: String,
    /// `Ok` with the daemon's word, or the refusal in words.
    pub result: Result<String, String>,
}

/// Cancels `id` with a reason. The daemon accepts a cancel only from the job's owner (R-108),
/// so the request is made as that owner: the dashboard acts for the person at this machine,
/// who owns every job in this machine's ledger.
pub fn cancel_job(
    link: &dyn DaemonLink,
    id: tasks_core::JobId,
    owner: &str,
    reason: &str,
) -> ActionOutcome {
    let reason = reason.trim();
    let what = format!("cancel #{}", id.0);
    let result = match link.call(&Request::Cancel {
        id,
        who: owner.to_string(),
        reason: if reason.is_empty() {
            None
        } else {
            Some(reason.to_string())
        },
    }) {
        Ok(Response::Cancelled { result, .. }) => Ok(result),
        Ok(other) => Err(format!("unexpected reply: {}", kind_of(&other))),
        Err(e) => Err(refusal_text(&e)),
    };
    ActionOutcome { what, result }
}

/// Drains the queue (naming `who`), or resumes it.
pub fn set_drain(link: &dyn DaemonLink, cancel: bool, who: &str) -> ActionOutcome {
    let what = if cancel { "resume" } else { "drain" }.to_string();
    let result = match link.call(&Request::Drain {
        cancel,
        who: Some(who.to_string()),
    }) {
        Ok(_) => Ok("done".to_string()),
        Err(e) => Err(refusal_text(&e)),
    };
    ActionOutcome { what, result }
}

fn refusal_text(e: &LinkError) -> String {
    e.to_string()
}

/// Follows one job's log: tail, then follow while the job lives. Each step puts a
/// [`LogSnapshot`] in `out`, pausing `poll` when caught up. Dropping the returned [`Feed`] stops it.
pub fn spawn_log(
    link: Arc<dyn DaemonLink>,
    job: tasks_core::JobId,
    live: bool,
    poll: Duration,
    out: Arc<Mailbox<LogSnapshot>>,
    wake: Wake,
) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut session = LogSession::new(job, live);
        while !flag.load(Ordering::SeqCst) {
            let Some(req) = session.next_request() else {
                break;
            };
            session.apply(link.call(&req).map_err(|e| e.to_string()));
            out.put(session.snapshot());
            wake();
            if !session.wants_more_now() {
                nap(poll, &flag);
            }
        }
    });
    Feed { stop }
}

/// Searches one job's log once and hands the display lines to `out`.
pub fn search_log(
    link: &dyn DaemonLink,
    job: tasks_core::JobId,
    pattern: &str,
) -> Result<logview::SearchHits, String> {
    match link.call(&logview::search_request(job, pattern)) {
        Ok(Response::LogGrep {
            groups, truncated, ..
        }) => {
            let mut lines = logview::grep_lines(&groups);
            if truncated {
                lines.push((
                    false,
                    format!("... stopped at {} matches", logview::SEARCH_MATCHES),
                ));
            }
            if lines.is_empty() {
                lines.push((false, "no match".to_string()));
            }
            Ok(lines)
        }
        Ok(other) => Err(format!("unexpected reply to search: {}", kind_of(&other))),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::link::FakeLink;
    use tasks_core::JobId;
    use tasks_daemon::api::{ApiError, ErrorCode, LogView};

    fn daemon_fake() -> FakeLink {
        let d = fixtures::busy();
        FakeLink::from_fn(move |req| match req {
            Request::Status => Ok(Response::Status(Box::new(d.status.clone()))),
            Request::List { .. } => Ok(Response::Jobs {
                jobs: d.jobs.clone(),
                truncated: false,
            }),
            Request::Cancel { id, who, .. } if who == "claude@laptop" => Ok(Response::Cancelled {
                id: *id,
                result: "requested".into(),
            }),
            Request::Cancel { .. } => Err(LinkError(
                ApiError::new(ErrorCode::NotOwner, "only the owner may cancel").message,
            )),
            Request::Drain { .. } => Ok(Response::Ack),
            _ => Err(LinkError("unexpected".into())),
        })
    }

    #[test]
    fn a_refresh_asks_for_status_and_the_job_list_with_a_bounded_recent_window() {
        let link = daemon_fake();
        let d = fetch_dashboard(&link).expect("dashboard");
        assert_eq!(d.jobs.len(), fixtures::busy().jobs.len());
        assert_eq!(
            link.requests(),
            vec![
                Request::Status,
                Request::List {
                    owner: None,
                    active_only: false,
                    recent_finished: Some(RECENT_FINISHED),
                    limit: None
                }
            ]
        );
    }

    #[test]
    fn a_failing_daemon_is_an_error_text_not_a_panic() {
        let link = FakeLink::from_fn(|_| Err(LinkError("socket gone".into())));
        assert_eq!(fetch_dashboard(&link), Err("socket gone".to_string()));
        let odd = FakeLink::from_fn(|_| Ok(Response::Ack));
        assert!(
            fetch_dashboard(&odd)
                .unwrap_err()
                .contains("unexpected reply")
        );
    }

    #[test]
    fn cancel_is_confirmed_with_a_reason_and_acts_as_the_jobs_owner() {
        let link = daemon_fake();
        let o = cancel_job(&link, JobId(101), "claude@laptop", "  wrong branch ");
        assert_eq!(o.what, "cancel #101");
        assert_eq!(o.result, Ok("requested".to_string()));
        assert_eq!(
            link.requests(),
            vec![Request::Cancel {
                id: JobId(101),
                who: "claude@laptop".into(),
                reason: Some("wrong branch".into())
            }]
        );
        let o = cancel_job(&link, JobId(101), "claude@laptop", "");
        assert!(matches!(
            link.requests().last(),
            Some(Request::Cancel { reason: None, .. })
        ));
        assert!(o.result.is_ok());
        let refused = cancel_job(&link, JobId(1), "someone@else", "x");
        assert!(refused.result.unwrap_err().contains("owner"));
    }

    #[test]
    fn drain_and_resume_say_who_asked() {
        let link = daemon_fake();
        assert!(set_drain(&link, false, "ui:abe").result.is_ok());
        assert!(set_drain(&link, true, "ui:abe").result.is_ok());
        assert_eq!(
            link.requests(),
            vec![
                Request::Drain {
                    cancel: false,
                    who: Some("ui:abe".into())
                },
                Request::Drain {
                    cancel: true,
                    who: Some("ui:abe".into())
                }
            ]
        );
    }

    #[test]
    fn r200_the_dashboard_feed_runs_off_thread_and_keeps_only_the_newest() {
        let link: Arc<dyn DaemonLink> = Arc::new(daemon_fake());
        let out = Arc::new(Mailbox::new());
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = Arc::clone(&woke);
        let feed = spawn_dashboard(
            link,
            Arc::clone(&out),
            Duration::from_millis(5),
            Arc::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // nobody drains for a while: the box holds one snapshot however many were fetched
        std::thread::sleep(Duration::from_millis(150));
        assert!(out.puts() > 3, "the feed kept refreshing: {}", out.puts());
        assert!(out.dropped() >= out.puts() - 1);
        assert!(matches!(out.take(), Some(Ok(_))));
        assert!(woke.load(Ordering::SeqCst) > 0);
        drop(feed);
        std::thread::sleep(Duration::from_millis(60));
        let after = out.puts();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(out.puts(), after, "dropping the feed stops the thread");
    }

    #[test]
    fn the_log_feed_follows_a_live_job_and_stops_when_it_ends() {
        let link: Arc<dyn DaemonLink> = Arc::new(FakeLink::from_fn(|req| match req {
            Request::Logs {
                view: LogView::Follow { offset, .. },
                ..
            } if *offset == u64::MAX => Ok(Response::LogChunk {
                data: String::new(),
                next_offset: 6,
                done: false,
            }),
            Request::Logs {
                view: LogView::Tail { .. },
                ..
            } => Ok(Response::LogTail {
                lines: vec!["one".into()],
                running: true,
            }),
            Request::Logs {
                view: LogView::Follow { offset, .. },
                ..
            } => Ok(Response::LogChunk {
                data: "two\n".into(),
                next_offset: offset + 4,
                done: true,
            }),
            _ => Err(LinkError("unexpected".into())),
        }));
        let out = Arc::new(Mailbox::new());
        let _feed = spawn_log(
            link,
            JobId(7),
            true,
            Duration::from_millis(5),
            Arc::clone(&out),
            Arc::new(|| {}),
        );
        let mut last = None;
        for _ in 0..200 {
            if let Some(s) = out.take() {
                let done = s.finished;
                last = Some(s);
                if done {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let s = last.expect("a snapshot");
        assert!(s.finished);
        assert_eq!(s.lines, ["one", "two"]);
    }
}
