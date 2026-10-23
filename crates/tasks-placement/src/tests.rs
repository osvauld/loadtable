//! Rule tests. Names carry the regression ids they pin (see docs/REGRESSIONS.md).

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use tasks_core::{Arch, DeviceId, Kind, Machine, MachineName, Os, SlotKind, StepHints, Tool};

fn policy() -> Policy {
    Policy {
        staleness_limit_secs: 60,
        sync_secs_per_mib: 2.0,
        local_bonus_secs: 30.0,
        unknown_run_secs: 120.0,
        unknown_wait_secs: 300.0,
        pressure_wait_secs: 60.0,
        p90_weight: 0.0,
        reroute_margin_secs: 60.0,
        reroute_cooldown_secs: 30,
        max_reroutes: 2,
    }
}

fn n(s: &str) -> MachineName {
    MachineName(s.into())
}

fn view(name: &str, local: bool) -> MachineView {
    MachineView {
        machine: Machine {
            name: n(name),
            os: Os::Linux,
            arch: Arch::X86_64,
            cores: 8,
            memory_bytes: 16 << 30,
            tools: [Tool("cargo".into())].into(),
            devices: BTreeSet::new(),
        },
        is_local: local,
        link: Link::Up { age_secs: 1 },
        free_slots: BTreeMap::from([(SlotKind::Compile, 1)]),
        queue_depth: 0,
        expected_wait_secs: None,
        pressure: Pressure::default(),
        estimate: None,
        checkout: Checkout {
            synced: !local,
            sync_bytes: 0,
        },
        marks: vec![],
    }
}

fn busy(v: &mut MachineView, wait: Option<f64>) {
    v.free_slots.insert(SlotKind::Compile, 0);
    v.queue_depth = 1;
    v.expected_wait_secs = wait;
}

fn est(median: f64) -> Option<RunEstimate> {
    Some(RunEstimate {
        median_secs: median,
        p90_secs: median,
        samples: 5,
    })
}

fn input(request: Request, machines: Vec<MachineView>) -> Input {
    Input {
        kind: Kind::Test,
        requirements: Requirements::default(),
        hints: StepHints::default(),
        request,
        local_only_reason: None,
        local_cargo_jobs: None,
        machines,
    }
}

use tasks_core::Requirements;

fn run(i: &Input) -> Report {
    decide(i, &policy()).unwrap()
}

fn chosen(r: &Report) -> Option<String> {
    match &r.decision {
        Decision::Run(m) => Some(m.0.clone()),
        _ => None,
    }
}

fn exclusions_of<'a>(r: &'a Report, name: &str) -> &'a [Exclusion] {
    match &r
        .machines
        .iter()
        .find(|m| m.name.0 == name)
        .unwrap()
        .verdict
    {
        Verdict::Excluded(e) => e,
        Verdict::Eligible(_) => &[],
    }
}

// ---- R-132 ----

#[test]
fn r132_device_jobs_are_always_local_unless_a_peer_is_named() {
    let mut local = view("laptop", true);
    local.machine.devices.insert(DeviceId("d".into()));
    let mut peer = view("nixos", false);
    peer.machine.devices.insert(DeviceId("d".into()));
    busy(&mut local, Some(900.0));
    let mut i = input(Request::Anywhere, vec![local, peer]);
    i.kind = Kind::Device;
    i.requirements.device = Some(DeviceId("d".into()));
    let r = run(&i);
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert_eq!(exclusions_of(&r, "nixos"), &[Exclusion::DeviceLocalOnly]);
    i.request = Request::Peer(n("nixos"));
    assert_eq!(chosen(&run(&i)).as_deref(), Some("nixos"));
}

#[test]
fn r132_run_class_stays_local_by_default_even_when_a_peer_is_idle() {
    let mut local = view("laptop", true);
    busy(&mut local, Some(900.0));
    let i = input(Request::Local, vec![local, view("nixos", false)]);
    let r = run(&i);
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert_eq!(exclusions_of(&r, "nixos"), &[Exclusion::NotRequested]);
}

#[test]
fn r132_no_peers_means_local() {
    let r = run(&input(Request::Anywhere, vec![view("laptop", true)]));
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
}

// ---- R-133 ----

#[test]
fn r133_offline_and_stale_peers_are_skipped_and_fall_back_to_local() {
    let mut off = view("a", false);
    off.link = Link::Down {
        last_seen_ago_secs: Some(500),
    };
    let mut stale = view("b", false);
    stale.link = Link::Up { age_secs: 61 };
    let mut local = view("laptop", true);
    busy(&mut local, Some(5000.0));
    let r = run(&input(Request::Anywhere, vec![off, stale, local]));
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert!(matches!(
        exclusions_of(&r, "a"),
        [Exclusion::Unreachable { .. }]
    ));
    assert_eq!(
        exclusions_of(&r, "b"),
        &[Exclusion::Stale {
            age_secs: 61,
            limit_secs: 60
        }]
    );
}

#[test]
fn r133_a_snapshot_exactly_at_the_limit_is_not_stale() {
    let mut p = view("nixos", false);
    p.link = Link::Up { age_secs: 60 };
    p.estimate = est(10.0);
    let mut l = view("laptop", true);
    busy(&mut l, Some(1000.0));
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("nixos")
    );
}

#[test]
fn r133_a_named_peer_that_is_down_is_a_clean_refusal_with_the_reason() {
    let mut p = view("nixos", false);
    p.link = Link::Down {
        last_seen_ago_secs: None,
    };
    let r = run(&input(
        Request::Peer(n("nixos")),
        vec![view("laptop", true), p],
    ));
    assert_eq!(
        r.decision,
        Decision::Refuse(Refusal::RequestedMachineCannot(n("nixos")))
    );
    assert!(matches!(
        exclusions_of(&r, "nixos"),
        [Exclusion::Unreachable { .. }]
    ));
}

#[test]
fn r133_anywhere_waits_when_the_only_capable_machine_is_down() {
    let mut local = view("laptop", true);
    local.machine.os = Os::Macos;
    let mut p = view("nixos", false);
    p.link = Link::Down {
        last_seen_ago_secs: Some(9),
    };
    let mut i = input(Request::Anywhere, vec![local, p]);
    i.requirements.os = Some(Os::Linux);
    assert_eq!(run(&i).decision, Decision::Wait);
}

// ---- R-134 ----

#[test]
fn r134_a_blocked_machine_counts_as_extra_wait_not_skipped() {
    let mut l = view("laptop", true);
    l.pressure = Pressure {
        blocked: true,
        summary: "memory PSI".into(),
    };
    let r = run(&input(Request::Anywhere, vec![l]));
    let s = r.score_of(&n("laptop")).unwrap();
    assert_eq!(s.wait_secs, 60.0);
    assert_eq!(s.pressure_secs, 60.0);
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
}

// ---- R-348 ----

fn faster(v: &mut MachineView, secs: f64) {
    v.estimate = est(secs);
}

fn gate_closed(v: &mut MachineView) {
    v.pressure = Pressure {
        blocked: true,
        summary: "CPU contended: pressure 63% > 40% for compile".into(),
    };
}

#[test]
fn r348_a_peer_with_a_closed_gate_does_not_win_over_an_idle_local_machine() {
    // the peer is much faster on paper (30 s against 200 s); only its gate is closed. The old
    // arithmetic (60 s of pressure) still picked it, and the job sat there looking like it ran
    let mut local = view("laptop", true);
    faster(&mut local, 200.0);
    let mut peer = view("nixos", false);
    faster(&mut peer, 30.0);
    gate_closed(&mut peer);
    let r = run(&input(Request::Anywhere, vec![local, peer]));
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    // it is still scored, with the reason, so the report explains the choice
    let s = r.score_of(&n("nixos")).unwrap();
    assert!(s.blocked && s.pressure_secs == 60.0);
    assert!(r.one_line().contains("laptop"), "{}", r.one_line());
}

#[test]
fn r348_the_same_peer_wins_when_its_gate_is_open() {
    let mut local = view("laptop", true);
    faster(&mut local, 200.0);
    let mut peer = view("nixos", false);
    faster(&mut peer, 30.0);
    let r = run(&input(Request::Anywhere, vec![local, peer]));
    assert_eq!(chosen(&r).as_deref(), Some("nixos"));
}

#[test]
fn r348_a_blocked_peer_still_wins_when_the_local_machine_is_not_idle() {
    let mut local = view("laptop", true);
    faster(&mut local, 200.0);
    busy(&mut local, Some(900.0));
    let mut peer = view("nixos", false);
    faster(&mut peer, 30.0);
    gate_closed(&mut peer);
    let r = run(&input(Request::Anywhere, vec![local, peer]));
    assert_eq!(chosen(&r).as_deref(), Some("nixos"));
}

#[test]
fn r348_a_peer_named_by_the_caller_is_used_whatever_its_gate_says() {
    let mut local = view("laptop", true);
    faster(&mut local, 200.0);
    let mut peer = view("nixos", false);
    gate_closed(&mut peer);
    let r = run(&input(Request::Peer(n("nixos")), vec![local, peer]));
    assert_eq!(chosen(&r).as_deref(), Some("nixos"));
}

#[test]
fn r348_a_peers_compile_gate_does_not_count_against_a_run_job() {
    let mut local = view("laptop", true);
    local.free_slots.insert(SlotKind::Run, 1);
    faster(&mut local, 200.0);
    let mut peer = view("nixos", false);
    peer.free_slots.insert(SlotKind::Run, 1);
    faster(&mut peer, 30.0);
    gate_closed(&mut peer);
    // run-class jobs reach a peer only when it is named
    let mut i = input(Request::Peer(n("nixos")), vec![local, peer]);
    i.kind = Kind::Run;
    assert!(!run(&i).score_of(&n("nixos")).unwrap().blocked);
}

#[test]
fn r134_a_held_slot_without_a_known_holder_waits_the_unknown_eta() {
    let mut l = view("laptop", true);
    busy(&mut l, None);
    let r = run(&input(Request::Anywhere, vec![l]));
    let s = r.score_of(&n("laptop")).unwrap();
    assert_eq!(s.wait_secs, 300.0);
    assert_eq!(s.wait_basis, WaitBasis::Unknown);
}

#[test]
fn r134_a_free_second_slot_means_no_wait() {
    let mut l = view("laptop", true);
    l.free_slots.insert(SlotKind::Compile, 1);
    l.expected_wait_secs = Some(999.0); // ignored: a slot is free and nothing queued
    let r = run(&input(Request::Anywhere, vec![l]));
    let s = r.score_of(&n("laptop")).unwrap();
    assert_eq!((s.wait_secs, s.wait_basis), (0.0, WaitBasis::Free));
}

// ---- R-135 ----

#[test]
fn r135_ties_go_to_the_machine_with_more_free_slots_then_to_the_peer() {
    let mut l = view("laptop", true);
    l.free_slots.insert(SlotKind::Compile, 2);
    let mut p = view("nixos", false);
    p.free_slots.insert(SlotKind::Compile, 1);
    let mut pol = policy();
    pol.local_bonus_secs = 0.0;
    let i = input(Request::Anywhere, vec![l.clone(), p.clone()]);
    assert_eq!(
        chosen(&decide(&i, &pol).unwrap()).as_deref(),
        Some("laptop")
    );
    l.free_slots.insert(SlotKind::Compile, 1);
    let i = input(Request::Anywhere, vec![l, p]);
    assert_eq!(chosen(&decide(&i, &pol).unwrap()).as_deref(), Some("nixos"));
}

// ---- R-136 ----

#[test]
fn r136_idle_laptop_wins_when_the_worker_queue_is_longer() {
    let l = view("laptop", true);
    let mut p = view("nixos", false);
    busy(&mut p, Some(200.0));
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("laptop")
    );
}

#[test]
fn r136_idle_laptop_beats_a_held_worker_slot_for_a_small_unknown_crate() {
    let mut l = view("laptop", true);
    l.estimate = est(20.0);
    let mut p = view("nixos", false);
    p.estimate = est(20.0);
    busy(&mut p, None);
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("laptop")
    );
}

#[test]
fn r136_busy_laptop_sends_work_to_an_open_worker() {
    let mut l = view("laptop", true);
    busy(&mut l, Some(400.0));
    let p = view("nixos", false);
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("nixos")
    );
}

#[test]
fn r136_full_offload_slots_add_wait_to_the_peer() {
    let mut l = view("laptop", true);
    busy(&mut l, Some(100.0));
    let mut p = view("nixos", false);
    busy(&mut p, Some(300.0)); // the offload slots are held
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("laptop")
    );
}

#[test]
fn r136_the_faster_estimate_wins_when_waits_are_equal() {
    let mut l = view("laptop", true);
    l.estimate = est(600.0);
    let mut p = view("nixos", false);
    p.estimate = est(100.0);
    assert_eq!(
        chosen(&run(&input(Request::Anywhere, vec![l, p]))).as_deref(),
        Some("nixos")
    );
}

#[test]
fn r136_p90_weight_pulls_the_run_time_toward_the_p90() {
    let mut l = view("laptop", true);
    l.estimate = Some(RunEstimate {
        median_secs: 100.0,
        p90_secs: 300.0,
        samples: 9,
    });
    let mut pol = policy();
    pol.p90_weight = 0.5;
    let r = decide(&input(Request::Anywhere, vec![l]), &pol).unwrap();
    assert_eq!(r.score_of(&n("laptop")).unwrap().run_secs, 200.0);
}

// ---- R-10 (requirements 7.10): warm local vs cold peer; sync cost ----

#[test]
fn req7_10_a_cold_unsynced_peer_loses_to_a_free_local_machine() {
    let mut l = view("laptop", true);
    l.estimate = est(100.0);
    let mut p = view("nixos", false);
    p.estimate = est(80.0);
    p.checkout = Checkout {
        synced: false,
        sync_bytes: 24 << 20,
    };
    let r = run(&input(Request::Anywhere, vec![l, p]));
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert_eq!(r.score_of(&n("nixos")).unwrap().sync_secs, 48.0);
}

#[test]
fn req7_10_identical_inputs_give_identical_decisions() {
    let i = input(
        Request::Anywhere,
        vec![view("laptop", true), view("nixos", false)],
    );
    assert_eq!(run(&i), run(&i));
}

// ---- R-138, R-141: transportability and capability marks ----

#[test]
fn r138_a_job_that_cannot_be_shipped_stays_local() {
    let mut l = view("laptop", true);
    busy(&mut l, Some(900.0));
    let mut i = input(Request::Anywhere, vec![l, view("nixos", false)]);
    i.local_only_reason = Some("/tmp/x.sh exists only here".into());
    let r = run(&i);
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert!(matches!(
        exclusions_of(&r, "nixos"),
        [Exclusion::NotTransportable(_)]
    ));
}

#[test]
fn r138_a_named_peer_with_an_unshippable_job_is_refused_with_the_reason() {
    let mut i = input(
        Request::Peer(n("nixos")),
        vec![view("laptop", true), view("nixos", false)],
    );
    i.local_only_reason = Some("path".into());
    let r = run(&i);
    assert!(matches!(r.decision, Decision::Refuse(_)));
    assert!(r.multi_line().contains("cannot be shipped: path"));
}

#[test]
fn r141_a_peer_with_a_capability_mark_is_skipped_with_the_evidence() {
    let mut l = view("laptop", true);
    busy(&mut l, Some(900.0));
    let mut p = view("nixos", false);
    p.marks = vec!["libclang missing (bindgen failed in job 456)".into()];
    let r = run(&input(Request::Anywhere, vec![l, p]));
    assert_eq!(chosen(&r).as_deref(), Some("laptop"));
    assert!(r.one_line().contains("marked: libclang missing"));
}

// ---- R-143: requests ----

#[test]
fn r143_requests_parse_from_placement_and_hints() {
    use tasks_core::Placement;
    assert_eq!(Request::from_placement(&Placement::Here), Request::Local);
    assert_eq!(Request::from_placement(&Placement::Pin), Request::Pin);
    assert_eq!(
        Request::from_placement(&Placement::Peer(n("x"))),
        Request::Peer(n("x"))
    );
    assert_eq!(
        Request::from_placement(&Placement::Anywhere),
        Request::Anywhere
    );
    assert_eq!(Request::from_hint("local"), Request::Local);
    assert_eq!(Request::from_hint("pin"), Request::Pin);
    assert_eq!(Request::from_hint("anywhere"), Request::Anywhere);
    assert_eq!(Request::from_hint("nixos"), Request::Peer(n("nixos")));
}

// ---- hard requirements, pin, peer ----

#[test]
fn hard_requirements_are_never_traded_off_and_keep_the_named_reason() {
    let mut l = view("laptop", true);
    l.estimate = est(1.0);
    let mut p = view("mac", false);
    p.machine.os = Os::Macos;
    p.estimate = est(1000.0);
    let mut i = input(Request::Anywhere, vec![l, p]);
    i.requirements.os = Some(Os::Macos);
    let r = run(&i);
    assert_eq!(chosen(&r).as_deref(), Some("mac"));
    assert_eq!(
        exclusions_of(&r, "laptop"),
        &[Exclusion::Unmet(vec![tasks_core::Unmet::Os {
            want: Os::Macos,
            have: Os::Linux
        }])]
    );
}

#[test]
fn hint_tools_are_extra_requirements() {
    let mut i = input(
        Request::Anywhere,
        vec![view("laptop", true), view("nixos", false)],
    );
    i.hints.tools = vec!["nasm".into()];
    let r = run(&i);
    assert_eq!(r.decision, Decision::Refuse(Refusal::NoCapableMachine));
    assert!(r.multi_line().contains("tool nasm missing"));
}

#[test]
fn pin_never_moves_and_says_why_when_local_cannot() {
    let mut l = view("laptop", true);
    l.machine.os = Os::Macos;
    let mut i = input(Request::Pin, vec![l, view("nixos", false)]);
    i.requirements.os = Some(Os::Linux);
    let r = run(&i);
    assert_eq!(
        r.decision,
        Decision::Refuse(Refusal::RequestedMachineCannot(n("laptop")))
    );
}

#[test]
fn a_named_peer_never_silently_moves_to_local() {
    let mut p = view("nixos", false);
    p.link = Link::Up { age_secs: 999 };
    let r = run(&input(
        Request::Peer(n("nixos")),
        vec![view("laptop", true), p],
    ));
    assert!(matches!(r.decision, Decision::Refuse(_)));
}

#[test]
fn an_unknown_peer_is_refused_by_name() {
    let r = run(&input(
        Request::Peer(n("ghost")),
        vec![view("laptop", true)],
    ));
    assert_eq!(
        r.decision,
        Decision::Refuse(Refusal::UnknownPeer(n("ghost")))
    );
}

#[test]
fn anywhere_with_nothing_capable_anywhere_is_refused_not_waited() {
    let mut i = input(Request::Anywhere, vec![view("laptop", true)]);
    i.requirements.os = Some(Os::Macos);
    assert_eq!(
        run(&i).decision,
        Decision::Refuse(Refusal::NoCapableMachine)
    );
}

#[test]
fn bad_input_is_an_error() {
    let pol = policy();
    let i = input(Request::Anywhere, vec![view("a", false)]);
    assert_eq!(decide(&i, &pol), Err(PlacementError::NoLocalMachine));
    let i = input(Request::Anywhere, vec![view("a", true), view("b", true)]);
    assert_eq!(decide(&i, &pol), Err(PlacementError::SeveralLocalMachines));
    let i = input(Request::Anywhere, vec![view("a", true), view("a", false)]);
    assert!(matches!(
        decide(&i, &pol),
        Err(PlacementError::DuplicateMachine(_))
    ));
    let mut v = view("a", true);
    v.expected_wait_secs = Some(f64::NAN);
    assert!(decide(&input(Request::Anywhere, vec![v]), &pol).is_err());
    let mut bad = policy();
    bad.p90_weight = 2.0;
    assert_eq!(
        decide(&input(Request::Anywhere, vec![view("a", true)]), &bad),
        Err(PlacementError::BadWeight)
    );
}

// ---- R-116: explanation text ----

fn explain_input() -> Input {
    let mut l = view("laptop", true);
    l.estimate = est(120.0);
    let mut p = view("nixos", false);
    p.estimate = est(100.0);
    busy(&mut p, Some(45.0));
    p.checkout = Checkout {
        synced: false,
        sync_bytes: 10 << 20,
    };
    let mut m = view("mac", false);
    m.link = Link::Down {
        last_seen_ago_secs: Some(300),
    };
    input(Request::Anywhere, vec![p, m, l])
}

#[test]
fn r116_multi_line_explanation_is_stable_and_sorted_by_name() {
    let r = run(&explain_input());
    assert_eq!(
        r.multi_line(),
        "placement: laptop (chosen by --anywhere)\n\
         \x20 laptop (local): wait 0s (slot free) + run 2m00s + sync 0s - local bonus 30s = 1m30s <- chosen\n\
         \x20 mac: excluded: unreachable, last seen 5m00s ago\n\
         \x20 nixos: wait 45s (expected) + run 1m40s + sync 20s = 2m45s"
    );
}

#[test]
fn r116_one_line_explanation_is_stable() {
    assert_eq!(
        run(&explain_input()).one_line(),
        "laptop (chosen by --anywhere): estimate 1m30s (slot free); mac excluded: unreachable, last seen 5m00s ago, nixos 2m45s"
    );
}

// R-355
#[test]
fn r355_the_line_says_where_why_and_never_presents_a_fallback_as_an_estimate() {
    // asked for a peer, with no run history anywhere: the reason is the ask, and there is no
    // number (the 6m30s of the first trial was the policy's assumed run time)
    let mut local = view("laptop", true);
    let mut peer = view("nixos", false);
    local.estimate = None;
    peer.estimate = None;
    let r = run(&input(
        Request::Peer(MachineName("nixos".into())),
        vec![local.clone(), peer.clone()],
    ));
    let line = r.one_line();
    assert!(
        line.starts_with(
            "nixos (asked with --peer nixos): no estimate (no run history for this command)"
        ),
        "{line}"
    );
    assert!(!line.contains("(local)"), "{line}");
    assert!(
        !line.contains("6m30s") && !line.contains("assumed"),
        "{line}"
    );
    assert!(
        r.multi_line()
            .starts_with("placement: nixos (asked with --peer nixos)\n")
    );
    // the machine that is this one says so on its own line, and only there
    assert!(r.multi_line().contains("laptop (local):"));
    assert!(!r.multi_line().contains("nixos (local)"));

    // history, but the queue ahead is unknown: still no number
    let mut busy_peer = peer.clone();
    busy_peer.estimate = est(100.0);
    busy(&mut busy_peer, None);
    let r = run(&input(
        Request::Peer(MachineName("nixos".into())),
        vec![local.clone(), busy_peer],
    ));
    assert!(
        r.one_line()
            .contains("no estimate (the queue ahead is unknown)"),
        "{}",
        r.one_line()
    );

    // history and a free slot: a number, with its basis
    let mut known = peer;
    known.estimate = est(100.0);
    let r = run(&input(
        Request::Peer(MachineName("nixos".into())),
        vec![local, known],
    ));
    assert!(
        r.one_line()
            .starts_with("nixos (asked with --peer nixos): estimate 1m40s (slot free)"),
        "{}",
        r.one_line()
    );
    // the other machines are listed without invented numbers too
    let mut other = view("mac", false);
    other.estimate = None;
    let r = run(&input(Request::Anywhere, vec![view("laptop", true), other]));
    assert!(r.one_line().contains("no estimate"), "{}", r.one_line());
}

#[test]
fn r116_wait_and_refusal_render_with_their_reasons() {
    let mut i = input(Request::Pin, vec![view("laptop", true)]);
    i.requirements.os = Some(Os::Macos);
    let r = run(&i);
    assert!(
        r.one_line()
            .starts_with("refused (pinned here with --pin): laptop cannot run it")
    );
    assert!(r.multi_line().contains("unmet: os Macos wanted, has Linux"));
}

#[test]
fn r116_durations_format() {
    assert_eq!(fmt_secs(0.4), "0s");
    assert_eq!(fmt_secs(59.6), "1m00s");
    assert_eq!(fmt_secs(125.0), "2m05s");
    assert_eq!(fmt_secs(3720.0), "1h02m");
    assert_eq!(fmt_secs(f64::NAN), "0s");
}

// ---- R-230: no ordering between jobs ----

#[test]
fn r230_placement_never_makes_a_job_wait_on_another_jobs_position() {
    // Two jobs from the same checkout. Each decision depends only on its own input:
    // deciding B first, or A first, or interleaved, gives the same answers, and neither
    // decision can name a dependency on the other.
    let mut p = view("nixos", false);
    p.checkout = Checkout {
        synced: false,
        sync_bytes: 4 << 20,
    };
    let a = input(Request::Anywhere, vec![view("laptop", true), p.clone()]);
    let mut b = a.clone();
    b.kind = Kind::Compile;
    let (ra1, rb1) = (run(&a), run(&b));
    let (rb2, ra2) = (run(&b), run(&a));
    assert_eq!((&ra1, &rb1), (&ra2, &rb2));
    // The sync of the shared checkout is charged to each job alike; neither "waits" for the
    // other's sync, so both scores carry the same sync part.
    assert_eq!(
        ra1.score_of(&n("nixos")).unwrap().sync_secs,
        rb1.score_of(&n("nixos")).unwrap().sync_secs
    );
    for r in [&ra1, &rb1] {
        assert!(!matches!(r.decision, Decision::Wait));
    }
}

#[test]
fn r230_a_job_from_another_checkout_is_unaffected_by_a_blocked_one() {
    let mut p = view("nixos", false);
    p.link = Link::Down {
        last_seen_ago_secs: None,
    };
    let stuck = input(
        Request::Peer(n("nixos")),
        vec![view("laptop", true), p.clone()],
    );
    let other = input(Request::Anywhere, vec![view("laptop", true), p]);
    assert!(matches!(run(&stuck).decision, Decision::Refuse(_)));
    assert_eq!(chosen(&run(&other)).as_deref(), Some("laptop"));
}

// ---- reroute ----

fn cur(m: &str) -> Current {
    Current {
        machine: n(m),
        state: JobState::Queued,
        queued_secs: 100,
        reroutes: 0,
    }
}

fn reroute_input() -> Input {
    // Queued on the laptop, which is now very busy; the worker is open.
    let mut l = view("laptop", true);
    busy(&mut l, Some(600.0));
    l.estimate = est(100.0);
    let mut p = view("nixos", false);
    p.estimate = est(100.0);
    input(Request::Anywhere, vec![l, p])
}

#[test]
fn reroute_moves_a_queued_job_when_the_gain_exceeds_the_margin() {
    let r = should_reroute(&cur("laptop"), &reroute_input(), &policy()).unwrap();
    match r {
        Reroute::Move { to, gain_secs, .. } => {
            assert_eq!(to, n("nixos"));
            assert_eq!(gain_secs, Some(570.0)); // 600+100-30 vs 100
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn reroute_never_moves_a_running_job() {
    let mut c = cur("laptop");
    c.state = JobState::Running;
    assert_eq!(
        should_reroute(&c, &reroute_input(), &policy()).unwrap(),
        Reroute::Stay(StayReason::Running)
    );
}

#[test]
fn reroute_never_moves_pin_local_or_named_peer_requests() {
    for req in [Request::Pin, Request::Local, Request::Peer(n("laptop"))] {
        let mut i = reroute_input();
        i.request = req;
        assert_eq!(
            should_reroute(&cur("laptop"), &i, &policy()).unwrap(),
            Reroute::Stay(StayReason::NotMovable)
        );
    }
}

#[test]
fn reroute_hysteresis_keeps_a_job_when_the_gain_is_within_the_margin() {
    let mut i = reroute_input();
    if let Some(l) = i.machines.iter_mut().find(|v| v.is_local) {
        l.expected_wait_secs = Some(100.0); // 100+100-30 = 170 vs 100: gain 70
    }
    let mut pol = policy();
    pol.reroute_margin_secs = 70.0;
    assert_eq!(
        should_reroute(&cur("laptop"), &i, &pol).unwrap(),
        Reroute::Stay(StayReason::WithinMargin)
    );
    pol.reroute_margin_secs = 69.0;
    assert!(matches!(
        should_reroute(&cur("laptop"), &i, &pol).unwrap(),
        Reroute::Move { .. }
    ));
}

#[test]
fn reroute_does_not_ping_pong() {
    // After the move the job sits on nixos; the laptop is now only slightly better.
    let mut i = reroute_input();
    for v in &mut i.machines {
        if v.is_local {
            v.free_slots.insert(SlotKind::Compile, 1);
            v.queue_depth = 0;
            v.expected_wait_secs = None;
        } else {
            busy(v, Some(120.0));
        }
    }
    // laptop 100-30 = 70 vs nixos 120+100 = 220: gain 150 > margin, so it would move once.
    // With a modest worker wait the job stays: no flapping on noise.
    for v in &mut i.machines {
        if !v.is_local {
            v.expected_wait_secs = Some(20.0);
        }
    }
    assert_eq!(
        should_reroute(&cur("nixos"), &i, &policy()).unwrap(),
        Reroute::Stay(StayReason::WithinMargin)
    );
}

#[test]
fn reroute_treats_the_current_peers_checkout_as_already_synced() {
    let mut i = reroute_input();
    for v in &mut i.machines {
        if !v.is_local {
            v.checkout = Checkout {
                synced: false,
                sync_bytes: 500 << 20, // would cost 1000s if charged again
            };
        } else {
            v.free_slots.insert(SlotKind::Compile, 1);
            v.queue_depth = 0;
        }
    }
    assert_eq!(
        should_reroute(&cur("nixos"), &i, &policy()).unwrap(),
        Reroute::Stay(StayReason::WithinMargin)
    );
}

#[test]
fn reroute_respects_cooldown_and_the_reroute_cap() {
    let mut c = cur("laptop");
    c.queued_secs = 5;
    assert_eq!(
        should_reroute(&c, &reroute_input(), &policy()).unwrap(),
        Reroute::Stay(StayReason::Cooldown)
    );
    let mut c = cur("laptop");
    c.reroutes = 2;
    assert_eq!(
        should_reroute(&c, &reroute_input(), &policy()).unwrap(),
        Reroute::Stay(StayReason::TooManyReroutes)
    );
}

#[test]
fn reroute_moves_at_once_when_the_current_machine_is_no_longer_usable() {
    let mut i = reroute_input();
    for v in &mut i.machines {
        if v.is_local {
            v.machine.os = Os::Macos; // cannot satisfy the requirement any more
        }
    }
    i.requirements.os = Some(Os::Linux);
    let mut c = cur("laptop");
    c.queued_secs = 0;
    c.reroutes = 9;
    match should_reroute(&c, &i, &policy()).unwrap() {
        Reroute::Move { to, gain_secs, .. } => {
            assert_eq!((to, gain_secs), (n("nixos"), None));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn reroute_stays_when_nothing_else_can_take_the_job() {
    let mut i = reroute_input();
    i.machines.retain(|v| v.is_local);
    assert_eq!(
        should_reroute(&cur("laptop"), &i, &policy()).unwrap(),
        Reroute::Stay(StayReason::NoBetter)
    );
}

#[test]
fn reroute_with_an_unknown_current_machine_is_an_error() {
    assert!(matches!(
        should_reroute(&cur("ghost"), &reroute_input(), &policy()),
        Err(PlacementError::UnknownCurrent(_))
    ));
}

// ---- candidates (kept from the first cut) ----

#[test]
fn splits_capable_and_incapable() {
    let req = Requirements {
        os: Some(Os::Macos),
        ..Requirements::default()
    };
    let mut mac = view("mac", false).machine;
    mac.os = Os::Macos;
    let ms = [view("lin", true).machine, mac];
    let out = candidates(&req, &ms);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0].1,
        Err(vec![tasks_core::Unmet::Os {
            want: Os::Macos,
            have: Os::Linux
        }])
    );
    assert_eq!(out[1], (n("mac"), Ok(())));
}

// ---- property-style ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn random_input(rng: &mut Rng) -> Input {
    let count = 1 + rng.below(4) as usize;
    let mut machines = Vec::new();
    for k in 0..count {
        let mut v = view(&format!("m{k}"), k == 0);
        if rng.chance(30) {
            v.machine.os = Os::Macos;
        }
        v.link = match rng.below(3) {
            0 => Link::Down {
                last_seen_ago_secs: Some(rng.below(1000)),
            },
            1 => Link::Up {
                age_secs: rng.below(200),
            },
            _ => Link::Up { age_secs: 1 },
        };
        v.free_slots.insert(SlotKind::Compile, rng.below(3) as u32);
        v.queue_depth = rng.below(3) as u32;
        v.expected_wait_secs = rng.chance(70).then(|| rng.below(2000) as f64);
        v.pressure.blocked = rng.chance(20);
        v.estimate = rng.chance(60).then(|| RunEstimate {
            median_secs: rng.below(900) as f64,
            p90_secs: rng.below(1800) as f64,
            samples: 3,
        });
        v.checkout = Checkout {
            synced: rng.chance(50),
            sync_bytes: rng.below(100 << 20),
        };
        if rng.chance(10) {
            v.marks.push("mark".into());
        }
        machines.push(v);
    }
    let request = match rng.below(4) {
        0 => Request::Local,
        1 => Request::Pin,
        2 => Request::Peer(n(&format!("m{}", rng.below(5)))),
        _ => Request::Anywhere,
    };
    let mut i = input(request, machines);
    i.kind = match rng.below(3) {
        0 => Kind::Compile,
        1 => Kind::Device,
        _ => Kind::Probe,
    };
    if rng.chance(30) {
        i.requirements.os = Some(Os::Linux);
    }
    i.local_only_reason = rng.chance(15).then(|| "path".to_string());
    i
}

#[test]
fn property_decisions_respect_exclusions_pin_and_determinism() {
    let pol = policy();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..2000 {
        let i = random_input(&mut rng);
        let r = decide(&i, &pol).unwrap();
        // determinism, also under a permuted machine order
        assert_eq!(r, decide(&i, &pol).unwrap());
        let mut rev = i.clone();
        rev.machines.reverse();
        assert_eq!(r.decision, decide(&rev, &pol).unwrap().decision);
        assert_eq!(r.multi_line(), decide(&rev, &pol).unwrap().multi_line());

        if let Decision::Run(name) = &r.decision {
            let v = i.machines.iter().find(|v| &v.machine.name == name).unwrap();
            // never an excluded machine
            assert!(matches!(
                r.machines.iter().find(|m| &m.name == name).unwrap().verdict,
                Verdict::Eligible(_)
            ));
            assert_eq!(i.requirements.satisfied_by(&v.machine), Ok(()));
            if !v.is_local {
                // never stale or unreachable, never marked, never unshippable
                assert!(
                    matches!(v.link, Link::Up { age_secs } if age_secs <= pol.staleness_limit_secs)
                );
                assert!(v.marks.is_empty() && i.local_only_reason.is_none());
            }
            // pin and local never move
            if matches!(i.request, Request::Pin | Request::Local) {
                assert!(v.is_local);
            }
            if let Request::Peer(p) = &i.request {
                assert_eq!(&v.machine.name, p);
            }
            // the winner has the lowest total among eligible machines
            // (a machine with a closed gate is passed over when another could start at once)
            let eligible: Vec<&Score> = r
                .machines
                .iter()
                .filter_map(|m| match &m.verdict {
                    Verdict::Eligible(s) => Some(s),
                    _ => None,
                })
                .collect();
            let someone_ready = eligible.iter().any(|s| s.ready_now());
            let best = eligible
                .iter()
                .filter(|s| !(someone_ready && s.blocked))
                .map(|s| s.total_secs())
                .fold(f64::INFINITY, f64::min);
            assert_eq!(r.score_of(name).unwrap().total_secs(), best);
        } else if matches!(i.request, Request::Pin) {
            assert!(matches!(r.decision, Decision::Refuse(_)));
        }
    }
}

// ---- R-541: the cap of the machine that runs the job ----

#[test]
fn r541_an_anywhere_job_above_the_local_cap_goes_to_a_peer_even_when_local_is_faster() {
    let mut l = view("laptop", true);
    l.estimate = est(10.0);
    let mut p = view("nixos", false);
    p.estimate = est(600.0);
    let mut i = input(Request::Anywhere, vec![l, p]);
    i.local_cargo_jobs = Some(CargoJobs { asked: 6, cap: 2 });
    let r = run(&i);
    assert_eq!(chosen(&r).as_deref(), Some("nixos"));
    assert!(matches!(
        exclusions_of(&r, "laptop"),
        [Exclusion::AboveCargoCap { asked: 6, cap: 2 }]
    ));
}

#[test]
fn r541_with_no_other_machine_an_anywhere_job_above_the_cap_is_refused_naming_the_caps() {
    let mut i = input(Request::Anywhere, vec![view("laptop", true)]);
    i.local_cargo_jobs = Some(CargoJobs { asked: 6, cap: 2 });
    let r = run(&i);
    assert!(matches!(r.decision, Decision::Refuse(_)));
    let text = r.one_line();
    assert!(
        text.contains("parallelism 6") && text.contains("cap of 2"),
        "{text}"
    );
}

#[test]
fn r541_a_request_within_the_cap_or_none_changes_nothing() {
    let mut i = input(Request::Anywhere, vec![view("laptop", true)]);
    i.local_cargo_jobs = Some(CargoJobs { asked: 2, cap: 2 });
    assert_eq!(chosen(&run(&i)).as_deref(), Some("laptop"));
}
