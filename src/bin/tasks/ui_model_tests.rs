use super::*;
use serde_json::json;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/snapshot");

fn fixture(scenario: &str, file: &str) -> Snapshot {
    let path = format!("{FIXTURES}/{scenario}/{file}");
    Snapshot::parse_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn state(snapshot: Snapshot, local: bool, age: f64) -> MachineState {
    MachineState {
        fallback_name: if local { "laptop" } else { "worker" }.into(),
        snapshot: Some(snapshot),
        local,
        seen_ago_s: Some(age),
        error: None,
        configured: true,
    }
}

fn scenario(name: &str) -> Vec<MachineState> {
    let laptop = fixture(name, "laptop.json");
    let worker = fixture(name, "worker.json");
    let newest = laptop.taken_at.unwrap().max(worker.taken_at.unwrap());
    let (la, wa) = (
        newest - laptop.taken_at.unwrap(),
        newest - worker.taken_at.unwrap(),
    );
    vec![state(laptop, true, la), state(worker, false, wa)]
}

fn running<'a>(view: &'a View, machine: &str, id: i64) -> &'a RunningCard {
    view.running
        .iter()
        .find(|r| r.key.machine == machine && r.key.id == id)
        .unwrap_or_else(|| panic!("no running card {machine}#{id}"))
}

fn queued<'a>(view: &'a View, machine: &str, id: i64) -> &'a QueuedRow {
    view.queued
        .iter()
        .find(|r| r.key.machine == machine && r.key.id == id)
        .unwrap_or_else(|| panic!("no queued row {machine}#{id}"))
}

// --- formatting -------------------------------------------------------------

#[test]
fn clock_formats_minutes_and_hours() {
    assert_eq!(clock(0.0), "0:00");
    assert_eq!(clock(724.0), "12:04");
    assert_eq!(clock(490.4), "8:10");
    assert_eq!(clock(3725.0), "1:02:05");
    assert_eq!(clock(-5.0), "0:00");
}

#[test]
fn remaining_time_reads_as_minutes_left() {
    assert_eq!(left(236.0), "~4 min left");
    assert_eq!(left(780.0), "~13 min left");
    assert_eq!(left(30.0), "<1 min left");
    assert_eq!(left(0.0), "taking longer than estimated");
    assert_eq!(left(-20.0), "taking longer than estimated");
    assert_eq!(left(5400.0), "~1 h 30 min left");
    assert_eq!(minutes(10.0), "1 min");
}

#[test]
fn memory_and_cpu_formatting() {
    let gib = 1024.0 * 1024.0 * 1024.0;
    assert_eq!(gb(3.9 * gib), "3.9 GB");
    assert_eq!(gb(15.0 * gib), "15 GB");
    assert_eq!(gb(0.1 * gib), "0.1 GB");
    assert_eq!(used_of_total(21.0 * gib, 39.0 * gib), "21/39 GB");
    assert_eq!(used_of_total(6.1 * gib, 7.6 * gib), "6.1/7.6 GB");
    assert_eq!(cores(7.76), "7.8 cores");
    assert_eq!(percent(0.68), "68%");
    assert_eq!(ago(5.0), "5 s ago");
    assert_eq!(ago(300.0), "5 min ago");
    assert_eq!(ago(7300.0), "2 h ago");
}

#[test]
fn short_command_drops_noise_and_unwraps_offload_clients() {
    let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(
        short_command(&argv(
            "env PKG_CONFIG_PATH=/x/y RUSTFLAGS=-g /usr/bin/cargo build --locked -j 2 -p desktop --release"
        )),
        "cargo build -p desktop --release"
    );
    assert_eq!(
        short_command(&argv(
            "/home/u/.local/bin/tasks-offload --worker u@w --who me -- cargo test --release -j8 --offline"
        )),
        "cargo test --release"
    );
    assert_eq!(
        short_command(&argv("cargo test --jobs 4 --manifest-path /a/b/Cargo.toml")),
        "cargo test --manifest-path Cargo.toml"
    );
    assert_eq!(
        short_command(&argv("cargo test --jobs=4 -- -j")),
        "cargo test --"
    );
    assert_eq!(short_command(&[]), "");
    // A client without `--` keeps its own name.
    assert_eq!(
        short_command(&argv("tasks-offload --help")),
        "tasks-offload --help"
    );
}

// --- reader -------------------------------------------------------------------

#[test]
fn reader_ignores_unknown_fields_and_keeps_nulls_unknown() {
    let snap = Snapshot::parse(json!({
        "version": 1,
        "taken_at": 10.0,
        "new_top": {"x": 1},
        "machine": {"id": "m", "role": "worker", "logical_cpus": null, "cpu_busy_cores": null,
                    "mem_total_bytes": null, "slots": null, "history": null, "new": true},
        "jobs": [
            {"id": 1, "phase": "running", "argv": null, "live": null, "estimate": null, "brand_new": 5},
            {"id": "not a number"},
            {"phase": "running"},
            {"id": 2, "submitted_at": "garbage"}
        ]
    }))
    .unwrap();
    assert_eq!(snap.jobs.len(), 1);
    assert_eq!(snap.jobs[0].id, 1);
    assert_eq!(snap.jobs[0].raw["brand_new"], 5);
    assert_eq!(snap.warnings.len(), 3, "{:?}", snap.warnings);
    let card = machine_card(&state(snap, false, 0.0));
    assert_eq!(card.cpu_text, "CPU unknown");
    assert_eq!(card.ram_text, "RAM unknown");
    assert_eq!(card.swap_text, "swap unknown");
    assert_eq!(card.pressure_text, "unknown");
    assert_eq!(card.pressure_level, Level::Unknown);
    assert!(card.slots_text.contains("slots unknown"));
    assert_eq!(card.history_span, "no history yet");
}

#[test]
fn reader_rejects_only_non_objects_and_flags_newer_versions() {
    assert!(Snapshot::parse(json!([1, 2])).is_err());
    assert!(Snapshot::parse_str("{nope").is_err());
    let snap = Snapshot::parse(json!({"version": 2, "machine": "bogus"})).unwrap();
    assert_eq!(snap.warnings.len(), 2, "{:?}", snap.warnings);
    assert!(snap.warnings[0].contains("newer"));
}

#[test]
fn every_fixture_parses_without_warnings() {
    for scenario in ["ui-busy", "ui-offline", "ui-idle"] {
        for file in ["laptop.json", "worker.json"] {
            let snap = fixture(scenario, file);
            assert!(
                snap.warnings.is_empty(),
                "{scenario}/{file}: {:?}",
                snap.warnings
            );
            assert!(!snap.jobs.is_empty());
        }
    }
}

// --- machine cards ---------------------------------------------------------------

#[test]
fn busy_machine_cards_match_the_agreed_layout() {
    let view = build(&scenario("ui-busy"));
    let laptop = &view.machines[0];
    assert_eq!(laptop.title, "kurukkan (laptop)");
    assert_eq!(laptop.status, Presence::Online);
    assert_eq!(laptop.cpu_text, "CPU 68%");
    assert_eq!(laptop.ram_text, "RAM 21/39 GB");
    assert_eq!(laptop.swap_text, "swap 15 GB");
    assert_eq!(
        (laptop.pressure_text.as_str(), laptop.pressure_level),
        ("ok", Level::Ok)
    );
    assert_eq!(
        laptop.slots_text,
        "compile 1/1 · run 0/4 · offload 2/4 · phone: none"
    );
    assert_eq!(laptop.cpu_history.len(), 150);
    assert!(
        laptop.history_span.starts_with("last 5 min"),
        "{}",
        laptop.history_span
    );

    let worker = &view.machines[1];
    assert_eq!(worker.title, "nixos worker");
    assert_eq!(worker.cpu_text, "CPU 97%");
    assert_eq!(worker.ram_text, "RAM 6.1/7.6 GB");
    assert_eq!(worker.swap_text, "swap 0.1 GB");
    assert_eq!(
        (worker.pressure_text.as_str(), worker.pressure_level),
        ("memory high", Level::Bad)
    );
    assert!(
        worker
            .pressure_detail
            .contains("memory PSI full avg10 above 0.5%")
    );
    assert_eq!(
        worker.slots_text,
        "compile 1/1 · run 0/4 · phone: SYNTHPHONE01 busy"
    );
    assert!(worker.slots_detail.contains("by #91 view-step-up@kurukkan"));
}

#[test]
fn online_state_covers_every_source_condition() {
    let mut s = MachineState {
        fallback_name: "worker".into(),
        configured: false,
        ..Default::default()
    };
    assert!(matches!(online(&s), Presence::NotConfigured(_)));
    s.configured = true;
    assert_eq!(online(&s), Presence::Connecting);
    s.error = Some("ssh: connection refused".into());
    assert_eq!(
        online(&s),
        Presence::Offline {
            last_seen: None,
            reason: "ssh: connection refused".into()
        }
    );
    s.snapshot = Some(Snapshot::default());
    s.seen_ago_s = Some(3.0);
    assert_eq!(online(&s), Presence::Online);
    s.seen_ago_s = Some(OFFLINE_AFTER_S + 1.0);
    assert!(matches!(
        online(&s),
        Presence::Offline {
            last_seen: Some(_),
            ..
        }
    ));
}

#[test]
fn pressure_words_name_the_blocker() {
    let with = |b: &str| Machine {
        blocker: Some(b.into()),
        ..Default::default()
    };
    assert_eq!(
        pressure_words(&with("available memory below 8 GiB")).0,
        "low free memory"
    );
    assert_eq!(
        pressure_words(&with("I/O PSI some avg10 above 20%")).0,
        "I/O high"
    );
    assert_eq!(
        pressure_words(&with("CPU PSI some avg10 above 40%")),
        ("CPU high".into(), Level::Warn)
    );
    assert_eq!(
        pressure_words(&with("cache disk below reserve")).0,
        "disk low"
    );
}

// --- running, pairing ------------------------------------------------------------

#[test]
fn offload_pairs_show_once_as_the_worker_job() {
    let view = build(&scenario("ui-busy"));
    // Laptop offload clients #306/#307/#303 are hidden behind worker #91/#92/#89.
    for id in [306, 307, 303] {
        assert!(view.running.iter().all(|r| r.key.id != id));
        assert!(view.queued.iter().all(|r| r.key.id != id));
        assert!(view.recent.iter().all(|r| r.key.id != id));
    }
    let card = running(&view, "nixos", 91);
    assert_eq!(card.machine, "worker");
    assert_eq!(card.title, "view-step-up");
    let note = card.offload_note.as_deref().unwrap();
    assert!(
        note.contains("laptop #306") && note.contains("sync 3.2 s"),
        "{note}"
    );
    // Buttons act on the laptop client, the job this machine owns.
    assert!(matches!(card.cancel, Target::Local { id: 306, .. }));
    assert!(matches!(card.logs, Target::Local { id: 306, .. }));
    // Linked by owner@laptop + checkout when remote_job_id is still null.
    let fast = queued(&view, "nixos", 92);
    assert!(fast.details.raw_json.contains("fast-start@kurukkan"));
    assert!(matches!(fast.cancel, Target::Local { id: 307, .. }));
    // Finished pair: linked by remote_job_id.
    let done = view.recent.iter().find(|r| r.key.id == 89).unwrap();
    assert!(done.summary.contains("laptop #303"), "{}", done.summary);
}

#[test]
fn running_cards_carry_progress_eta_and_live_numbers() {
    let view = build(&scenario("ui-busy"));
    let warm = running(&view, "nixos", 91);
    assert_eq!(warm.command, "cargo test --release");
    assert_eq!(warm.warmth, Warmth::Warm);
    assert_eq!(warm.cpu_text, "7.8 cores");
    assert_eq!(warm.rss_text, "3.9 GB");
    let (fraction, text) = warm.progress.clone().unwrap();
    assert_eq!(text, "212/310 crates");
    assert!((fraction.unwrap() - 212.0 / 310.0).abs() < 1e-6);
    // The worker snapshot is 1 s old when shown: elapsed is measured on the
    // worker's clock plus that age (724 s), the 236 s remainder shrinks by it.
    assert_eq!(warm.elapsed_text, "12:04 elapsed");
    assert_eq!(warm.eta_text, "~4 min left (6 similar runs)");
    assert!(warm.eta_basis.contains("median of 6 warm runs"));
    assert!(warm.stale.is_none());

    let cold = running(&view, "kurukkan", 305);
    assert_eq!(cold.machine, "laptop");
    assert_eq!(cold.warmth, Warmth::Cold);
    assert_eq!(
        cold.warmth_reason,
        "no build output for this checkout on this machine"
    );
    assert_eq!(cold.elapsed_text, "8:10 elapsed");
    assert_eq!(cold.eta_text, "~13 min left (2 similar runs)");
    assert_eq!(cold.progress.clone().unwrap().1, "90/520 crates");
    assert!(matches!(cold.cancel, Target::Local { id: 305, .. }));

    // A remote job without a laptop pair: buttons disabled with the agreed tooltip.
    let device = running(&view, "nixos", 93);
    assert_eq!(
        device.cancel,
        Target::Disabled("cancel on the worker: later".into())
    );
    assert!(matches!(device.logs, Target::Disabled(_)));
    assert_eq!(device.warmth, Warmth::Unknown);
    assert_eq!(device.eta_text, "ETA unknown");
    assert!(device.progress.is_none());
    assert_eq!(view.running.len(), 3);
}

#[test]
fn unknown_live_values_never_become_zero() {
    let snap = Snapshot::parse(json!({
        "taken_at": 100.0,
        "machine": {"id": "m", "role": "host"},
        "jobs": [{"id": 7, "phase": "running", "started_at": null, "live": null,
                  "progress": {"crates_compiled": 5, "crates_expected": null}}]
    }))
    .unwrap();
    let view = build(&[state(snap, true, 0.0)]);
    let card = &view.running[0];
    assert_eq!(card.cpu_text, "CPU unknown");
    assert_eq!(card.rss_text, "RAM unknown");
    assert_eq!(card.elapsed_text, "elapsed unknown");
    assert_eq!(card.eta_text, "ETA unknown");
    assert_eq!(card.warmth, Warmth::Unknown);
    assert_eq!(card.command, "unknown command");
    assert_eq!(
        card.progress,
        Some((None, "5 crates (total unknown)".into()))
    );
}

// --- queue -------------------------------------------------------------------------

#[test]
fn queue_start_math_single_and_multi_slot() {
    // One compile slot: starts are the running remainder plus the totals ahead.
    assert_eq!(
        queue_starts(
            &[Some(240.0)],
            1,
            &[Some(200.0), Some(60.0), None, Some(5.0)]
        ),
        vec![Some(240.0), Some(440.0), Some(500.0), None]
    );
    // Four run slots, one busy: the next three start now.
    assert_eq!(
        queue_starts(&[Some(100.0)], 4, &[Some(10.0); 5]),
        vec![Some(0.0), Some(0.0), Some(0.0), Some(10.0), Some(10.0)]
    );
    // A holder without an estimate makes the wait unknown, unless a slot is free.
    assert_eq!(
        queue_starts(&[None], 1, &[Some(1.0), Some(1.0)]),
        vec![None, None]
    );
    assert_eq!(
        queue_starts(&[None], 2, &[Some(30.0), Some(1.0)]),
        vec![Some(0.0), None]
    );
    // Overdue holders count as free now.
    assert_eq!(
        queue_starts(&[Some(-50.0)], 1, &[Some(1.0)]),
        vec![Some(0.0)]
    );
    assert!(queue_starts(&[], 0, &[]).is_empty());
}

#[test]
fn queued_rows_show_position_reason_and_start_estimate() {
    let view = build(&scenario("ui-busy"));
    let fast = queued(&view, "nixos", 92);
    assert_eq!(fast.position, 1);
    assert_eq!(fast.machine, "worker");
    assert_eq!(fast.waiting_text, "waits for compile slot held by #91");
    // #91 has 236 s left in a 1 s old snapshot.
    assert_eq!(fast.start_text, "starts in ~4 min");

    let lint = queued(&view, "kurukkan", 308);
    assert_eq!(
        (lint.position, lint.start_text.as_str()),
        (1, "starts in ~13 min")
    );
    let docs = queued(&view, "kurukkan", 309);
    // Second in line: behind #305 (780 s) and #308 (240 s).
    assert_eq!(
        (docs.position, docs.start_text.as_str()),
        (2, "starts in ~17 min")
    );

    let proposal = queued(&view, "kurukkan", 304);
    assert!(proposal.proposal && proposal.local);
    assert_eq!(proposal.start_text, "starts after approval");
    // Proposals sort after the real queue.
    assert!(view.queued.last().unwrap().proposal);
}

#[test]
fn pressure_waits_say_what_they_wait_for() {
    let snap = Snapshot::parse(json!({
        "taken_at": 0.0,
        "machine": {"id": "m", "role": "host", "slots": [{"kind": "run", "name": "p2p-run-1", "busy": false}]},
        "jobs": [{"id": 1, "phase": "waiting_run", "resource": "run",
                  "queue": {"position": 1, "waiting_for": "memory pressure"}}]
    }))
    .unwrap();
    let view = build(&[state(snap, true, 0.0)]);
    assert_eq!(view.queued[0].waiting_text, "waits for memory pressure");
    assert_eq!(
        view.queued[0].start_text,
        "starts when memory pressure clears"
    );
}

// --- offline and idle ------------------------------------------------------------

#[test]
fn offline_worker_is_greyed_and_its_offload_shows_from_the_laptop() {
    let view = build(&scenario("ui-offline"));
    match &view.machines[1].status {
        Presence::Offline { last_seen, .. } => {
            assert_eq!(last_seen.as_deref(), Some("5 min ago"))
        }
        other => panic!("worker should be offline: {other:?}"),
    }
    // The worker's last known jobs stay visible but marked stale.
    let stale = running(&view, "nixos", 91);
    assert_eq!(
        stale.stale.as_deref(),
        Some("machine offline; last seen 5 min ago")
    );
    // Shown as of the last snapshot, not extrapolated into "overdue".
    assert_eq!(stale.elapsed_text, "7:04 elapsed");
    assert_eq!(
        stale.eta_text,
        "~4 min left (6 similar runs) (at last contact)"
    );
    // remote_job_id 91 still links the pair, so #306 is not shown twice.
    assert!(view.running.iter().all(|r| r.key.id != 306));
}

#[test]
fn unlinked_offload_client_is_shown_from_the_laptop() {
    let mut states = scenario("ui-offline");
    states[1].snapshot = None;
    states[1].error = Some("status-watch exited".into());
    let view = build(&states);
    let client = running(&view, "kurukkan", 306);
    assert_eq!(client.machine, "laptop ➡ worker");
    assert_eq!(client.command, "cargo test --release");
    assert!(
        client
            .offload_note
            .as_deref()
            .unwrap()
            .contains("not visible")
    );
    assert!(matches!(client.cancel, Target::Local { id: 306, .. }));
    assert_eq!(view.machines[1].title, "worker");
}

#[test]
fn idle_cluster_has_only_recent_results() {
    let view = build(&scenario("ui-idle"));
    assert!(view.running.is_empty());
    assert!(view.queued.is_empty());
    assert_eq!(view.machines[1].pressure_text, "ok");
    assert!(view.machines[1].mem_history.is_empty());
    assert_eq!(view.machines[1].cpu_history.len(), 40);
    let ids: Vec<_> = view.recent.iter().map(|r| r.key.id).collect();
    // Newest first; the laptop's offload client #303 is folded into worker #89.
    assert_eq!(ids, vec![89, 302, 301, 300, 64]);
    let passed = &view.recent[0];
    assert_eq!(passed.outcome, Outcome::Passed);
    assert!(
        passed.summary.starts_with("1:12 · warm"),
        "{}",
        passed.summary
    );
    let failed = view.recent.iter().find(|r| r.key.id == 302).unwrap();
    assert!(failed.summary.contains("exit 101"), "{}", failed.summary);
}

#[test]
fn recent_filter_by_owner_machine_and_cancelled() {
    let view = build(&scenario("ui-idle"));
    let ids = |rows: Vec<&RecentRow>| rows.iter().map(|r| r.key.id).collect::<Vec<_>>();
    assert_eq!(
        ids(filter_recent(&view.recent, "", None, false)),
        vec![89, 302, 301, 64]
    );
    assert_eq!(ids(filter_recent(&view.recent, "", None, true)).len(), 5);
    assert_eq!(
        ids(filter_recent(&view.recent, "FAST", None, true)),
        vec![64]
    );
    assert_eq!(
        ids(filter_recent(&view.recent, "", Some("worker"), true)),
        vec![89, 64]
    );
    assert_eq!(
        ids(filter_recent(&view.recent, "lint", Some("laptop"), true)),
        vec![301]
    );
}

#[test]
fn class_handles_unknown_phases_conservatively() {
    let job = |v: Value| serde_json::from_value::<Job>(v).unwrap();
    assert_eq!(
        class(&job(json!({"id": 1, "phase": "queued"}))),
        Class::Proposal
    );
    assert_eq!(
        class(&job(json!({"id": 1, "phase": "preparing"}))),
        Class::Waiting
    );
    assert_eq!(
        class(&job(json!({"id": 1, "phase": "waiting_device"}))),
        Class::Waiting
    );
    assert_eq!(
        class(&job(
            json!({"id": 1, "phase": "syncing", "started_at": 5.0})
        )),
        Class::Running
    );
    assert_eq!(
        class(&job(
            json!({"id": 1, "phase": "new", "finished_at": 5.0, "result": {"exit_code": 0}})
        )),
        Class::Finished(Outcome::Passed)
    );
    assert_eq!(class(&job(json!({"id": 1}))), Class::Waiting);
}

// --- agents --------------------------------------------------------------------

#[test]
fn agent_of_takes_the_owner_up_to_the_at() {
    assert_eq!(
        agent_of("metrics@kurukkan"),
        ("metrics".to_string(), Some("kurukkan".to_string()))
    );
    // No "@": the whole owner is the agent, no host.
    assert_eq!(agent_of("lint-bot"), ("lint-bot".to_string(), None));
    assert_eq!(agent_of("who@"), ("who".to_string(), None));
    assert_eq!(
        agent_of(" a@b@c "),
        ("a".to_string(), Some("b@c".to_string()))
    );
    assert_eq!(agent_of(""), ("unknown".to_string(), None));
    assert_eq!(agent_of("@host").0, "unknown");
}

#[test]
fn agent_colours_are_stable_and_distinct() {
    // Same algorithm as the reference mock: h = h*31 + char, mod 360.
    assert_eq!(agent_hue("a"), 97);
    assert_eq!(agent_hue("metrics"), agent_hue("metrics"));
    assert_ne!(agent_rgb("metrics"), agent_rgb("lead-vp8enc"));
    assert_eq!(hsl_to_rgb(0.0, 1.0, 0.5), [255, 0, 0]);
    assert_eq!(hsl_to_rgb(120.0, 1.0, 0.5), [0, 255, 0]);
    assert_eq!(hsl_to_rgb(240.0, 1.0, 0.5), [0, 0, 255]);
    assert_eq!(hsl_to_rgb(0.0, 0.0, 0.55), [140, 140, 140]);
}

#[test]
fn every_row_names_its_agent_and_details_the_full_owner() {
    let view = build(&scenario("ui-busy"));
    let warm = running(&view, "nixos", 91);
    assert_eq!(
        (warm.agent.as_str(), warm.owner.as_str()),
        ("view-step-up", "view-step-up@kurukkan")
    );
    assert_eq!(warm.details.owner, "view-step-up@kurukkan");
    assert_eq!(warm.via, Some(306));
    // Laptop owners have no "@" in the fixtures.
    assert_eq!(running(&view, "kurukkan", 305).agent, "callui-claude");
    let fast = queued(&view, "nixos", 92);
    assert_eq!(
        (fast.agent.as_str(), fast.via, fast.warmth),
        ("fast-start", Some(307), Warmth::Warm)
    );
    let done = view.recent.iter().find(|r| r.key.id == 89).unwrap();
    assert_eq!(
        (done.agent.as_str(), done.via),
        ("release-worker-device", Some(303))
    );
    assert_eq!(
        (done.wall_text.as_str(), done.warmth),
        ("1:12", Warmth::Warm)
    );
    assert!(done.finished_text.ends_with("ago"));
}

#[test]
fn machine_cards_list_slot_holders_and_agents_here() {
    let view = build(&scenario("ui-busy"));
    let laptop = &view.machines[0];
    assert_eq!(laptop.chips[0].text, "build #305 callui-claude");
    assert!(laptop.chips[0].busy);
    assert_eq!(laptop.chips[0].agent.as_deref(), Some("callui-claude"));
    assert_eq!(laptop.chips[1].text, "run-1");
    assert!(!laptop.chips[1].busy);
    let worker = &view.machines[1];
    assert_eq!(worker.chips[0].text, "build #91 view-step-up");
    assert_eq!(
        worker.chips[0].hover,
        "build: held by view-step-up@kurukkan"
    );
    assert_eq!(
        worker.chips.last().unwrap().text,
        "device-SYNTHPHONE01 #93 release-worker-device"
    );
    // Running and waiting jobs only, first appearance order, no duplicates.
    assert_eq!(
        laptop.agents,
        vec![
            "callui-claude",
            "view-step-up",
            "fast-start",
            "lint-bot",
            "docs-agent"
        ]
    );
    assert_eq!(
        worker.agents,
        vec!["view-step-up", "fast-start", "release-worker-device"]
    );
    let idle = build(&scenario("ui-idle"));
    assert!(idle.machines.iter().all(|m| m.agents.is_empty()));
}

#[test]
fn summary_line_counts_agents_jobs_and_cores() {
    let view = build(&scenario("ui-busy"));
    // Agents: callui-claude, view-step-up, release-worker-device (running),
    // fast-start, lint-bot, docs-agent (queued); the proposal does not count.
    assert_eq!(
        view.summary,
        "6 agents active · 3 running · 3 queued · 13.2 cores busy across 2 machines"
    );
    let idle = build(&scenario("ui-idle"));
    assert_eq!(
        idle.summary,
        "0 agents active · 0 running · 0 queued · 0.4 cores busy across 2 machines"
    );
    let mut offline = scenario("ui-offline");
    offline[1].snapshot = None;
    offline[1].error = Some("gone".into());
    assert!(
        build(&offline)
            .summary
            .ends_with("0.9 cores busy across 1 machine")
    );
}
