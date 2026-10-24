//! The pure decision core: one ordered walk over the waiters, no I/O and no clock.

use crate::gates::{Readings, check_gates};
use crate::{
    AdmissionInput, Decision, LIGHT_LANE_BASE, Lane, SHORT_LANE_BASE, Verdict, WaitReason, Waiter,
    sequence,
};
use std::collections::BTreeMap;
use tasks_core::{DeviceId, JobId, SlotKind};

/// Decides every waiter. Waiters are walked in [`sequence`] order (class, effective wait,
/// submit time, owners in turn), the single order used for slots, checkouts and positions
/// (R-230). A waiter with holds is skipped without consuming anything, so it never blocks the
/// ones behind it.
pub fn decide(input: &AdmissionInput) -> Vec<Decision> {
    let order: Vec<&Waiter> = sequence(
        &input.waiters,
        input.now_ms,
        input.thresholds.aging_ms,
        &input.last_started,
    )
    .into_iter()
    .filter_map(|i| input.waiters.get(i))
    .collect();
    let mut walk = Walk::new(input);
    order
        .into_iter()
        .map(|w| Decision {
            job: w.job,
            verdict: walk.step(input, w),
        })
        .collect()
}

struct Walk {
    readings: Readings,
    free: BTreeMap<SlotKind, Vec<u32>>,
    short_free: Vec<u32>,
    light_free: Vec<u32>,
    /// Light jobs admitted so far and waiting ahead, for the queue position.
    light_claimed: Vec<JobId>,
    light_ahead: Vec<JobId>,
    claimed: BTreeMap<SlotKind, Vec<JobId>>,
    ahead: BTreeMap<SlotKind, Vec<JobId>>,
    devices_taken: BTreeMap<DeviceId, JobId>,
    checkouts: BTreeMap<String, JobId>,
    /// Jobs admitted so far in this walk that run on this machine.
    started: Vec<JobId>,
    /// An `isolate` job admitted in this walk: nothing behind it starts.
    isolated: Option<JobId>,
    /// The first `isolate` job still waiting: it goes first, so nothing behind it starts.
    isolate_first: Option<JobId>,
}

impl Walk {
    fn new(input: &AdmissionInput) -> Walk {
        let sorted = |v: &[u32]| {
            let mut v = v.to_vec();
            v.sort_unstable();
            v
        };
        let limit = input.thresholds.short_job_lane_slots;
        let mut short_free = sorted(&input.short_lane.free);
        short_free.retain(|i| *i < limit);
        let light_limit = input.thresholds.light_slots;
        let mut light_free = sorted(&input.light_lane.free);
        light_free.retain(|i| *i < light_limit);
        Walk {
            readings: Readings {
                pressure: input.pressure,
                available_memory: input.available_memory,
                cache_free: input.cache_free,
                state_free: input.state_free,
            },
            free: input
                .slots
                .iter()
                .map(|(k, s)| (*k, sorted(&s.free)))
                .collect(),
            short_free,
            light_free,
            light_claimed: Vec::new(),
            light_ahead: Vec::new(),
            claimed: BTreeMap::new(),
            ahead: BTreeMap::new(),
            devices_taken: BTreeMap::new(),
            checkouts: BTreeMap::new(),
            started: Vec::new(),
            isolated: None,
            isolate_first: None,
        }
    }

    fn step(&mut self, input: &AdmissionInput, w: &Waiter) -> Verdict {
        // The first waiter on a checkout, held or not, owns its place in line.
        let pred = w
            .checkout
            .as_ref()
            .and_then(|c| match self.checkouts.get(c) {
                Some(p) => Some(*p),
                None => {
                    self.checkouts.insert(c.clone(), w.job);
                    None
                }
            });
        if let Some(h) = w.holds.first() {
            return Verdict::Wait(WaitReason::Held(h.clone()));
        }
        if let Some(p) = pred {
            return Verdict::Wait(WaitReason::BehindCheckout(p));
        }
        if let Some(r) = self.alone(input, w) {
            if w.isolate {
                self.isolate_first.get_or_insert(w.job);
            }
            return Verdict::Wait(r);
        }
        let verdict = if w.kind == SlotKind::Device {
            self.device(input, w)
        } else if w.light
            && input.thresholds.light_slots > 0
            && matches!(w.kind, SlotKind::Compile | SlotKind::Run)
        {
            self.light(input, w)
        } else {
            self.slot(input, w)
        };
        let local = !matches!(w.kind, SlotKind::Offload | SlotKind::Transfer);
        match &verdict {
            Verdict::Admit { .. } if local => {
                self.started.push(w.job);
                if w.isolate {
                    self.isolated = Some(w.job);
                }
            }
            Verdict::Wait(_) if w.isolate => {
                self.isolate_first.get_or_insert(w.job);
            }
            _ => {}
        }
        verdict
    }

    /// The `isolate` rule (R-712). Nothing starts beside a running or just admitted isolate job;
    /// an isolate job waiting ahead of this one holds everything behind it back, so a stream
    /// of small jobs cannot starve it; an isolate job itself starts only on an idle machine.
    fn alone(&self, input: &AdmissionInput, w: &Waiter) -> Option<WaitReason> {
        if let Some(r) = input.running.iter().find(|r| r.isolate) {
            return Some(WaitReason::RunsAlone(r.job));
        }
        if let Some(j) = self.isolated.or(self.isolate_first) {
            return Some(WaitReason::RunsAlone(j));
        }
        if w.isolate {
            let mut others: Vec<JobId> = input.running.iter().map(|r| r.job).collect();
            others.extend(self.started.iter().copied());
            if !others.is_empty() {
                return Some(WaitReason::WaitsToRunAlone(others));
            }
        }
        None
    }

    fn device(&mut self, input: &AdmissionInput, w: &Waiter) -> Verdict {
        let Some(id) = &w.device else {
            return Verdict::Wait(WaitReason::NoDevice);
        };
        if let Some(h) = self.devices_taken.get(id) {
            return Verdict::Wait(WaitReason::DeviceBusy {
                device: id.clone(),
                holder: Some(*h),
            });
        }
        let Some(st) = input.devices.get(id) else {
            return Verdict::Wait(WaitReason::DeviceUnknown(id.clone()));
        };
        if !st.free {
            return Verdict::Wait(WaitReason::DeviceBusy {
                device: id.clone(),
                holder: st.holder,
            });
        }
        self.devices_taken.insert(id.clone(), w.job);
        Verdict::Admit {
            slot: (SlotKind::Device, st.index),
            lane: Lane::Normal,
        }
    }

    /// A light job takes a place in the light pool, not a compile or run slot. The pool's locks
    /// are run-slot files at [`LIGHT_LANE_BASE`] and up; the run gate (minus its CPU limit)
    /// applies.
    fn light(&mut self, input: &AdmissionInput, w: &Waiter) -> Verdict {
        let t = &input.thresholds;
        let blocked = check_gates(
            SlotKind::Run,
            Lane::Light,
            w.new_output,
            &self.readings,
            t,
        );
        if let Some(r) = blocked
            && !self.light_free.is_empty()
        {
            self.light_ahead.push(w.job);
            return Verdict::Wait(r);
        }
        if !self.light_free.is_empty() {
            let index = self.light_free.remove(0);
            self.light_claimed.push(w.job);
            return Verdict::Admit {
                slot: (SlotKind::Run, LIGHT_LANE_BASE + index),
                lane: Lane::Light,
            };
        }
        let mut holders: Vec<Option<JobId>> = input.light_lane.busy.iter().map(|(_, h)| *h).collect();
        holders.extend(self.light_claimed.iter().map(|j| Some(*j)));
        let reason = WaitReason::LightBusy {
            total: input.light_lane.free.len() + input.light_lane.busy.len(),
            holders,
            ahead: self.light_ahead.clone(),
        };
        self.light_ahead.push(w.job);
        Verdict::Wait(reason)
    }

    fn slot(&mut self, input: &AdmissionInput, w: &Waiter) -> Verdict {
        let t = &input.thresholds;
        let kind = w.kind;
        let blocked = check_gates(kind, Lane::Normal, w.new_output, &self.readings, t);
        let has_free = self.free.get(&kind).is_some_and(|f| !f.is_empty());
        if has_free
            && blocked.is_none()
            && let Some(f) = self.free.get_mut(&kind)
            && !f.is_empty()
        {
            let index = f.remove(0);
            self.claimed.entry(kind).or_default().push(w.job);
            return Verdict::Admit {
                slot: (kind, index),
                lane: Lane::Normal,
            };
        }
        if w.is_short(t)
            && !self.short_free.is_empty()
            && check_gates(kind, Lane::Short, w.new_output, &self.readings, t).is_none()
        {
            let index = self.short_free.remove(0);
            self.claimed.entry(kind).or_default().push(w.job);
            return Verdict::Admit {
                slot: (kind, SHORT_LANE_BASE + index),
                lane: Lane::Short,
            };
        }
        let reason = match blocked {
            Some(r) if has_free => r,
            _ => self.busy(input, kind),
        };
        self.ahead.entry(kind).or_default().push(w.job);
        Verdict::Wait(reason)
    }

    fn busy(&self, input: &AdmissionInput, kind: SlotKind) -> WaitReason {
        let st = input.slots.get(&kind);
        let mut holders: Vec<Option<JobId>> = st
            .map(|s| s.busy.iter().map(|(_, h)| *h).collect())
            .unwrap_or_default();
        holders.extend(
            self.claimed
                .get(&kind)
                .into_iter()
                .flatten()
                .map(|j| Some(*j)),
        );
        WaitReason::SlotsBusy {
            kind,
            total: st.map_or(0, |s| s.free.len() + s.busy.len()),
            holders,
            ahead: self.ahead.get(&kind).cloned().unwrap_or_default(),
        }
    }
}
