//! The pure decision core: one ordered walk over the waiters, no I/O and no clock.

use crate::gates::{Readings, check_gates};
use crate::{
    AdmissionInput, Decision, Lane, SHORT_LANE_BASE, Verdict, WaitReason, Waiter, sequence,
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
    claimed: BTreeMap<SlotKind, Vec<JobId>>,
    ahead: BTreeMap<SlotKind, Vec<JobId>>,
    devices_taken: BTreeMap<DeviceId, JobId>,
    checkouts: BTreeMap<String, JobId>,
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
            claimed: BTreeMap::new(),
            ahead: BTreeMap::new(),
            devices_taken: BTreeMap::new(),
            checkouts: BTreeMap::new(),
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
        if w.kind == SlotKind::Device {
            return self.device(input, w);
        }
        self.slot(input, w)
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
