//! Executing API requests against the scheduler and ledger.

use crate::api::{
    ApiError, AuditProblemView, AuditRowView, AuditVerifyView, DEFAULT_AUDIT_ROWS,
    DEFAULT_LIST_ROWS, ErrorCode, EstimateView, HelloInfo, JobGrep, LogSearchQuery, LogView,
    MAX_AUDIT_ROWS, MAX_LIST_ROWS, PROTOCOL_VERSION, Reply, Request, Response,
};
use crate::logs::{self, LogError, Matcher};
use crate::scheduler::Scheduler;
use crate::status::job_view;
use crate::submit::Via;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, Owner, Placement};
use tasks_ledger::{
    AuditEvent, AuditQuery, CancelOutcome, JobRecord, LedgerError, ListFilter, PlanContext,
    RetryPolicy, action, retry_busy,
};
use tasks_policy::{Executables, ExpandOptions, RealFs, Timing};
use tasks_timing::{History, RecipeKey, Shown};

/// Longest cancel reason, in characters.
const MAX_CANCEL_REASON: usize = 500;
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
        | L::IllegalTransition { .. }
        | L::NotQueued { .. } => ErrorCode::Refused,
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
    fn trust_control(&self) -> Result<Arc<dyn crate::trust_control::TrustControl>, ApiError> {
        self.trust().ok_or_else(|| {
            ApiError::new(
                ErrorCode::Refused,
                "this daemon has no identity yet (it was started without the trust runtime)",
            )
        })
    }

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
            Request::Identity => {
                let t = self.trust_control()?;
                Ok(Response::Identity(t.identity()))
            }
            Request::PairInvite {
                roles,
                both_ways,
                ttl_secs,
            } => {
                let t = self.trust_control()?;
                let v = t
                    .invite(&roles, both_ways, ttl_secs)
                    .map_err(|e| ApiError::new(ErrorCode::Refused, e))?;
                Ok(Response::Invite(v))
            }
            Request::PairAccept { ticket } => {
                let t = self.trust_control()?;
                let v = t
                    .accept(&ticket)
                    .map_err(|e| ApiError::new(ErrorCode::Refused, e))?;
                self.wake().notify();
                Ok(Response::Paired(v))
            }
            Request::PairReset => {
                let t = self.trust_control()?;
                let epoch = t
                    .reset()
                    .map_err(|e| ApiError::new(ErrorCode::Refused, e))?;
                Ok(Response::PairReset { epoch })
            }
            Request::Peers => {
                let t = self.trust_control()?;
                t.set_link_quality(self.forwarder.link_quality());
                Ok(Response::Peers { peers: t.peers() })
            }
            Request::PeerRevoke { name } => {
                let t = self.trust_control()?;
                let v = t
                    .revoke(&name)
                    .map_err(|e| ApiError::new(ErrorCode::Refused, e))?;
                self.wake().notify();
                Ok(Response::Revoked(v))
            }
            Request::Submit { spec } => self.submit(spec),
            Request::SubmitPlan {
                plan,
                owner,
                cwd,
                env,
                placement,
                broad_test,
                priority,
                priority_reason,
            } => {
                let ctx = Self::plan_context(&owner, cwd, env, placement, broad_test)?
                    .with_priority(priority, priority_reason)
                    .map_err(|e| ApiError::new(ErrorCode::Refused, format!("invalid plan: {e}")))?;
                self.submit_plan(&plan, ctx)
            }
            Request::SubmitSuite {
                name,
                owner,
                cwd,
                env,
                placement,
                quick,
                executables,
                broad_test,
                priority,
                priority_reason,
            } => {
                let ctx = Self::plan_context(&owner, cwd, env, placement, broad_test)?
                    .with_priority(priority, priority_reason)
                    .map_err(|e| {
                        ApiError::new(ErrorCode::Refused, format!("invalid suite: {e}"))
                    })?;
                self.submit_suite(&name, ctx, quick, &executables)
            }
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
            Request::Cancel { id, who, reason } => self.cancel(id, &who, reason.as_deref()),
            Request::Prioritize {
                id,
                who,
                priority,
                reason,
            } => self.prioritize(id, &who, priority, reason.as_deref()),
            Request::Status => Ok(Response::Status(Box::new(self.status()))),
            Request::JobMetrics { id } => {
                let rec = self.record(id)?;
                Ok(Response::JobMetrics(self.api_job_metrics(&rec)))
            }
            Request::Estimate { argv, kind } => Ok(self.estimate(&argv, kind)),
            Request::Drain { cancel, who } => {
                self.set_drain(!cancel, who);
                Ok(Response::Ack)
            }
            Request::History {
                recipes,
                machines,
                max_samples,
            } => self.history_view(&recipes, &machines, max_samples),
            Request::Audit {
                since_ms,
                until_ms,
                peer,
                job,
                action,
                limit,
            } => self.audit_rows(since_ms, until_ms, peer, job, action, limit),
            Request::AuditVerify => {
                let sink = self.audit_sink_or_err()?;
                let rep = sink.log().verify().map_err(|e| ledger_error(&e))?;
                Ok(Response::AuditVerify(AuditVerifyView {
                    ok: rep.ok(),
                    rows: rep.rows,
                    first_seq: rep.first_seq,
                    last_seq: rep.last_seq,
                    checkpoints: rep.checkpoints,
                    signed_through: rep.signed_through,
                    pruned_before: rep.pruned_before,
                    problem: rep.problem.map(|p| AuditProblemView {
                        seq: p.seq,
                        kind: p.kind.name().to_string(),
                        message: p.message,
                    }),
                }))
            }
            Request::AuditCheckpoint => {
                let sink = self.audit_sink_or_err()?;
                let row = sink.log().checkpoint().map_err(|e| ledger_error(&e))?;
                Ok(Response::AuditCheckpoint {
                    seq: row.map(|r| r.seq),
                })
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

    /// Starts or ends a drain, remembering who asked and when. A second drain keeps the first
    /// requester; a resume forgets it.
    fn set_drain(&self, on: bool, who: Option<String>) {
        let now = self.clock.now_ms();
        if on != self.draining.load(Ordering::SeqCst) {
            let name = who.clone().unwrap_or_else(|| "unknown".to_string());
            self.audit(
                AuditEvent::new(if on {
                    action::QUEUE_DRAIN
                } else {
                    action::QUEUE_RESUME
                })
                .owner(name),
            );
        }
        {
            let mut d = crate::scheduler::lock(&self.drained);
            if !on {
                *d = None;
            } else if d.is_none() || !self.draining.load(Ordering::SeqCst) {
                *d = Some((who.unwrap_or_else(|| "unknown".to_string()), now));
            }
        }
        self.draining.store(on, Ordering::SeqCst);
        self.wake.notify();
    }

    /// The shared submit rules for a spec (R-191, R-210, R-091); a refusal is `Refused` and is
    /// written to the audit log with the rule that refused it.
    /// Applies the submit rules; the warnings of a passing spec come back for the reply.
    fn rules(
        &self,
        spec: &JobSpec,
        what: &str,
        hint: Option<&str>,
    ) -> Result<Vec<String>, ApiError> {
        match self.spec_verdict_hinted(spec, Via::Local, hint) {
            Ok(v) => Ok(v.warnings),
            Err(refusals) => {
                self.audit_refusal(spec, &refusals, Via::Local, None);
                let text = refusals
                    .iter()
                    .map(|r| r.message.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(ApiError::new(ErrorCode::Refused, format!("{what}{text}")))
            }
        }
    }

    fn audit_sink_or_err(&self) -> Result<Arc<crate::audited::AuditSink>, ApiError> {
        self.audit_sink()
            .ok_or_else(|| ApiError::new(ErrorCode::Refused, "this daemon keeps no audit log"))
    }

    fn audit_rows(
        &self,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        peer: Option<String>,
        job: Option<i64>,
        act: Option<String>,
        limit: Option<usize>,
    ) -> Out {
        let limit = limit.unwrap_or(DEFAULT_AUDIT_ROWS);
        if limit == 0 || limit > MAX_AUDIT_ROWS {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("limit must be between 1 and {MAX_AUDIT_ROWS}"),
            ));
        }
        let sink = self.audit_sink_or_err()?;
        let q = AuditQuery {
            since_ms,
            until_ms,
            peer: peer.map(|p| sink.resolve_peer(&p)),
            job,
            action: act.filter(|a| !a.is_empty()),
            limit: limit + 1,
        };
        let mut rows = retry_busy(&API_RETRY, &sleep, || sink.log().query(&q))
            .map_err(|e| ledger_error(&e))?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        let mut names = sink.names();
        // this machine's own rows show its name too
        names.insert(sink.log().actor().to_string(), self.cfg.machine.clone());
        let name_of = |did: &str| names.get(did).cloned();
        let rows = rows
            .into_iter()
            .map(|r| {
                let details = r.details_json();
                AuditRowView {
                    actor_name: name_of(&r.actor),
                    peer_name: r.peer.as_deref().and_then(name_of),
                    seq: r.seq,
                    at_ms: r.at_ms,
                    actor: r.actor,
                    owner: r.owner,
                    action: r.action,
                    job: r.job,
                    peer: r.peer,
                    target: r.target,
                    details,
                    prev_hash: r.prev_hash,
                    hash: r.hash,
                }
            })
            .collect();
        Ok(Response::Audit { rows, truncated })
    }

    /// Writes a job refused by the submit rules to the audit log: the rule ids, never the
    /// command line.
    pub(crate) fn audit_refusal(
        &self,
        spec: &JobSpec,
        refusals: &[tasks_policy::submit_rules::Refusal],
        via: Via,
        actor: Option<String>,
    ) {
        let rules: Vec<&str> = refusals.iter().map(|r| r.rule.id()).collect();
        let mut ev = AuditEvent::new(action::JOB_REFUSED)
            .owner(spec.owner.as_str())
            .target(spec.cwd.display().to_string())
            .detail("rules", rules)
            .detail(
                "via",
                match via {
                    Via::Local => "local",
                    Via::Peer => "peer",
                },
            );
        if let Some(did) = actor {
            ev = ev.by_actor(did.clone()).peer(did);
        }
        self.audit(ev);
    }

    fn submit(&self, spec: JobSpec) -> Out {
        Self::owner(spec.owner.as_str())?;
        spec.validate()
            .map_err(|e| ApiError::new(ErrorCode::Refused, format!("invalid job: {e}")))?;
        let warnings = self.rules(&spec, "", None)?;
        let id = retry_busy(&API_RETRY, &sleep, || self.ledger.submit(&spec))
            .map_err(|e| ledger_error(&e))?;
        self.wake.notify();
        Ok(Response::Submitted { id, warnings })
    }

    /// What a plan's or suite's steps share, with the owner validated.
    fn plan_context(
        owner: &str,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        placement: Placement,
        broad_test: bool,
    ) -> Result<PlanContext, ApiError> {
        Ok(PlanContext {
            owner: Self::owner(owner)?,
            cwd,
            env,
            placement,
            broad_test_escape: broad_test,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
        })
    }

    fn submit_plan(&self, plan: &Plan, ctx: PlanContext) -> Out {
        self.check_plan(&ctx, plan)?;
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
        ctx: PlanContext,
        quick: bool,
        executables: &Executables,
    ) -> Out {
        let cwd = ctx.cwd.clone();
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
        let expansion = tasks_policy::expand(&loaded.policy, name, executables, &opts)
            .map_err(|e| ApiError::new(ErrorCode::Refused, e.to_string()))?;
        self.check_plan(&ctx, &expansion.plan)?;
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

    /// Applies the submit rules to every step of a plan, as the specs the ledger will store.
    fn check_plan(&self, ctx: &PlanContext, plan: &Plan) -> Result<(), ApiError> {
        for step in &plan.steps {
            let spec = JobSpec {
                owner: ctx.owner.clone(),
                argv: step.argv.clone(),
                cwd: ctx.cwd.clone(),
                env: ctx.env.clone(),
                kind: step.kind,
                requirements: step.requirements.clone(),
                placement: ctx.placement.clone(),
                after: Vec::new(),
                timeout_secs: step.hints.timeout_secs,
                broad_test_escape: ctx.broad_test_escape,
                priority: ctx.step_priority(&step.hints).0,
                priority_reason: ctx.step_priority(&step.hints).1,
                fetch: Vec::new(),
            };
            spec.validate().map_err(|e| {
                ApiError::new(
                    ErrorCode::Refused,
                    format!("step {:?}: invalid job: {e}", step.name),
                )
            })?;
            self.rules(
                &spec,
                &format!("step {:?}: ", step.name),
                step.hints.placement.as_deref(),
            )?;
        }
        Ok(())
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
                        end_offset: Some(0),
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
                let (lines, end) = logs::tail_at(&path, lines, !live).map_err(|e| log_error(&e))?;
                Ok(Response::LogTail {
                    lines,
                    running: live,
                    end_offset: Some(end),
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

    fn cancel(&self, id: JobId, who: &str, reason: Option<&str>) -> Out {
        if reason.is_some_and(|r| r.chars().count() > MAX_CANCEL_REASON) {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("the cancel reason is longer than {MAX_CANCEL_REASON} characters"),
            ));
        }
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
        let outcome = retry_busy(&API_RETRY, &sleep, || {
            self.ledger.request_cancel_with(id, who, reason)
        })
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

    /// Changes the class of a job that has not started. Only its owner may (as for a cancel,
    /// R-108); a running or finished job is refused (priority never preempts); `blocking` needs
    /// a reason. The change is an audit row naming who, the old and new class and the reason.
    fn prioritize(
        &self,
        id: JobId,
        who: &str,
        priority: tasks_core::Priority,
        reason: Option<&str>,
    ) -> Out {
        let rec = self.record(id)?;
        if rec.spec.owner.as_str() != who {
            return Err(ApiError::new(
                ErrorCode::NotOwner,
                format!(
                    "job {} belongs to {}; {who} cannot change its priority",
                    id.0,
                    rec.spec.owner.as_str()
                ),
            ));
        }
        let from = retry_busy(&API_RETRY, &sleep, || {
            self.ledger.set_priority(id, priority, reason)
        })
        .map_err(|e| ledger_error(&e))?;
        self.wake.notify();
        Ok(Response::Prioritized {
            id,
            from,
            to: priority,
        })
    }

    /// What may be shown as a command's duration. The request's `kind` only ever picked a
    /// built-in default, which is never shown as an estimate.
    fn estimate(&self, argv: &[String], _kind: Option<tasks_core::Kind>) -> Response {
        let recipe = RecipeKey::history_key(argv, &[])
            .unwrap_or_else(|| argv.first().cloned().unwrap_or_default());
        let others: Vec<_> = self
            .forwarder
            .peers()
            .iter()
            .map(|p| tasks_timing::estimate::MachineInfo {
                name: p.machine.name.0.clone(),
                cores: p.machine.cores,
            })
            .collect();
        let want = self.wanted_cache(&recipe);
        let shown = self.shown_for(&recipe, &self.machine_info(), &others, want);
        Response::Estimate(match &shown {
            Shown::Eta(e) => EstimateView {
                recipe,
                median_secs: Some(e.median_secs),
                p90_secs: Some(e.p90_secs),
                samples: e.samples,
                text: shown.to_string(),
                why_not: None,
                rough: e.rough,
            },
            Shown::Absent(w) => EstimateView {
                recipe,
                median_secs: None,
                p90_secs: None,
                samples: 0,
                text: shown.to_string(),
                why_not: Some(w.to_string()),
                rough: false,
            },
        })
    }
}
