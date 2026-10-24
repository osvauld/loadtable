//! The run-alone, light and retry flags travel with the job spec to the peer (R-712, R-714,
//! R-716), and an older sender that knows none of them is still understood.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;
use common::*;
use std::time::Duration;
use tasks_core::JobSpec;
use tasks_peer::proto::{Body, SubmitJob};
use tasks_peer::transport::Transport;
use tasks_peer::{CheckoutId, Forward, ForwardJob, SubmitKey};

#[test]
fn r712_the_isolate_light_and_retry_flags_reach_the_job_the_peer_runs() {
    let w = World::new();
    let mut spec = w.spec("");
    spec.isolate = true;
    spec.retry_alone = true;
    let job = ForwardJob {
        local_id: 1,
        spec,
        root: w.root.clone(),
        origin_host: "laptop".into(),
        peer: w.nixos.clone(),
        key: ForwardJob::default_key("laptop", 1).unwrap(),
    };
    let mut f = Forward::new(job, fast_policy(), w.locks.clone(), Duration::ZERO);
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 3, |_, _| {});
    let sent = w.runner.submitted();
    assert_eq!(sent.len(), 1);
    let spec = &sent[0].1;
    assert!(spec.isolate && spec.retry_alone && !spec.light, "{spec:?}");

    // a light job likewise
    let mut light = w.spec("");
    light.light = true;
    let r = w
        .transport
        .connect(&w.nixos, Duration::from_secs(5))
        .unwrap()
        .request(Body::Submit(SubmitJob {
            key: SubmitKey::new("laptop:2").unwrap(),
            checkout: CheckoutId::derive("laptop", &w.root),
            rel_cwd: String::new(),
            spec: light,
            shared_checkout: false,
        }));
    assert!(r.is_ok(), "{r:?}");
    assert!(w.runner.submitted()[1].1.light);
}

#[test]
fn r712_a_spec_from_an_older_sender_without_the_flags_is_all_false_and_default_flags_are_not_sent()
{
    let w = World::new();
    let spec = w.spec("");
    let mut v = serde_json::to_value(&spec).unwrap();
    for k in ["isolate", "light", "retry_alone"] {
        assert!(v.get(k).is_none(), "{k} is not written when false");
        v.as_object_mut().unwrap().remove(k);
    }
    let back: JobSpec = serde_json::from_value(v).unwrap();
    assert!(!back.isolate && !back.light && !back.retry_alone);
}
