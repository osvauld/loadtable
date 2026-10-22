//! Tests of the pure decision core, including the R-230 deadlock and a property test.

use crate::testkit::{input, waiter};
use crate::*;
use std::collections::{BTreeMap, BTreeSet};
use tasks_core::{DeviceId, JobId, SlotKind};

fn verdicts(i: &AdmissionInput) -> BTreeMap<i64, Verdict> {
    decide(i)
        .into_iter()
        .map(|d| (d.job.0, d.verdict))
        .collect()
}

fn text(v: &Verdict) -> String {
    match v {
        Verdict::Wait(r) => r.to_string(),
        Verdict::Admit { .. } => "admit".into(),
    }
}

fn is_admit(v: &Verdict) -> bool {
    matches!(v, Verdict::Admit { .. })
}

fn same_checkout(job: i64, queued: u64) -> Waiter {
    let mut w = waiter(job, SlotKind::Offload, queued);
    w.checkout = Some("/work/a".into());
    w
}

#[test]
fn r230_gated_waiters_never_deadlock_or_starve_others() {
    // 4272 was noted first, 4271 has the smaller id: the old two orders disagreed.
    let other = {
        let mut w = waiter(4273, SlotKind::Offload, 300);
        w.checkout = Some("/work/b".into());
        w
    };
    let (a, b) = (same_checkout(4271, 200), same_checkout(4272, 100));

    // one free slot: the earliest wins, the same-checkout one is named, 4273 is just queued
    let v = verdicts(&input(1, vec![a.clone(), b.clone(), other.clone()]));
    assert!(is_admit(&v[&4272]));
    assert_eq!(text(&v[&4271]), "behind #4272 (same checkout)");
    assert!(text(&v[&4273]).contains("you are 1st in line"));

    // the front waiter is held: it must not block 4273 from the only slot
    let mut held = b.clone();
    held.holds.push(Hold::SameCheckoutBusy(JobId(4200)));
    let v = verdicts(&input(1, vec![a.clone(), held, other.clone()]));
    assert!(text(&v[&4272]).contains("checkout busy with running #4200"));
    assert_eq!(text(&v[&4271]), "behind #4272 (same checkout)");
    assert!(
        is_admit(&v[&4273]),
        "other checkout starved: {:?}",
        v[&4273]
    );

    // with two slots nobody else is delayed and nobody waits on a later waiter
    let v = verdicts(&input(2, vec![a, b, other]));
    assert!(is_admit(&v[&4272]) && is_admit(&v[&4273]));
    assert_eq!(text(&v[&4271]), "behind #4272 (same checkout)");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn property_free_slot_and_eligible_waiter_means_someone_is_admitted() {
    let kinds = [SlotKind::Compile, SlotKind::Run, SlotKind::Offload];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..2000 {
        let mut waiters = Vec::new();
        for n in 0..rng.next(9) {
            let kind = kinds[rng.next(3) as usize % kinds.len()];
            let mut w = waiter(100 + n as i64, kind, rng.next(5));
            if rng.next(4) == 0 {
                w.priority = Priority::Urgent;
            }
            if rng.next(2) == 0 {
                w.checkout = Some(format!("c{}", rng.next(3)));
            }
            if rng.next(4) == 0 {
                w.holds.push(Hold::Drain);
            }
            waiters.push(w);
        }
        let mut i = input(0, waiters.clone());
        for k in kinds {
            let free = rng.next(3) as u32;
            if let Some(s) = i.slots.get_mut(&k) {
                s.free = (0..free).collect();
            }
        }
        let out = decide(&i);
        let admitted: Vec<_> = out.iter().filter(|d| is_admit(&d.verdict)).collect();
        // sanity: never admit held waiters, never reuse a slot, never exceed free slots
        let mut seen = BTreeSet::new();
        for d in &admitted {
            let w = waiters.iter().find(|w| w.job == d.job).unwrap();
            assert!(w.holds.is_empty(), "case {case}: held waiter admitted");
            let Verdict::Admit { slot, .. } = d.verdict else {
                continue;
            };
            assert!(seen.insert(slot), "case {case}: slot reused");
            assert!(i.slots[&slot.0].free.contains(&slot.1) || slot.1 >= SHORT_LANE_BASE);
        }
        // liveness: the first unheld waiter of a kind with a free slot, with nobody ahead of it
        // on its checkout, forces an admission of that kind
        let mut sorted = waiters.clone();
        sorted.sort_by_key(Waiter::sort_key);
        for k in kinds {
            let free = !i.slots[&k].free.is_empty();
            let eligible = sorted.iter().enumerate().any(|(pos, w)| {
                w.kind == k
                    && w.holds.is_empty()
                    && w.checkout.as_ref().is_none_or(|c| {
                        !sorted
                            .iter()
                            .take(pos)
                            .any(|e| e.checkout.as_ref() == Some(c))
                    })
            });
            if free && eligible {
                let any = admitted
                    .iter()
                    .any(|d| matches!(d.verdict, Verdict::Admit { slot, .. } if slot.0 == k));
                assert!(
                    any,
                    "case {case}: idle {k:?} slot with eligible waiter: {out:?}"
                );
            }
        }
        // acyclic: a waiter is only ever behind an earlier one
        for (pos, d) in out.iter().enumerate() {
            if let Verdict::Wait(WaitReason::BehindCheckout(p)) = &d.verdict {
                assert!(out.iter().take(pos).any(|e| e.job == *p), "case {case}");
            }
        }
    }
}

#[test]
fn slots_busy_names_holders_and_exact_position() {
    let mut i = input(0, vec![]);
    let st = i.slots.get_mut(&SlotKind::Compile).unwrap();
    st.busy = vec![(0, Some(JobId(4278))), (1, Some(JobId(4280)))];
    i.waiters = vec![
        waiter(7, SlotKind::Compile, 1),
        waiter(8, SlotKind::Compile, 2),
        waiter(9, SlotKind::Compile, 3),
    ];
    let v = verdicts(&i);
    assert_eq!(
        text(&v[&7]),
        "all 2 compile slots busy (#4278, #4280); you are 1st in line"
    );
    assert_eq!(
        text(&v[&8]),
        "all 2 compile slots busy (#4278, #4280); you are 2nd in line"
    );
    assert_eq!(
        text(&v[&9]),
        "all 2 compile slots busy (#4278, #4280); you are 3rd in line"
    );
}

#[test]
fn held_waiters_are_skipped_and_not_counted_in_position() {
    let mut i = input(0, vec![]);
    i.slots.get_mut(&SlotKind::Run).unwrap().busy = vec![(0, None)];
    let mut held = waiter(1, SlotKind::Run, 1);
    held.holds.push(Hold::Dependency(JobId(50)));
    i.waiters = vec![held, waiter(2, SlotKind::Run, 2)];
    let v = verdicts(&i);
    assert_eq!(text(&v[&1]), "held: waiting for #50 to finish");
    assert!(text(&v[&2]).contains("external"));
    assert!(text(&v[&2]).contains("1st in line"));
}

#[test]
fn urgent_goes_first_and_keeps_order_among_urgent_r040() {
    let mut u1 = waiter(30, SlotKind::Run, 50);
    let mut u2 = waiter(31, SlotKind::Run, 40);
    u1.priority = Priority::Urgent;
    u2.priority = Priority::Urgent;
    let old = waiter(1, SlotKind::Run, 1);
    let out = decide(&input(1, vec![old, u1, u2]));
    assert_eq!(out.iter().map(|d| d.job.0).collect::<Vec<_>>(), [31, 30, 1]);
    assert!(is_admit(&out[0].verdict));
    assert!(!is_admit(&out[1].verdict));
}

#[test]
fn requeued_job_keeps_its_first_queue_time_r051() {
    // job 9 came back after a withdrawn admission but carries its original queued_at
    let v = verdicts(&input(
        1,
        vec![
            waiter(10, SlotKind::Run, 200),
            waiter(9, SlotKind::Run, 100),
        ],
    ));
    assert!(is_admit(&v[&9]) && !is_admit(&v[&10]));
}

#[test]
fn devices_are_exclusive_and_skip_pressure_r035() {
    let mut i = input(1, vec![]);
    i.pressure.cpu = Some(100.0);
    i.available_memory = 0;
    let id = DeviceId("9BWAY1LSDC".into());
    i.devices.insert(
        id.clone(),
        DeviceState {
            index: 3,
            free: true,
            holder: None,
        },
    );
    let dev = |job, q| {
        let mut w = waiter(job, SlotKind::Device, q);
        w.device = Some(id.clone());
        w
    };
    i.waiters = vec![dev(1, 1), dev(2, 2)];
    let v = verdicts(&i);
    assert_eq!(
        v[&1],
        Verdict::Admit {
            slot: (SlotKind::Device, 3),
            lane: Lane::Normal
        }
    );
    assert_eq!(text(&v[&2]), "device 9BWAY1LSDC held by #1");
    i.devices.get_mut(&id).unwrap().free = false;
    i.devices.get_mut(&id).unwrap().holder = Some(JobId(77));
    i.waiters = vec![dev(3, 1)];
    assert_eq!(text(&verdicts(&i)[&3]), "device 9BWAY1LSDC held by #77");
    i.waiters = vec![waiter(4, SlotKind::Device, 1)];
    assert_eq!(text(&verdicts(&i)[&4]), "device job names no device");
}

#[test]
fn offload_admits_while_compile_and_run_are_full_and_cpu_saturated_r031() {
    let mut i = input(1, vec![]);
    i.slots.get_mut(&SlotKind::Compile).unwrap().free.clear();
    i.slots.get_mut(&SlotKind::Run).unwrap().free.clear();
    i.pressure.cpu = Some(100.0);
    i.pressure.io = Some(100.0);
    i.waiters = vec![
        waiter(1, SlotKind::Offload, 1),
        waiter(2, SlotKind::Compile, 2),
    ];
    let v = verdicts(&i);
    assert!(is_admit(&v[&1]));
    assert!(text(&v[&2]).contains("compile slot"));
}

#[test]
fn short_lane_admits_through_the_cpu_gate_and_is_audited() {
    let mut i = input(1, vec![]);
    i.pressure.cpu = Some(63.0);
    let mut short = waiter(1, SlotKind::Compile, 1);
    short.predicted_secs = Some(30.0);
    let mut long = waiter(2, SlotKind::Compile, 2);
    long.predicted_secs = Some(600.0);
    let none = waiter(3, SlotKind::Compile, 3);
    let mut short2 = waiter(4, SlotKind::Compile, 4);
    short2.predicted_secs = Some(5.0);
    i.waiters = vec![short, long, none, short2];
    let v = verdicts(&i);
    assert_eq!(
        v[&1],
        Verdict::Admit {
            slot: (SlotKind::Compile, SHORT_LANE_BASE),
            lane: Lane::Short
        }
    );
    assert_eq!(
        text(&v[&2]),
        "CPU contended: pressure 63% > 40% for compile"
    );
    assert_eq!(
        text(&v[&3]),
        "CPU contended: pressure 63% > 40% for compile"
    );
    // lane has one slot; the second short job waits for the normal gate
    assert_eq!(
        text(&v[&4]),
        "CPU contended: pressure 63% > 40% for compile"
    );
}

#[test]
fn short_lane_keeps_memory_floor_and_disk_reserve() {
    let mut base = input(1, vec![]);
    base.pressure.cpu = Some(99.0);
    let mut w = waiter(1, SlotKind::Compile, 1);
    w.predicted_secs = Some(1.0);
    w.new_output = true;
    base.waiters = vec![w];
    assert!(is_admit(&verdicts(&base)[&1]));
    let mut low_mem = base.clone();
    low_mem.available_memory = 1 << 30;
    assert!(text(&verdicts(&low_mem)[&1]).starts_with("memory:"));
    let mut low_disk = base.clone();
    low_disk.cache_free = 12 << 30;
    assert_eq!(
        text(&verdicts(&low_disk)[&1]),
        "disk: cache has 12 GiB free, reserve 20 GiB"
    );
}

#[test]
fn short_lane_works_when_normal_slots_are_full() {
    let mut i = input(0, vec![]);
    i.slots.get_mut(&SlotKind::Run).unwrap().busy = vec![(0, Some(JobId(5)))];
    let mut w = waiter(1, SlotKind::Run, 1);
    w.predicted_secs = Some(2.0);
    i.waiters = vec![w];
    assert!(matches!(
        verdicts(&i)[&1],
        Verdict::Admit {
            lane: Lane::Short,
            ..
        }
    ));
    i.thresholds.short_job_lane_slots = 0;
    assert!(!is_admit(&verdicts(&i)[&1]));
}

#[test]
fn disk_wait_ends_by_itself_when_space_returns_r062() {
    let mut i = input(1, vec![]);
    let mut w = waiter(1, SlotKind::Compile, 1);
    w.new_output = true;
    i.waiters = vec![w];
    i.cache_free = 5 << 30;
    let first = verdicts(&i);
    assert!(text(&first[&1]).starts_with("disk: cache has 5 GiB free"));
    i.cache_free = 50 << 30;
    assert!(is_admit(&verdicts(&i)[&1]));
}

#[test]
fn urgent_waiter_cannot_bypass_gates() {
    let mut i = input(1, vec![]);
    i.pressure.cpu = Some(99.0);
    let mut w = waiter(1, SlotKind::Compile, 1);
    w.priority = Priority::Urgent;
    i.waiters = vec![w];
    assert!(!is_admit(&verdicts(&i)[&1]));
}

#[test]
fn empty_queue_has_no_decisions() {
    assert!(decide(&input(1, vec![])).is_empty());
}
