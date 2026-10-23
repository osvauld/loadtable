//! The pure decision: which machine, or wait, or refuse.

use crate::error::PlacementError;
use crate::input::{Input, Link, MachineView, Policy, Request};
use std::cmp::Ordering;
use tasks_core::{Kind, MachineName, SlotKind, Unmet};

const MIB: f64 = 1_048_576.0;

/// Why a machine cannot take the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exclusion {
    /// The request does not allow this machine (pin, local default, other peer named).
    NotRequested,
    /// Device jobs stay on the machine that holds the device unless a peer is named.
    DeviceLocalOnly,
    /// Hard requirements the machine fails.
    Unmet(Vec<Unmet>),
    /// The job cannot be shipped.
    NotTransportable(String),
    /// A capability mark (with evidence) rules the machine out.
    Marked(String),
    /// Not reachable.
    Unreachable {
        /// Seconds since last seen.
        last_seen_ago_secs: Option<u64>,
    },
    /// The state snapshot is older than the staleness limit.
    Stale {
        /// Snapshot age.
        age_secs: u64,
        /// The limit.
        limit_secs: u64,
    },
}

impl Exclusion {
    /// True when the machine may become usable later without anything changing in the job.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Exclusion::Unreachable { .. } | Exclusion::Stale { .. }
        )
    }
}

/// How the wait part was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBasis {
    /// A slot is free and nothing is queued.
    Free,
    /// The machine's own expected wait.
    Expected,
    /// Holder ETA unknown; the policy's assumption.
    Unknown,
}

/// A candidate's score, in seconds, broken into its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// Expected wait, pressure included.
    pub wait_secs: f64,
    /// How `wait_secs` was derived.
    pub wait_basis: WaitBasis,
    /// Part of `wait_secs` that is pressure.
    pub pressure_secs: f64,
    /// The machine's admission gate for this job is closed (its own pressure for the local
    /// machine; the compile gate a peer reports).
    pub blocked: bool,
    /// Run time from the estimate.
    pub run_secs: f64,
    /// False when `run_secs` is the policy's assumption.
    pub run_known: bool,
    /// Cost to sync the checkout.
    pub sync_secs: f64,
    /// Local preference bonus, subtracted.
    pub local_bonus_secs: f64,
}

impl Score {
    /// True when the job would start at once: a free slot, nobody ahead and no closed gate. A
    /// machine that is not ready never beats one that is (a peer that holds the job back on its
    /// own gates looks "running" here while this machine sits idle).
    pub fn ready_now(&self) -> bool {
        !self.blocked && self.wait_basis == WaitBasis::Free
    }

    /// Expected finish time: wait + run + sync - local bonus.
    pub fn total_secs(&self) -> f64 {
        self.wait_secs + self.run_secs + self.sync_secs - self.local_bonus_secs
    }
}

/// A machine's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Can run the job, with this score.
    Eligible(Score),
    /// Cannot, for these reasons.
    Excluded(Vec<Exclusion>),
}

/// One machine in the report.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineReport {
    /// Machine name.
    pub name: MachineName,
    /// The local machine.
    pub is_local: bool,
    /// Free slots of the job's slot kind (0 when it needs none).
    pub free_slots: u32,
    /// Pressure summary text.
    pub pressure: String,
    /// Verdict.
    pub verdict: Verdict,
}

/// Why the job is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A named peer is not in the machine list.
    UnknownPeer(MachineName),
    /// The pinned, local or named machine cannot run it (see the machine's exclusions).
    RequestedMachineCannot(MachineName),
    /// No machine can ever run it as asked.
    NoCapableMachine,
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Queue on this machine.
    Run(MachineName),
    /// Capable machines exist but none is usable right now; ask again later.
    Wait,
    /// Nothing will ever run it as asked.
    Refuse(Refusal),
}

/// The decision with every machine's score or reason, sorted by machine name.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// What was asked for.
    pub request: Request,
    /// The outcome.
    pub decision: Decision,
    /// Per machine details.
    pub machines: Vec<MachineReport>,
}

impl Report {
    /// The chosen machine's report, if any.
    pub fn chosen(&self) -> Option<&MachineReport> {
        match &self.decision {
            Decision::Run(n) => self.machines.iter().find(|m| &m.name == n),
            _ => None,
        }
    }

    /// The score of a named machine when it is eligible.
    pub fn score_of(&self, name: &MachineName) -> Option<&Score> {
        self.machines
            .iter()
            .find(|m| &m.name == name)
            .and_then(|m| match &m.verdict {
                Verdict::Eligible(s) => Some(s),
                Verdict::Excluded(_) => None,
            })
    }
}

/// The slot kind a job of this kind waits for, if any.
pub fn slot_for(kind: Kind) -> Option<SlotKind> {
    match kind {
        Kind::Compile | Kind::Test | Kind::Package => Some(SlotKind::Compile),
        Kind::Run => Some(SlotKind::Run),
        Kind::Device => Some(SlotKind::Device),
        Kind::Probe => None,
    }
}

fn exclusions(input: &Input, v: &MachineView, policy: &Policy) -> Vec<Exclusion> {
    let mut out = Vec::new();
    let allowed = match &input.request {
        Request::Local | Request::Pin => v.is_local,
        Request::Peer(n) => &v.machine.name == n,
        Request::Anywhere => true,
    };
    if !allowed {
        out.push(Exclusion::NotRequested);
    } else if input.request == Request::Anywhere && input.kind == Kind::Device && !v.is_local {
        out.push(Exclusion::DeviceLocalOnly);
    }
    let mut req = input.requirements.clone();
    req.tools
        .extend(input.hints.tools.iter().cloned().map(tasks_core::Tool));
    if let Err(u) = req.satisfied_by(&v.machine) {
        out.push(Exclusion::Unmet(u));
    }
    if !v.is_local {
        if let Some(why) = &input.local_only_reason {
            out.push(Exclusion::NotTransportable(why.clone()));
        }
        out.extend(v.marks.iter().cloned().map(Exclusion::Marked));
        match v.link {
            Link::Down { last_seen_ago_secs } => {
                out.push(Exclusion::Unreachable { last_seen_ago_secs })
            }
            Link::Up { age_secs } if age_secs > policy.staleness_limit_secs => {
                out.push(Exclusion::Stale {
                    age_secs,
                    limit_secs: policy.staleness_limit_secs,
                })
            }
            Link::Up { .. } => {}
        }
    }
    out
}

fn free_of(kind: Kind, v: &MachineView) -> u32 {
    slot_for(kind)
        .and_then(|s| v.free_slots.get(&s).copied())
        .unwrap_or(0)
}

fn score(input: &Input, v: &MachineView, policy: &Policy) -> Score {
    let (mut wait_secs, wait_basis) =
        if slot_for(input.kind).is_none() || (free_of(input.kind, v) > 0 && v.queue_depth == 0) {
            (0.0, WaitBasis::Free)
        } else if let Some(w) = v.expected_wait_secs {
            (w, WaitBasis::Expected)
        } else {
            (policy.unknown_wait_secs, WaitBasis::Unknown)
        };
    // a peer reports only its compile gate, so it counts for compile-slot jobs alone
    let blocked =
        v.pressure.blocked && (v.is_local || slot_for(input.kind) == Some(SlotKind::Compile));
    let pressure_secs = if blocked {
        policy.pressure_wait_secs
    } else {
        0.0
    };
    wait_secs += pressure_secs;
    let (run_secs, run_known) = match v.estimate {
        Some(e) => (
            e.median_secs + policy.p90_weight * (e.p90_secs - e.median_secs).max(0.0),
            true,
        ),
        None => (policy.unknown_run_secs, false),
    };
    let sync_secs = if v.is_local || v.checkout.synced {
        0.0
    } else {
        v.checkout.sync_bytes as f64 / MIB * policy.sync_secs_per_mib
    };
    Score {
        wait_secs,
        wait_basis,
        pressure_secs,
        blocked,
        run_secs,
        run_known,
        sync_secs,
        local_bonus_secs: if v.is_local {
            policy.local_bonus_secs
        } else {
            0.0
        },
    }
}

/// Best first: lowest total, then more free slots, then a peer over the local machine
/// (keeps load off the laptop on an exact tie), then name.
fn better(a: &MachineReport, sa: &Score, b: &MachineReport, sb: &Score) -> Ordering {
    sa.total_secs()
        .total_cmp(&sb.total_secs())
        .then(b.free_slots.cmp(&a.free_slots))
        .then(a.is_local.cmp(&b.is_local))
        .then(a.name.cmp(&b.name))
}

/// Decides where the job runs. Pure: the same input always gives the same report.
pub fn decide(input: &Input, policy: &Policy) -> Result<Report, PlacementError> {
    policy.validate()?;
    input.validate()?;
    let mut machines: Vec<MachineReport> = input
        .machines
        .iter()
        .map(|v| {
            let ex = exclusions(input, v, policy);
            MachineReport {
                name: v.machine.name.clone(),
                is_local: v.is_local,
                free_slots: free_of(input.kind, v),
                pressure: v.pressure.summary.clone(),
                verdict: if ex.is_empty() {
                    Verdict::Eligible(score(input, v, policy))
                } else {
                    Verdict::Excluded(ex)
                },
            }
        })
        .collect();
    machines.sort_by(|a, b| a.name.cmp(&b.name));

    // An idle machine beats any machine whose gate is closed, whatever the estimates say: a
    // closed gate has no known end, and the job would sit there looking like it runs.
    let someone_ready = machines
        .iter()
        .any(|m| matches!(&m.verdict, Verdict::Eligible(s) if s.ready_now()));
    let mut best: Option<(&MachineReport, &Score)> = None;
    for m in &machines {
        if let Verdict::Eligible(s) = &m.verdict {
            if someone_ready && s.blocked {
                continue;
            }
            best = match best {
                Some((bm, bs)) if better(bm, bs, m, s) != Ordering::Greater => Some((bm, bs)),
                _ => Some((m, s)),
            };
        }
    }
    let decision = if let Some((m, _)) = best {
        Decision::Run(m.name.clone())
    } else {
        refusal_or_wait(input, &machines)
    };
    Ok(Report {
        request: input.request.clone(),
        decision,
        machines,
    })
}

fn refusal_or_wait(input: &Input, machines: &[MachineReport]) -> Decision {
    match &input.request {
        Request::Local | Request::Pin => machines
            .iter()
            .find(|m| m.is_local)
            .map(|m| Decision::Refuse(Refusal::RequestedMachineCannot(m.name.clone())))
            .unwrap_or(Decision::Refuse(Refusal::NoCapableMachine)),
        Request::Peer(n) => {
            if machines.iter().any(|m| &m.name == n) {
                Decision::Refuse(Refusal::RequestedMachineCannot(n.clone()))
            } else {
                Decision::Refuse(Refusal::UnknownPeer(n.clone()))
            }
        }
        Request::Anywhere => {
            let transient_only = machines.iter().any(|m| {
                matches!(&m.verdict, Verdict::Excluded(ex)
                    if ex.iter().all(Exclusion::is_transient))
            });
            if transient_only {
                Decision::Wait
            } else {
                Decision::Refuse(Refusal::NoCapableMachine)
            }
        }
    }
}
