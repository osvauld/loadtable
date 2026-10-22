//! Executing API requests against the scheduler and ledger.

use crate::api::{
    ApiError, DEFAULT_LIST_ROWS, ErrorCode, EstimateView, HelloInfo, JobGrep, LogSearchQuery,
    LogView, MAX_LIST_ROWS, PROTOCOL_VERSION, Reply, Request, Response,
};
use crate::logs::{self, LogError, Matcher};
use crate::scheduler::Scheduler;
use crate::status::job_view;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, Owner, Placement};
use tasks_ledger::{
    CancelOutcome, JobRecord, LedgerError, ListFilter, PlanContext, RetryPolicy, retry_busy,
};
use tasks_policy::{Executables, ExpandOptions, RealFs, Timing};
use tasks_timing::{History, RecipeKey};

const DEFAULT_GREP_MATCHES: usize = 200;
const MAX_GREP_MATCHES: usize = 5000;
const DEFAULT_GREP_BYTES: usize = 256 * 1024;
const MAX_GREP_BYTES: usize = 2 << 20;
/// Ledger waits in API handlers stay below the client's per-attempt timeout (R-102).
const API_RETRY: RetryPolicy = RetryPolicy {
    deadline: Duration::from_secs(30),
    initial: Duration::from_millis(50),
    max_pause: Duration::from_secs(2),
};
/// Finished jobs a cross-job search looks through, besides every unfinished job.
const SEARCH_RECENT_JOBS: usize = 500;

fn ledger_error(e: &LedgerError) -> ApiError {
    use LedgerError as L;
    let code = match e {
        L::NotFound(_) => ErrorCode::NotFound,
        L::Invalid(_)
        | L::Plan(_)
        | L::UnknownDependency(_)
        | L::DependencyNotPassable(_)
        | L::IllegalTransition { .. } => ErrorCode::Refused,
        L::Unavailable { .. } => ErrorCode::LedgerUnavailable,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, e.to_string())
}

fn log_error(e: &LogError) -> ApiError {
    match e {
        LogError::Bad(m) => ApiError::new(ErrorCode::BadRequest, m.clone()),
        LogError::Io { .. } => ApiError::new(ErrorCode::NotFound, e.to_string()),
    }
}

fn sleep(d: Duration) {
    std::thread::sleep(d);
}

type Out = Result<Response, ApiError>;

impl Scheduler {
    /// Answers one request. Never panics and never blocks on a running job.
    pub fn handle(self: &Arc<Self>, req: Request) -> Reply {
        match self.dispatch(req) {
            Ok(r) => Reply::Ok(r),
            Err(e) => Reply::Err(e),
        }
    }

    fn dispatch(self: &Arc<Self>, req: Request) -> Out {
        match req {
            Request::Hello => Ok(Response::Hello(HelloInfo {
                protocol: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                state_dir: self.cfg.state_dir.clone(),
                lock_dir: self.cfg.lock_dir.clone(),
                machine: self.cfg.machine.clone(),
            })),
            Request::Submit { spec } => self.submit(spec),
            Request::SubmitPlan {
                plan,
                owner,
                cwd,
                env,
                placement,
            } => self.submit_plan(&plan, &owner, cwd, env, placement),
            Request::SubmitSuite {
                name,
                owner,
                cwd,
                env,
                placement,
                quick,
            } => self.submit_suite(&name, &owner, cwd, env, placement, quick),
            Request::List {
                owner,
                active_only,
                recent_finished,
                limit,
            } => self.list(owner, active_only, recent_finished, limit),
            Request::Get { id } => {
                let rec = self.record(id)?;
                Ok(Response::Job {
                    job: Box::new(job_view(&rec)),
                })
            }
            Request::Logs { id, view } => self.logs(id, view),
            Request::LogSearch { query } => self.log_search(&query),
            Request::Cancel { id, who } => self.cancel(id, &who),
            Request::Status => Ok(Response::Status(Box::new(self.status()))),
            Request::Estimate { argv, kind } => Ok(self.estimate(&argv, kind)),
            Request::Drain { cancel } => {
                self.draining.store(!cancel, Ordering::SeqCst);
                self.wake.notify();
                Ok(Response::Ack)
            }
            Request::Execution { enabled } => {
                std::fs::write(
                    self.cfg.intent_path(),
                    if enabled { "enabled\n" } else { "stopped\n" },
                )
                .map_err(|e| ApiError::new(ErrorCode::Internal, format!("cannot persist: {e}")))?;
                self.execution_enabled.store(enabled, Ordering::SeqCst);
                self.wake.notify();
                Ok(Response::Ack)
            }
            Request::Shutdown => {
                let n = self.running_count();
                if n > 0 {
                    return Err(ApiError::new(
                        ErrorCode::Busy,
                        format!("{n} job(s) are running; wait for them or cancel them first"),
                    ));
                }
                self.shutdown.store(true, Ordering::SeqCst);
                self.wake.notify();
                Ok(Response::Ack)
            }
        }
    }

    fn record(&self, id: JobId) -> Result<JobRecord, ApiError> {
        retry_busy(&API_RETRY, &sleep, || self.ledger.get(id))
            .map_err(|e| ledger_error(&e))?
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no such job {}", id.0)))
    }

    fn owner(name: &str) -> Result<Owner, ApiError> {
        Owner::new(name).map_err(|e| ApiError::new(ErrorCode::Refused, format!("owner: {e}")))
    }

    fn submit(&self, spec: JobSpec) -> Out {
        Self::owner(spec.owner.as_str())?;
        spec.validate()
            .map_err(|e| ApiError::new(ErrorCode::Refused, format!("invalid job: {e}")))?;
        let id = retry_busy(&API_RETRY, &sleep, || self.ledger.submit(&spec))
            .map_err(|e| ledger_error(&e))?;
        self.wake.notify();
        Ok(Response::Submitted { id })
    }

    fn submit_plan(
        &self,
        plan: &Plan,
        owner: &str,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        placement: Placement,
    ) -> Out {
        let ctx = PlanContext {
            owner: Self::owner(owner)?,
            cwd,
            env,
            placement,
        };
        let done = retry_busy(&API_RETRY, &sleep, || self.ledger.submit_plan(&ctx, plan))
            .map_err(|e| ledger_error(&e))?;
        self.wake.notify();
        Ok(Response::PlanSubmitted {
            plan_id: done.plan_id,
            jobs: done.jobs,
        })
    }

    fn submit_suite(
        &self,
        name: &str,
        owner: &str,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        placement: Placement,
        quick: bool,
    ) -> Out {
        let ctx = PlanContext {
            owner: Self::owner(owner)?,
            cwd: cwd.clone(),
            env,
            placement,
        };
        let loaded = tasks_policy::load(&RealFs, &cwd)
            .map_err(|e| ApiError::new(ErrorCode::Refused, e.to_string()))?;
        let machines = vec![self.cfg.machine.clone()];
        let history: &dyn History = &*self.ledger;
        let opts = ExpandOptions {
            quick,
            timing: Some(Timing {
                history,
                machines: &machines,
            }),
        };
        let expansion = tasks_policy::expand(&loaded.policy, name, &Executables::new(), &opts)
            .map_err(|e| ApiError::new(ErrorCode::Refused, e.to_string()))?;
        let done = retry_busy(&API_RETRY, &sleep, || {
            self.ledger.submit_plan(&ctx, &expansion.plan)
        })
        .map_err(|e| ledger_error(&e))?;
        self.wake.notify();
        Ok(Response::PlanSubmitted {
            plan_id: done.plan_id,
            jobs: done.jobs,
        })
    }

    fn list(
        &self,
        owner: Option<String>,
        active_only: bool,
        recent_finished: Option<usize>,
        limit: Option<usize>,
    ) -> Out {
        let limit = limit.unwrap_or(DEFAULT_LIST_ROWS);
        if limit == 0 || limit > MAX_LIST_ROWS {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("limit must be between 1 and {MAX_LIST_ROWS}"),
            ));
        }
        let filter = ListFilter {
            owner,
            active_only,
            id: None,
            recent_finished,
            limit: Some(limit + 1),
        };
        let mut rows = retry_busy(&API_RETRY, &sleep, || self.ledger.list(&filter))
            .map_err(|e| ledger_error(&e))?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        Ok(Response::Jobs {
            jobs: rows.iter().map(job_view).collect(),
            truncated,
        })
    }

    fn log_file(&self, rec: &JobRecord) -> PathBuf {
        rec.log_path
            .clone()
            .unwrap_or_else(|| self.cfg.log_path(rec.id))
    }

    fn logs(&self, id: JobId, view: LogView) -> Out {
        let rec = self.record(id)?;
        let path = self.log_file(&rec);
        let live = !rec.phase.is_terminal();
        if !path.exists() {
            // a job that has not started has no log yet; a finished one lost it
            return if live {
                Ok(match view {
                    LogView::Tail { .. } => Response::LogTail {
                        lines: Vec::new(),
                        running: true,
                    },
                    LogView::Grep { .. } => Response::LogGrep {
                        groups: Vec::new(),
                        matches: 0,
                        truncated: false,
                    },
                    LogView::Follow { offset, .. } => Response::LogChunk {
                        data: String::new(),
                        next_offset: offset,
                        done: false,
                    },
                })
            } else {
                Err(ApiError::new(
                    ErrorCode::NotFound,
                    format!("job {} has no log", id.0),
                ))
            };
        }
        match view {
            LogView::Tail { lines } => {
                let lines = logs::tail(&path, lines, !live).map_err(|e| log_error(&e))?;
                Ok(Response::LogTail {
                    lines,
                    running: live,
                })
            }
            LogView::Grep {
                pattern,
                context,
                regex,
                max_matches,
            } => {
                let m = Matcher::new(&pattern, regex).map_err(|e| log_error(&e))?;
                let max = max_matches
                    .unwrap_or(DEFAULT_GREP_MATCHES)
                    .clamp(1, MAX_GREP_MATCHES);
                let out = logs::grep_file(&path, &m, context, max, DEFAULT_GREP_BYTES)
                    .map_err(|e| log_error(&e))?;
                Ok(Response::LogGrep {
                    groups: out.groups,
                    matches: out.matches,
                    truncated: out.truncated,
                })
            }
            LogView::Follow { offset, max_bytes } => {
                let (data, next_offset, eof) =
                    logs::follow(&path, offset, max_bytes).map_err(|e| log_error(&e))?;
                Ok(Response::LogChunk {
                    data,
                    next_offset,
                    done: eof && !live,
                })
            }
        }
    }

    fn log_search(&self, q: &LogSearchQuery) -> Out {
        let matcher = Matcher::new(&q.pattern, q.regex).map_err(|e| log_error(&e))?;
        logs::check_context(q.context).map_err(|e| log_error(&e))?;
        let now = self.clock.now_ms();
        let since = match &q.since {
            Some(t) => Some(logs::parse_time(t, now).map_err(|e| log_error(&e))?),
            None => None,
        };
        let until = match &q.until {
            Some(t) => Some(logs::parse_time(t, now).map_err(|e| log_error(&e))?),
            None => None,
        };
        let max_matches = q
            .max_matches
            .unwrap_or(DEFAULT_GREP_MATCHES)
            .clamp(1, MAX_GREP_MATCHES);
        let max_bytes = q
            .max_bytes
            .unwrap_or(DEFAULT_GREP_BYTES)
            .clamp(1, MAX_GREP_BYTES);
        let filter = ListFilter {
            owner: q.owner.clone(),
            recent_finished: Some(SEARCH_RECENT_JOBS),
            ..ListFilter::default()
        };
        let mut rows = retry_busy(&API_RETRY, &sleep, || self.ledger.list(&filter))
            .map_err(|e| ledger_error(&e))?;
        rows.retain(|r| {
            (q.ids.is_empty() || q.ids.contains(&r.id))
                && q.phase
                    .as_deref()
                    .is_none_or(|p| logs::phase_tag(&r.phase) == p)
                && since.is_none_or(|s| r.submitted_at_ms >= s)
                && until.is_none_or(|u| r.submitted_at_ms <= u)
        });
        rows.sort_by_key(|r| std::cmp::Reverse(r.id));
        let mut results = Vec::new();
        let mut matches = 0usize;
        let mut bytes = 0usize;
        let mut truncated = false;
        for r in rows {
            let path = self.log_file(&r);
            if !path.exists() {
                continue;
            }
            let left = max_matches - matches;
            if left == 0 || bytes >= max_bytes {
                truncated = true;
                break;
            }
            let out = logs::grep_file(&path, &matcher, q.context, left, max_bytes - bytes)
                .map_err(|e| log_error(&e))?;
            matches += out.matches;
            bytes += out.bytes;
            truncated |= out.truncated;
            if out.matches > 0 {
                results.push(JobGrep {
                    id: r.id,
                    owner: r.spec.owner.as_str().to_string(),
                    phase: logs::phase_tag(&r.phase).to_string(),
                    groups: out.groups,
                });
            }
        }
        Ok(Response::LogSearch {
            results,
            matches,
            truncated,
        })
    }

    fn cancel(&self, id: JobId, who: &str) -> Out {
        let rec = self.record(id)?;
        // R-108: only the owner cancels
        if rec.spec.owner.as_str() != who {
            return Err(ApiError::new(
                ErrorCode::NotOwner,
                format!(
                    "job {} belongs to {}; {who} cannot cancel it",
                    id.0,
                    rec.spec.owner.as_str()
                ),
            ));
        }
        let outcome = retry_busy(&API_RETRY, &sleep, || self.ledger.request_cancel(id, who))
            .map_err(|e| ledger_error(&e))?;
        let result = match outcome {
            CancelOutcome::Cancelled => "cancelled",
            CancelOutcome::Requested => {
                self.stop_running(id);
                "requested"
            }
            CancelOutcome::AlreadyFinished(_) => "already_finished",
        };
        self.wake.notify();
        Ok(Response::Cancelled {
            id,
            result: result.to_string(),
        })
    }

    fn estimate(&self, argv: &[String], kind: Option<tasks_core::Kind>) -> Response {
        let keys = RecipeKey::from_argv(argv, &[]);
        let recipe = keys.first().map_or_else(
            || argv.first().cloned().unwrap_or_default(),
            ToString::to_string,
        );
        let others: Vec<_> = self
            .forwarder
            .peers()
            .iter()
            .map(|p| tasks_timing::estimate::MachineInfo {
                name: p.machine.name.0.clone(),
                cores: p.machine.cores,
            })
            .collect();
        match self.estimate_for(&recipe, &self.machine_info(), &others) {
            Some(mut e) => {
                if e.samples == 0
                    && let Some(k) = kind
                {
                    let (median, p90) = tasks_timing::estimate::kind_default(k);
                    e.median_secs = median;
                    e.p90_secs = p90;
                }
                Response::Estimate(EstimateView {
                    recipe,
                    median_secs: e.median_secs,
                    p90_secs: e.p90_secs,
                    samples: e.samples,
                    text: e.to_string(),
                })
            }
            None => Response::Estimate(EstimateView {
                recipe,
                median_secs: 0.0,
                p90_secs: 0.0,
                samples: 0,
                text: "no estimate: the timing history could not be read".to_string(),
            }),
        }
    }
}
