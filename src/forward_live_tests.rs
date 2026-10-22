use super::*;
use serde_json::{Value, json};

const FORWARD: [&str; 4] = ["/x/tasks", "peer-forward", "--", "cargo"];

fn job(id: i64, phase: &str, argv: &[&str], live: Option<Value>, offload: Option<Value>) -> Value {
    json!({
        "id": id, "owner": "o", "phase": phase, "resource": "offload", "device": null,
        "checkout": "/c", "checkout_name": "c", "command": "cargo test", "kind_key": "k",
        "argv": argv, "submitted_at": 1, "started_at": 2, "finished_at": null,
        "queue": null, "live": live, "progress": null, "warmth": "unknown",
        "warmth_reason": "", "estimate": null, "result": null, "offload": offload,
    })
}

fn views(jobs: Vec<Value>) -> Vec<JobView> {
    jobs.into_iter()
        .map(|j| serde_json::from_value(j).unwrap())
        .collect()
}

fn live(cores: f64, rss: u64) -> Value {
    json!({"cpu_cores": cores, "rss_bytes": rss, "processes": 7})
}

fn link(worker: &str, remote: Option<i64>) -> Value {
    json!({"worker": worker, "remote_job_id": remote, "transport": "iroh",
           "sync_s": null, "fetch_s": null})
}

fn peer(name: &str, state: &str, stale: bool, jobs: Vec<Value>) -> PeerSnapshot {
    PeerSnapshot {
        name: name.into(),
        role: "worker".into(),
        state: state.into(),
        stale,
        stale_after_s: 30.0,
        snapshot: Some(json!({
            "version": 1, "taken_at": 1.0,
            "machine": {"id": name, "role": "worker", "logical_cpus": 12,
                        "cpu_busy_cores": null, "slots": [], "swap_total_bytes": null,
                        "swap_used_bytes": null, "pressure": null, "blocker": null, "mem_total_bytes": null, "tasks_job": null},
            "jobs": jobs,
        })),
        ..PeerSnapshot::default()
    }
}

fn remote_job(id: i64, phase: &str) -> Value {
    let mut j = job(
        id,
        phase,
        &["cargo", "test"],
        Some(live(11.6, 9 << 30)),
        None,
    );
    j["progress"] = json!({"crates_compiled": 300, "crates_expected": 600,
                           "test_targets_built": 4, "test_targets_expected": 11});
    j
}

#[test]
fn a_forwarded_job_takes_the_peers_cpu_ram_and_progress() {
    let mut jobs = views(vec![job(
        4196,
        "running",
        &FORWARD,
        Some(live(0.005, 46_788_608)),
        Some(link("nixos", Some(1405))),
    )]);
    let peers = [peer(
        "nixos",
        "online",
        false,
        vec![remote_job(1405, "running")],
    )];
    assert_eq!(overlay(&mut jobs, &peers), [(4196, Source::Peer)]);
    let l = jobs[0].live.as_ref().unwrap();
    assert_eq!(
        (l.cpu_cores, l.rss_bytes, l.processes),
        (Some(11.6), 9 << 30, 7)
    );
    let p = jobs[0].progress.as_ref().unwrap();
    assert_eq!((p.crates_compiled, p.test_targets_built), (300, Some(4)));
}

#[test]
fn without_fresh_peer_data_the_proxys_zero_is_dropped_not_shown() {
    let linked = || Some(link("nixos", Some(1405)));
    let sampled = || Some(live(0.005, 46_788_608));
    let cases = [
        (peer("nixos", "offline", false, vec![]), "offline"),
        (
            peer("nixos", "online", true, vec![remote_job(1405, "running")]),
            "stale",
        ),
        (
            peer("nixos", "online", false, vec![remote_job(7, "running")]),
            "job not listed",
        ),
        (
            peer("nixos", "online", false, vec![remote_job(1405, "passed")]),
            "already finished",
        ),
        (
            peer("other", "online", false, vec![remote_job(1405, "running")]),
            "other peer",
        ),
    ];
    for (p, why) in cases {
        let mut jobs = views(vec![job(4196, "running", &FORWARD, sampled(), linked())]);
        assert_eq!(overlay(&mut jobs, &[p]), [(4196, Source::Unknown)], "{why}");
        assert!(jobs[0].live.is_none(), "{why}");
    }
    let mut jobs = views(vec![job(
        4198,
        "running",
        &FORWARD,
        sampled(),
        Some(link("nixos", None)),
    )]);
    let peers = [peer(
        "nixos",
        "online",
        false,
        vec![remote_job(1405, "running")],
    )];
    assert_eq!(
        overlay(&mut jobs, &peers),
        [(4198, Source::Unknown)],
        "no remote id yet"
    );
}

#[test]
fn other_jobs_are_left_alone() {
    let peers = [peer(
        "nixos",
        "online",
        false,
        vec![remote_job(1405, "running")],
    )];
    let mut jobs = views(vec![
        job(
            1,
            "running",
            &["cargo", "test"],
            Some(live(2.0, 1 << 30)),
            None,
        ),
        job(2, "passed", &FORWARD, None, Some(link("nixos", Some(1405)))),
        job(
            3,
            "running",
            &["/x/tasks", "run"],
            Some(live(1.0, 1 << 20)),
            Some(link("nixos", Some(1405))),
        ),
    ]);
    assert!(overlay(&mut jobs, &peers).is_empty());
    assert_eq!(jobs[0].live.as_ref().unwrap().cpu_cores, Some(2.0));
    assert_eq!(jobs[2].live.as_ref().unwrap().cpu_cores, Some(1.0));
}
