//! Tests of the pure decision core, including the R-230 deadlock and a property test.

use crate::testkit::{input, short_waiter, waiter};
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
            w.priority =
                [Priority::Blocking, Priority::Normal, Priority::Background][rng.next(3) as usize];
            w.owner = format!("o{}", rng.next(3));
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
        let sorted: Vec<Waiter> =
            sequence(&waiters, i.now_ms, i.thresholds.aging_ms, &i.last_started)
                .into_iter()
                .map(|k| waiters[k].clone())
                .collect();
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
fn r338_blocking_goes_first_and_keeps_order_among_blocking_r040() {
    let mut u1 = waiter(30, SlotKind::Run, 50);
    let mut u2 = waiter(31, SlotKind::Run, 40);
    u1.priority = Priority::Blocking;
    u2.priority = Priority::Blocking;
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
fn r342_short_lane_admits_through_the_cpu_gate_and_is_audited() {
    let mut i = input(1, vec![]);
    i.pressure.cpu = Some(63.0);
    let mut short = waiter(1, SlotKind::Compile, 1);
    short.predicted_secs = Some(30.0);
    short.predicted_peak_memory = Some(200 << 20);
    let mut long = waiter(2, SlotKind::Compile, 2);
    long.predicted_secs = Some(600.0);
    long.predicted_peak_memory = Some(200 << 20);
    let none = waiter(3, SlotKind::Compile, 3);
    let mut short2 = waiter(4, SlotKind::Compile, 4);
    short2.predicted_secs = Some(5.0);
    short2.predicted_peak_memory = Some(200 << 20);
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
fn r344_short_lane_keeps_memory_floor_and_disk_reserve() {
    let mut base = input(1, vec![]);
    base.pressure.cpu = Some(99.0);
    let mut w = short_waiter(1, SlotKind::Compile, 1);
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
    let w = short_waiter(1, SlotKind::Run, 1);
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
fn r338_a_blocking_waiter_cannot_bypass_gates() {
    let mut i = input(1, vec![]);
    i.pressure.cpu = Some(99.0);
    let mut w = waiter(1, SlotKind::Compile, 1);
    w.priority = Priority::Blocking;
    i.waiters = vec![w];
    assert!(!is_admit(&verdicts(&i)[&1]));
}

#[test]
fn empty_queue_has_no_decisions() {
    assert!(decide(&input(1, vec![])).is_empty());
}

#[test]
fn r342_the_short_lane_needs_history_for_both_duration_and_peak_memory() {
    let mut i = input(0, vec![]);
    i.slots.get_mut(&SlotKind::Compile).unwrap().busy = vec![(0, Some(JobId(9)))];
    i.pressure.cpu = Some(95.0);
    let ok = short_waiter(1, SlotKind::Compile, 1);
    let mut no_memory = ok.clone();
    no_memory.job = JobId(2);
    no_memory.queued_at_ms = 2;
    no_memory.predicted_peak_memory = None;
    let mut big = ok.clone();
    big.job = JobId(3);
    big.queued_at_ms = 3;
    big.predicted_peak_memory = Some(i.thresholds.short_job_max_memory_bytes + 1);
    let mut no_time = ok.clone();
    no_time.job = JobId(4);
    no_time.queued_at_ms = 4;
    no_time.predicted_secs = None;
    // one waiter at a time, so they do not compete for the lane's single slot
    let lane = |w: &Waiter| {
        let mut i = i.clone();
        i.waiters = vec![w.clone()];
        matches!(
            verdicts(&i)[&w.job.0],
            Verdict::Admit {
                lane: Lane::Short,
                ..
            }
        )
    };
    assert!(lane(&ok));
    assert!(!lane(&no_memory), "memory unknown");
    assert!(!lane(&big), "over the memory cap");
    assert!(!lane(&no_time), "duration unknown");
}

#[test]
fn r338_class_decides_who_gets_the_only_slot() {
    let mut bg = waiter(1, SlotKind::Run, 1);
    bg.priority = Priority::Background;
    let normal = waiter(2, SlotKind::Run, 50);
    let mut blocking = waiter(3, SlotKind::Run, 90);
    blocking.priority = Priority::Blocking;
    let mut i = input(1, vec![bg, normal, blocking]);
    i.now_ms = 100;
    let v = verdicts(&i);
    assert!(is_admit(&v[&3]) && !is_admit(&v[&2]) && !is_admit(&v[&1]));
}

#[test]
fn r340_a_background_job_that_waited_long_enough_beats_a_newer_normal_one() {
    let mut bg = waiter(1, SlotKind::Run, 0);
    bg.priority = Priority::Background;
    let normal = waiter(2, SlotKind::Run, 30 * 60_000);
    let mut i = input(1, vec![bg, normal]);
    // not aged yet: background waits behind the newer normal job
    i.now_ms = 19 * 60_000;
    i.waiters[1].queued_at_ms = 18 * 60_000;
    assert!(is_admit(&verdicts(&i)[&2]));
    // 21 minutes in it has risen to normal at minute 20, and a normal job submitted after that
    // queues behind it
    i.waiters[1].queued_at_ms = 21 * 60_000 - 1000;
    i.now_ms = 21 * 60_000;
    let v = verdicts(&i);
    assert!(is_admit(&v[&1]) && !is_admit(&v[&2]));
}

#[test]
fn r341_one_owners_backlog_does_not_hold_the_only_slot_against_another_owner() {
    let mut ws: Vec<Waiter> = (1..=30)
        .map(|n| {
            let mut w = waiter(n, SlotKind::Run, n as u64);
            w.owner = "busy".into();
            w
        })
        .collect();
    let mut other = waiter(100, SlotKind::Run, 5000);
    other.owner = "quiet".into();
    ws.push(other);
    // busy just started a job; quiet has not
    let mut i = input(1, ws);
    i.last_started.insert("busy".into(), 4000);
    i.now_ms = 6000;
    let v = verdicts(&i);
    assert!(is_admit(&v[&100]));
    assert!((1..=30).all(|n| !is_admit(&v[&n])));
    // with two slots the second goes to busy's oldest job, not its newest
    let mut i2 = i.clone();
    i2.slots.get_mut(&SlotKind::Run).unwrap().free = vec![0, 1];
    let v = verdicts(&i2);
    assert!(is_admit(&v[&100]) && is_admit(&v[&1]) && !is_admit(&v[&2]));
}

fn alone(job: i64, kind: SlotKind, queued: u64) -> Waiter {
    let mut w = waiter(job, kind, queued);
    w.isolate = true;
    w
}

fn running(job: i64, isolate: bool) -> Running {
    Running {
        job: JobId(job),
        isolate,
    }
}

#[test]
fn r712_an_isolate_job_starts_on_an_idle_machine_and_nothing_starts_beside_it() {
    let i = input(
        2,
        vec![
            alone(1, SlotKind::Run, 1),
            waiter(2, SlotKind::Run, 2),
            waiter(3, SlotKind::Compile, 3),
        ],
    );
    let v = verdicts(&i);
    assert!(is_admit(&v[&1]));
    assert_eq!(text(&v[&2]), "job #1 runs alone (isolate)");
    assert_eq!(text(&v[&3]), "job #1 runs alone (isolate)");
}

#[test]
fn r712_an_isolate_job_waits_for_every_running_job_and_names_them() {
    let mut i = input(2, vec![alone(1, SlotKind::Run, 1)]);
    i.running = vec![running(8, false), running(9, false)];
    let v = verdicts(&i);
    assert_eq!(
        text(&v[&1]),
        "runs alone (isolate): waiting for #8, #9 to finish"
    );
    // a job admitted just before it in the same pass counts as running
    let mut i = input(
        2,
        vec![waiter(5, SlotKind::Run, 1), alone(6, SlotKind::Run, 2)],
    );
    i.running = vec![];
    let v = verdicts(&i);
    assert!(is_admit(&v[&5]));
    assert_eq!(
        text(&v[&6]),
        "runs alone (isolate): waiting for #5 to finish"
    );
}

#[test]
fn r712_while_an_isolate_job_runs_no_other_job_starts() {
    let mut i = input(
        3,
        vec![waiter(2, SlotKind::Run, 1), waiter(3, SlotKind::Compile, 2)],
    );
    i.running = vec![running(7, true)];
    let v = verdicts(&i);
    assert_eq!(text(&v[&2]), "job #7 runs alone (isolate)");
    assert_eq!(text(&v[&3]), "job #7 runs alone (isolate)");
}

#[test]
fn r713_an_isolate_job_first_in_line_stops_later_jobs_from_starting() {
    // a job is running, so the isolate job cannot start; the small jobs behind it must not
    // keep the machine busy forever
    let mut i = input(
        3,
        vec![
            alone(1, SlotKind::Run, 1),
            waiter(2, SlotKind::Run, 2),
            waiter(3, SlotKind::Compile, 3),
        ],
    );
    i.running = vec![running(9, false)];
    let v = verdicts(&i);
    assert!(text(&v[&1]).starts_with("runs alone (isolate)"));
    assert_eq!(text(&v[&2]), "job #1 runs alone (isolate)");
    assert_eq!(text(&v[&3]), "job #1 runs alone (isolate)");
}

#[test]
fn r713_jobs_ahead_of_the_isolate_job_still_start_and_a_held_isolate_job_blocks_nobody() {
    let mut i = input(
        3,
        vec![waiter(2, SlotKind::Run, 1), alone(1, SlotKind::Run, 2)],
    );
    i.running = vec![];
    let v = verdicts(&i);
    assert!(is_admit(&v[&2]), "ahead of the isolate job");
    assert!(!is_admit(&v[&1]));
    // a dependency hold skips the isolate job without making the others wait for it
    let mut held = alone(1, SlotKind::Run, 1);
    held.holds.push(Hold::Dependency(JobId(50)));
    let v = verdicts(&input(3, vec![held, waiter(2, SlotKind::Run, 2)]));
    assert!(is_admit(&v[&2]));
}

#[test]
fn r713_offloaded_jobs_neither_wait_for_nor_count_as_the_local_machine() {
    // the offload waiter stands for a job that runs on a peer
    let i = input(
        2,
        vec![waiter(1, SlotKind::Offload, 1), alone(2, SlotKind::Run, 2)],
    );
    let v = verdicts(&i);
    assert!(is_admit(&v[&1]) && is_admit(&v[&2]));
}

fn light(job: i64, kind: SlotKind, queued: u64) -> Waiter {
    let mut w = waiter(job, kind, queued);
    w.light = true;
    w
}

fn slot_of(v: &Verdict) -> (SlotKind, u32, Lane) {
    match v {
        Verdict::Admit { slot, lane } => (slot.0, slot.1, *lane),
        other => panic!("not admitted: {other:?}"),
    }
}

#[test]
fn r714_a_light_job_takes_a_place_in_the_light_pool_and_leaves_the_run_slots_alone() {
    // every compile and run slot is taken, yet the light job starts
    let mut i = input(
        1,
        vec![light(1, SlotKind::Compile, 1), light(2, SlotKind::Run, 2)],
    );
    for k in [SlotKind::Compile, SlotKind::Run] {
        let st = i.slots.get_mut(&k).unwrap();
        st.busy = st.free.drain(..).map(|n| (n, None)).collect();
    }
    let v = verdicts(&i);
    assert_eq!(
        slot_of(&v[&1]),
        (SlotKind::Run, LIGHT_LANE_BASE, Lane::Light)
    );
    assert_eq!(
        slot_of(&v[&2]),
        (SlotKind::Run, LIGHT_LANE_BASE + 1, Lane::Light)
    );
}

#[test]
fn r714_a_light_job_ignores_the_cpu_gate_but_not_the_memory_gate() {
    let mut i = input(
        1,
        vec![light(1, SlotKind::Run, 1), waiter(2, SlotKind::Run, 2)],
    );
    i.pressure.cpu = Some(99.0);
    let v = verdicts(&i);
    assert!(is_admit(&v[&1]), "{:?}", v[&1]);
    assert!(
        text(&v[&2]).starts_with("CPU contended"),
        "ordinary jobs still wait"
    );
    i.pressure.memory = tasks_platform::Level::Critical;
    let v = verdicts(&i);
    assert!(
        text(&v[&1]).starts_with("memory stalls"),
        "{}",
        text(&v[&1])
    );
    i.pressure.memory = tasks_platform::Level::Normal;
    i.available_memory = 0;
    assert!(text(&verdicts(&i)[&1]).starts_with("memory:"));
}

#[test]
fn r715_a_full_light_pool_says_so_and_keeps_its_own_queue() {
    let ws = vec![
        light(1, SlotKind::Run, 1),
        light(2, SlotKind::Run, 2),
        light(3, SlotKind::Run, 3),
        waiter(4, SlotKind::Run, 4),
    ];
    let v = verdicts(&input(1, ws));
    assert!(is_admit(&v[&1]) && is_admit(&v[&2]));
    assert_eq!(
        text(&v[&3]),
        "all 2 light slots busy (#1, #2); you are 1st in line"
    );
    // a light backlog does not take the ordinary run slot from the job behind it
    assert!(is_admit(&v[&4]));
}

#[test]
fn r715_with_the_pool_switched_off_a_light_job_is_an_ordinary_one() {
    let mut i = input(1, vec![light(1, SlotKind::Run, 1)]);
    i.thresholds.light_slots = 0;
    i.light_lane = SlotState::default();
    let v = verdicts(&i);
    assert_eq!(slot_of(&v[&1]), (SlotKind::Run, 0, Lane::Normal));
}
