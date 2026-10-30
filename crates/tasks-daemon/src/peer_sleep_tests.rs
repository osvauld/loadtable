//! A sleeping peer is waited for, and a machine with work stays awake and, on battery, does not
//! take peers' jobs. Names carry the regression ids they pin.

use crate::testkit::{FakeForwarder, Rig};
use std::sync::Arc;
use tasks_core::{Kind, MachineName, Phase, Placement, Tool, WaitReason};
use tasks_ledger::{NewJob, Origin};
use tasks_placement::Link;
use tasks_platform::PowerReading;

const MIN_MS: i64 = 60_000;

fn down(ago: Option<u64>) -> Link {
    Link::Down {
        last_seen_ago_secs: ago,
    }
}

/// A rig with the usual `nixos` peer, whose link the test sets.
fn rig_with_peer(link: Link) -> (Rig, Arc<FakeForwarder>) {
    let mut p = crate::tests::peer_view();
    p.link = link;
    let f = Arc::new(FakeForwarder::new(vec![p]));
    (Rig::with_forwarder(f.clone()), f)
}

fn set_link(f: &FakeForwarder, link: Link) {
    f.peers.lock().unwrap()[0].link = link;
}

fn on_peer(r: &Rig, timeout: Option<u64>) -> tasks_core::JobId {
    let mut s = r.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Peer(MachineName("nixos".into()));
    s.timeout_secs = timeout;
    r.submit(&s)
}

fn wait_text(r: &Rig, id: tasks_core::JobId) -> String {
    match r.phase(id) {
        Phase::Waiting(WaitReason::Placement(t)) => t,
        other => panic!("expected a placement wait, got {other:?}"),
    }
}

#[test]
fn r601_a_job_for_a_sleeping_peer_waits_then_goes_when_it_is_back() {
    let (r, f) = rig_with_peer(down(Some(78)));
    let id = on_peer(&r, None);
    let rep = r.tick();
    assert!(rep.not_started.is_empty() && rep.forwarded.is_empty());
    let t = wait_text(&r, id);
    assert!(
        t.starts_with("waiting for nixos: asleep or unreachable since ")
            && t.contains("gives up at"),
        "{t}"
    );
    // visible where agents look: the row's wait text
    assert_eq!(r.record(id).wait_text.as_deref(), Some(t.as_str()));
    // it holds no slot and runs nothing
    assert_eq!(r.sch.running_count(), 0);
    assert_eq!(r.procs.spawn_count(), 0);
    // still waiting a few minutes later
    r.clock.advance(5 * MIN_MS);
    r.tick();
    assert_eq!(wait_text(&r, id), t);
    // the peer wakes: the job goes to it as if nothing had happened
    set_link(&f, Link::Up { age_secs: 1 });
    assert_eq!(r.tick().forwarded, vec![id]);
    assert_eq!(f.forwarded.lock().unwrap().len(), 1);
}

#[test]
fn r601_a_stale_peer_is_waited_for_too() {
    let (r, f) = rig_with_peer(Link::Up { age_secs: 300 });
    let id = on_peer(&r, None);
    r.tick();
    assert!(wait_text(&r, id).starts_with("waiting for nixos"));
    set_link(&f, Link::Up { age_secs: 2 });
    assert_eq!(r.tick().forwarded, vec![id]);
}

#[test]
fn r602_after_the_grace_the_job_ends_not_started_with_the_reason() {
    let (r, _f) = rig_with_peer(down(Some(78)));
    let id = on_peer(&r, None);
    r.tick();
    r.clock.advance(10 * MIN_MS + 1000);
    assert_eq!(r.tick().not_started, vec![id]);
    let Phase::NotStarted { reason } = r.phase(id) else {
        panic!("not started expected");
    };
    assert!(
        reason.starts_with("nixos stayed unreachable for 10m; last seen "),
        "{reason}"
    );
}

#[test]
fn r602_the_grace_is_a_machine_policy_setting() {
    let mut p = crate::tests::peer_view();
    p.link = down(Some(5));
    let f = Arc::new(FakeForwarder::new(vec![p]));
    let r = Rig::with_policy_forwarder("[placement]\npeer_wait_secs = 30\n", f);
    let id = on_peer(&r, None);
    r.tick();
    r.clock.advance(31_000);
    r.tick();
    let Phase::NotStarted { reason } = r.phase(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("stayed unreachable for 30s"), "{reason}");
}

#[test]
fn r602_the_wait_never_outlasts_the_jobs_own_timeout() {
    let (r, _f) = rig_with_peer(down(Some(78)));
    let id = on_peer(&r, Some(60));
    r.tick();
    assert!(wait_text(&r, id).starts_with("waiting for nixos"));
    r.clock.advance(61_000);
    r.tick();
    let Phase::NotStarted { reason } = r.phase(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("stayed unreachable for 1m"), "{reason}");
}

#[test]
fn r603_a_peer_never_seen_is_refused_at_once_without_the_misleading_wording() {
    let (r, _f) = rig_with_peer(down(None));
    let id = on_peer(&r, None);
    assert_eq!(r.tick().not_started, vec![id]);
    let Phase::NotStarted { reason } = r.phase(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("nixos has not answered"), "{reason}");
    assert!(!reason.contains("nixos cannot run it"), "{reason}");
}

#[test]
fn r604_anywhere_waits_when_the_only_eligible_machine_is_a_sleeping_peer() {
    let mut p = crate::tests::peer_view();
    p.machine.tools.insert(Tool("rare-tool".into()));
    p.link = down(Some(40));
    let f = Arc::new(FakeForwarder::new(vec![p]));
    let r = Rig::with_forwarder(f.clone());
    let mut s = r.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Anywhere;
    s.requirements.tools.insert(Tool("rare-tool".into()));
    let id = r.submit(&s);
    r.tick();
    assert!(wait_text(&r, id).starts_with("waiting for nixos"));
    set_link(&f, Link::Up { age_secs: 1 });
    assert_eq!(r.tick().forwarded, vec![id]);
}

#[test]
fn r604_anywhere_still_picks_another_machine_when_a_peer_sleeps() {
    let (r, _f) = rig_with_peer(down(Some(40)));
    let mut s = r.spec("o", &["make", "x"], Kind::Run);
    s.placement = Placement::Anywhere;
    let id = r.submit(&s);
    assert_eq!(r.tick().started, vec![id]);
}

// ---------------------------------------------------------------- staying awake

#[test]
fn r608_the_machine_is_held_awake_while_it_has_work_and_released_after_the_linger() {
    let r = Rig::new();
    assert_eq!(r.power.held(), 0);
    r.tick();
    assert_eq!(r.power.held(), 0, "an idle daemon lets the machine sleep");
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    assert_eq!(r.power.held(), 1);
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(id);
    r.tick();
    // the job is over; the hold outlasts it
    assert_eq!(r.power.held(), 1);
    r.clock.advance(119_000);
    r.tick();
    assert_eq!(r.power.held(), 1);
    r.clock.advance(2_000);
    r.tick();
    assert_eq!(r.power.held(), 0);
    assert_eq!(r.power.acquired(), 1);
}

#[test]
fn r608_queued_work_holds_the_machine_awake_too() {
    // one run slot taken: the second job only queues
    let r = Rig::with_policy("[slots]\nrun = 1\n");
    let a = r.submit(&r.spec("o", &["make", "a"], Kind::Run));
    let b = r.submit(&r.spec("o", &["make", "b"], Kind::Run));
    r.tick();
    r.until_running(a);
    r.tick();
    let v = r.sch.status().power.unwrap();
    assert!(v.holding);
    assert_eq!(v.awake_jobs, 2, "{v:?}");
    assert_eq!(v.awake_line(), "keeping awake: 2 jobs");
    let _ = b;
}

#[test]
fn r609_status_says_when_sleep_is_allowed_and_when_the_guard_is_lacking() {
    let r = Rig::new();
    r.tick();
    let v = r.sch.status().power.unwrap();
    assert_eq!(v.awake_line(), "sleep allowed");
    assert_eq!(v.source_line(), "power: on AC");
    r.power.refuse_with(Some("not available: no logind"));
    r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    let v = r.sch.status().power.unwrap();
    assert!(
        v.awake_line().starts_with("sleep guard not available:"),
        "{}",
        v.awake_line()
    );
}

#[test]
fn r609_status_reports_the_lingering_hold() {
    let r = Rig::new();
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.procs.auto_exit(0);
    r.tick();
    r.until_done(id);
    r.tick();
    let v = r.sch.status().power.unwrap();
    assert!(
        v.awake_line()
            .starts_with("keeping awake: no jobs, releases in ")
    );
}

#[test]
fn r613_the_hold_can_be_turned_off_in_the_policy() {
    let r = Rig::with_policy("[power]\nkeep_awake = \"off\"\n");
    r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    assert_eq!(r.power.held(), 0);
}

// ---------------------------------------------------------------- battery

fn peer_job(r: &Rig) -> tasks_core::JobId {
    peer_job_keyed(r, "k1")
}

/// A job a peer sent, under its own submission key.
fn peer_job_keyed(r: &Rig, key: &str) -> tasks_core::JobId {
    let mut job = NewJob::new(r.spec("origin-owner", &["make", "x"], Kind::Run));
    job.origin = Some(Origin {
        peer: "laptop-node".into(),
        key: key.into(),
    });
    r.ledger.submit_job(&job).unwrap()
}

#[test]
fn r614_a_job_from_a_peer_waits_while_this_machine_is_on_battery() {
    let r = Rig::with_policy("[power]\npeer_jobs_on_battery = false\n");
    r.power.set_power(PowerReading {
        on_battery: true,
        percent: Some(80),
    });
    let id = peer_job(&r);
    let rep = r.tick();
    assert!(rep.started.is_empty());
    assert_eq!(wait_text(&r, id), "testbox is on battery at 80%");
    assert_eq!(r.procs.spawn_count(), 0);
    // not counted as work, so the machine is not held awake for it (R-615)
    assert_eq!(r.power.held(), 0);
    let v = r.sch.status().power.unwrap();
    assert_eq!(
        (v.on_battery, v.battery_pct, v.battery_held),
        (true, Some(80), 1)
    );
    assert!(
        v.source_line().contains("on battery 80%") && v.source_line().contains("1 job from peers")
    );
    // plugged in: it runs
    r.power.set_power(PowerReading::AC);
    assert_eq!(r.tick().started, vec![id]);
    assert_eq!(r.sch.status().power.unwrap().battery_held, 0);
}

#[test]
fn r614_local_jobs_are_unaffected_by_battery() {
    let r = Rig::new();
    r.power.set_power(PowerReading {
        on_battery: true,
        percent: Some(40),
    });
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    assert_eq!(r.tick().started, vec![id]);
    assert_eq!(r.power.held(), 1);
}

#[test]
fn r614_the_policy_can_let_peer_jobs_run_on_battery() {
    let r =
        Rig::with_policy("[power]\npeer_jobs_on_battery = true\npeer_jobs_min_battery_pct = 0\n");
    r.power.set_power(PowerReading {
        on_battery: true,
        percent: Some(10),
    });
    let id = peer_job(&r);
    assert_eq!(r.tick().started, vec![id]);
}

#[test]
fn r611_this_machines_power_reaches_the_peer_status_and_the_peer_line() {
    use tasks_peer::StatusSource;
    let r = Rig::new();
    r.power.set_power(PowerReading {
        on_battery: true,
        percent: Some(80),
    });
    r.tick();
    let st = crate::peer_link::serve::DaemonStatus::new(r.sch.clone()).status();
    assert_eq!(
        st.power,
        Some(tasks_peer::proto::PowerSummary {
            on_battery: true,
            percent: Some(80),
            holds_peer_jobs: false,
        })
    );
    assert_eq!(crate::status::power_text(true, Some(80)), "on battery 80%");
}

#[test]
fn r613_the_new_settings_have_the_documented_defaults() {
    let p = crate::machine_policy::MachinePolicy::defaults();
    assert_eq!(p.peer_wait_secs(), 600);
    assert!(p.keep_awake());
    assert_eq!(p.keep_awake_linger_secs(), 120);
    // R-834: peer jobs run on battery above 30%
    assert!(p.peer_jobs_on_battery());
    assert_eq!(p.peer_jobs_min_battery_pct(), 30);
    // a toml boolean is accepted for the battery switch
    let p = crate::machine_policy::MachinePolicy::from_text(
        Some("[power]\npeer_jobs_on_battery = false\n"),
        &|_| None,
    );
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    assert!(!p.peer_jobs_on_battery());
}

fn on_battery(pct: Option<u8>) -> PowerReading {
    PowerReading {
        on_battery: true,
        percent: pct,
    }
}

// R-834
#[test]
fn r834_by_default_peer_jobs_run_on_battery_above_thirty_percent_and_wait_at_or_below() {
    let r = Rig::new();
    r.power.set_power(on_battery(Some(31)));
    let a = peer_job(&r);
    assert_eq!(r.tick().started, vec![a]);
    r.power.set_power(on_battery(Some(30)));
    let b = peer_job_keyed(&r, "k2");
    assert!(r.tick().started.is_empty());
    assert_eq!(wait_text(&r, b), "testbox is on battery at 30%");
    let v = r.sch.status().power.unwrap();
    assert!(v.holds_peer_jobs);
    assert!(
        v.source_line()
            .contains("1 job from peers wait for AC or a charge above 30%"),
        "{}",
        v.source_line()
    );
    // charged above the line again: it runs
    r.power.set_power(on_battery(Some(45)));
    assert_eq!(r.tick().started, vec![b]);
}

#[test]
fn r834_an_unknown_charge_counts_as_low_and_zero_turns_the_line_off() {
    let p = crate::machine_policy::MachinePolicy::defaults();
    assert!(p.holds_peer_jobs(Some(on_battery(None))));
    assert!(!p.holds_peer_jobs(Some(PowerReading::AC)));
    assert!(!p.holds_peer_jobs(None));
    let p = crate::machine_policy::MachinePolicy::from_text(
        Some("[power]\npeer_jobs_min_battery_pct = 0\n"),
        &|_| None,
    );
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    assert!(!p.holds_peer_jobs(Some(on_battery(Some(3)))));
    assert!(!p.holds_peer_jobs(Some(on_battery(None))));
}

// R-835
#[test]
fn r835_low_battery_is_said_in_the_status_and_reaches_the_peers() {
    use tasks_peer::StatusSource;
    let r = Rig::new();
    r.power.set_power(on_battery(Some(22)));
    r.tick();
    // before any job waits on it
    let v = r.sch.status().power.unwrap();
    assert!(
        v.source_line()
            .contains("battery low: jobs from peers wait for AC or a charge above 30%"),
        "{}",
        v.source_line()
    );
    let st = crate::peer_link::serve::DaemonStatus::new(r.sch.clone()).status();
    let p = st.power.unwrap();
    assert!(p.holds_peer_jobs && p.percent == Some(22));
}
