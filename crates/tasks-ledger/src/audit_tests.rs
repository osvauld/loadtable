use super::*;
use crate::audit::{
    GENESIS_HASH, MemoryAudit, ProblemKind, SigVerdict, SignatureCheck, verify_chain,
};
use crate::{ManualClock, SqliteStore};
use tasks_identity::Identity;

fn ident(n: u8) -> Arc<Identity> {
    Arc::new(Identity::from_entropy(&[n; 32]).unwrap())
}

/// A log over each store kind, with a manual clock and a signer; tests run against both.
fn logs() -> Vec<(&'static str, Arc<AuditLog>, Arc<ManualClock>)> {
    let stores: Vec<(&'static str, Arc<dyn AuditLedger>)> = vec![
        ("sqlite", Arc::new(SqliteStore::open_in_memory().unwrap())),
        ("memory", Arc::new(MemoryAudit::new())),
    ];
    stores
        .into_iter()
        .map(|(n, s)| {
            let clock = Arc::new(ManualClock::new(1_000));
            let log = AuditLog::new(s, clock.clone(), "did:key:zMe").with_signer(ident(1));
            log.set_checkpoint_every(0);
            (n, Arc::new(log), clock)
        })
        .collect()
}

fn fill(log: &AuditLog, clock: &ManualClock, n: usize) {
    for i in 0..n {
        clock.advance(10);
        log.record(
            AuditEvent::new(action::JOB_SUBMITTED)
                .job(i as i64)
                .owner("agent")
                .detail("n", i as u64),
        );
    }
}

fn kinds(log: &AuditLog) -> Option<ProblemKind> {
    log.verify().unwrap().problem.map(|p| p.kind)
}

// R-270
#[test]
fn r270_a_clean_chain_verifies_and_starts_from_genesis() {
    for (name, log, clock) in logs() {
        assert!(log.verify().unwrap().ok(), "{name}: empty");
        fill(&log, &clock, 5);
        let rep = log.verify().unwrap();
        assert!(rep.ok(), "{name}: {rep:?}");
        assert_eq!(
            (rep.rows, rep.first_seq, rep.last_seq),
            (5, Some(1), Some(5))
        );
        let rows = log.store().scan(0, 10).unwrap();
        assert_eq!(rows[0].prev_hash, GENESIS_HASH);
        for w in rows.windows(2) {
            assert_eq!(w[1].prev_hash, w[0].hash, "{name}");
        }
    }
}

// R-270
#[test]
fn r270_an_edited_row_breaks_the_chain_at_that_row() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 6);
    store.tamper(|rows| rows[2].owner = "someone-else".into());
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (3, ProblemKind::Edited));
}

// R-270
#[test]
fn r270_an_edit_in_sqlite_is_found_and_recomputing_one_hash_does_not_hide_it() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 5);
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE audit SET action = 'job.cancelled' WHERE seq = 2",
            [],
        )
        .unwrap();
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (2, ProblemKind::Edited));
    // the forger fixes row 2's own hash; row 3 no longer follows it
    let row = log.store().scan(1, 1).unwrap().remove(0);
    let fixed = crate::audit::hash_of(
        row.seq,
        &NewAudit {
            at_ms: row.at_ms,
            actor: row.actor.clone(),
            owner: row.owner.clone(),
            action: row.action.clone(),
            job: row.job,
            peer: row.peer.clone(),
            target: row.target.clone(),
            details: row.details.clone(),
        },
        &row.prev_hash,
    );
    store
        .lock()
        .unwrap()
        .execute("UPDATE audit SET hash = ?1 WHERE seq = 2", [fixed])
        .unwrap();
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (3, ProblemKind::BrokenLink));
}

// R-271
#[test]
fn r271_a_deleted_row_is_a_gap_and_a_deleted_head_row_is_missing_history() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 6);
    store.tamper(|rows| {
        rows.remove(3);
    });
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (5, ProblemKind::Gap));
    assert!(p.message.contains("4"), "{}", p.message);

    // the same through SQL
    let sql = Arc::new(SqliteStore::open_in_memory().unwrap());
    let log = AuditLog::new(sql.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 6);
    sql.lock()
        .unwrap()
        .execute("DELETE FROM audit WHERE seq = 3", [])
        .unwrap();
    assert_eq!(kinds(&log), Some(ProblemKind::Gap));
    sql.lock()
        .unwrap()
        .execute("DELETE FROM audit WHERE seq < 3", [])
        .unwrap();
    // rows 1 and 2 are gone as well: the oldest survivor has no prune record behind it
    assert_eq!(kinds(&log), Some(ProblemKind::MissingHistory));
}

// R-271
#[test]
fn r271_deleting_the_oldest_rows_without_a_prune_record_is_found() {
    for (name, log, clock) in logs() {
        fill(&log, &clock, 5);
        // drop the first two rows the way retention would, but without its record
        let store = log.store().clone();
        let before = store.scan(0, 10).unwrap();
        assert_eq!(before.len(), 5);
        let cutoff = before[1].at_ms + 1;
        // a forged prune: the rows go, no record is written
        let forged = store
            .prune_before(cutoff, &|_, _| {
                Ok(NewAudit {
                    action: "job.submitted".into(),
                    details: "{}".into(),
                    ..NewAudit::default()
                })
            })
            .unwrap();
        assert!(forged.is_some(), "{name}");
        let p = log.verify().unwrap().problem.unwrap();
        assert_eq!(p.kind, ProblemKind::MissingHistory, "{name}: {p:?}");
    }
}

// R-272
#[test]
fn r272_reordered_rows_are_found() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 6);
    store.tamper(|rows| rows.swap(2, 3));
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!(p.seq, 4);
    assert!(
        matches!(p.kind, ProblemKind::Gap | ProblemKind::Reordered),
        "{p:?}"
    );

    // swapping the sequence numbers of two rows in SQL leaves each row's hash wrong
    let sql = Arc::new(SqliteStore::open_in_memory().unwrap());
    let log = AuditLog::new(sql.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 6);
    {
        let c = sql.lock().unwrap();
        c.execute("UPDATE audit SET seq = -1 WHERE seq = 3", [])
            .unwrap();
        c.execute("UPDATE audit SET seq = 3 WHERE seq = 4", [])
            .unwrap();
        c.execute("UPDATE audit SET seq = 4 WHERE seq = -1", [])
            .unwrap();
    }
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (3, ProblemKind::Edited));
}

// R-273
#[test]
fn r273_pruning_writes_a_record_and_verify_works_from_the_oldest_survivor() {
    for (name, log, clock) in logs() {
        fill(&log, &clock, 10); // at 1010..1100
        log.checkpoint().unwrap();
        clock.set(1_100 + 5 * 86_400_000);
        fill(&log, &clock, 3);
        // keep two days: the first 11 rows (10 + checkpoint) are older
        let pruned = log.prune(Duration::from_secs(2 * 86_400)).unwrap().unwrap();
        assert_eq!((pruned.rows, pruned.through_seq), (11, 11), "{name}");
        let rows = log.store().scan(0, 100).unwrap();
        assert_eq!(rows.first().unwrap().seq, 12, "{name}");
        assert_eq!(rows.last().unwrap().action, ACTION_PRUNED, "{name}");
        let rec = rows.last().unwrap().details_json();
        assert_eq!(rec["through_seq"], 11);
        assert_eq!(rec["through_hash"], serde_json::json!(rows[0].prev_hash));
        let rep = log.verify().unwrap();
        assert!(rep.ok(), "{name}: {rep:?}");
        assert_eq!(rep.first_seq, Some(12));
        assert_eq!(rep.pruned_before, Some(12));
        // new rows keep chaining after the prune record
        fill(&log, &clock, 2);
        assert!(log.verify().unwrap().ok(), "{name}");
        // nothing left to prune
        assert_eq!(log.prune(Duration::from_secs(2 * 86_400)).unwrap(), None);
    }
}

// R-273
#[test]
fn r273_a_second_prune_anchors_on_its_own_record() {
    for (name, log, clock) in logs() {
        fill(&log, &clock, 4);
        clock.advance(10 * 86_400_000);
        fill(&log, &clock, 4);
        assert!(log.prune(Duration::from_secs(86_400)).unwrap().is_some());
        clock.advance(10 * 86_400_000);
        fill(&log, &clock, 2);
        let second = log.prune(Duration::from_secs(86_400)).unwrap().unwrap();
        assert!(second.rows > 0, "{name}");
        let rep = log.verify().unwrap();
        assert!(rep.ok(), "{name}: {rep:?}");
    }
}

// R-273
#[test]
fn r273_a_prune_record_naming_the_wrong_hash_is_found() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 4);
    clock.advance(10 * 86_400_000);
    fill(&log, &clock, 2);
    log.prune(Duration::from_secs(86_400)).unwrap().unwrap();
    assert!(log.verify().unwrap().ok());
    // the record's signature covers its hash, so editing it is caught as an edit first
    store.tamper(|rows| {
        let last = rows.last_mut().unwrap();
        last.details = last.details.replace("through_hash", "through_hasX");
    });
    assert_eq!(kinds(&log), Some(ProblemKind::Edited));
}

// R-274
#[test]
fn r274_a_checkpoint_is_signed_by_the_machine_key_and_verifies() {
    for (name, log, clock) in logs() {
        fill(&log, &clock, 3);
        let ck = log.checkpoint().unwrap().unwrap();
        assert_eq!(ck.action, ACTION_CHECKPOINT);
        let d = ck.details_json();
        assert_eq!(d["head_seq"], 3, "{name}");
        assert_eq!(d["head_hash"], serde_json::json!(ck.prev_hash));
        assert_eq!(d["signer"], serde_json::json!(ident(1).did()));
        let rep = log.verify().unwrap();
        assert!(rep.ok(), "{name}: {rep:?}");
        assert_eq!((rep.checkpoints, rep.signed_through), (1, Some(4)));
        fill(&log, &clock, 2);
        assert_eq!(log.verify().unwrap().signed_through, Some(4), "{name}");
    }
}

// R-274
#[test]
fn r274_a_checkpoint_is_written_every_n_rows_and_the_count_survives_a_reopen() {
    let store: Arc<dyn AuditLedger> = Arc::new(SqliteStore::open_in_memory().unwrap());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    log.set_checkpoint_every(4);
    fill(&log, &clock, 3);
    assert_eq!(store.last_checkpoint_seq().unwrap(), None);
    fill(&log, &clock, 1);
    assert_eq!(store.last_checkpoint_seq().unwrap(), Some(5));
    fill(&log, &clock, 2);
    // a new log over the same store knows two rows are pending
    let again = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    again.set_checkpoint_every(4);
    fill(&again, &clock, 2);
    assert_eq!(store.last_checkpoint_seq().unwrap(), Some(10));
    assert!(again.verify().unwrap().ok());
}

// R-275
#[test]
fn r275_a_forged_or_foreign_signature_is_a_named_failure() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(ident(1));
    fill(&log, &clock, 3);
    log.checkpoint().unwrap();
    assert!(log.verify().unwrap().ok());

    // a checkpoint whose signature bytes were replaced, with the row's hash fixed up to match:
    // only the signature check can catch it
    let mut forged = store.scan(0, 10).unwrap();
    let ck = forged.pop().unwrap();
    let mut d = ck.details_json();
    d["sig"] = serde_json::json!("00".repeat(64));
    let new = NewAudit {
        at_ms: ck.at_ms,
        actor: ck.actor.clone(),
        owner: String::new(),
        action: ck.action.clone(),
        job: None,
        peer: None,
        target: None,
        details: d.to_string(),
    };
    let fixed = crate::audit::seal(forged.last(), &new);
    store.tamper(|rows| {
        rows.pop();
        rows.push(fixed);
    });
    let p = log.verify().unwrap().problem.unwrap();
    assert_eq!((p.seq, p.kind), (4, ProblemKind::BadSignature));
}

// R-275
#[test]
fn r275_a_checkpoint_signed_by_another_machine_is_refused_when_pinned() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let other = AuditLog::new(store.clone(), clock.clone(), "did:key:zOther").with_signer(ident(2));
    fill(&other, &clock, 3);
    other.checkpoint().unwrap();
    assert!(other.verify().unwrap().ok());
    // read by a log that expects machine 1's key
    let me = AuditLog::new(store, clock, "did:key:zMe").with_signer(ident(1));
    let p = me.verify().unwrap().problem.unwrap();
    assert_eq!(p.kind, ProblemKind::UnexpectedSigner);
    // unpinned, the signature itself is fine
    let rep = verify_chain(&**me.store(), &DidSignatures { expected: None }).unwrap();
    assert!(rep.ok());
    assert_eq!(
        DidSignatures {
            expected: Some("did:key:zA")
        }
        .check("did:key:zB", b"m", "00"),
        SigVerdict::WrongSigner
    );
}

// R-275
#[test]
fn r275_an_unsigned_daemon_writes_checkpoints_that_vouch_for_nothing() {
    let store = Arc::new(MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(5));
    let log = AuditLog::new(store, clock.clone(), "unsigned-machine");
    fill(&log, &clock, 3);
    log.checkpoint().unwrap();
    let rep = log.verify().unwrap();
    assert!(rep.ok(), "{rep:?}");
    assert_eq!((rep.checkpoints, rep.signed_through), (0, None));
}

// R-276
#[test]
fn r276_secrets_are_never_stored_in_details() {
    for (name, log, _clock) in logs() {
        let token = "g1.eyJzZWNyZXQiOiJ0b2tlbiJ9.c2lnbmF0dXJl";
        let ticket = "tasks1.eyJ2ZXJzaW9uIjoxfQ";
        let row = log
            .try_record(
                AuditEvent::new(action::GRANT_ISSUED)
                    .detail("role", "submit")
                    .detail("token", "anything")
                    .detail("invite_token", "i1.abc.def")
                    .detail("note", token)
                    .detail(
                        "nested",
                        json!({"mnemonic": "word word word", "ok": ticket}),
                    )
                    .detail("list", json!(["fine", "i1.xyz.abc"]))
                    .detail("Private_Key", "k"),
            )
            .unwrap();
        for needle in ["anything", token, ticket, "word word", "i1.", "\"k\""] {
            assert!(
                !row.details.contains(needle),
                "{name}: {needle} in {}",
                row.details
            );
        }
        assert!(row.details.contains("\"role\":\"submit\""), "{name}");
        assert!(row.details.contains("[redacted]"), "{name}");
        // and the stored text equals what was hashed
        assert!(log.verify().unwrap().ok(), "{name}");
    }
}

// R-276
#[test]
fn r276_details_are_bounded() {
    let (_, log, _) = logs().remove(0);
    let long = "x".repeat(900);
    let row = log
        .try_record(AuditEvent::new(action::JOB_CANCELLED).detail("reason", long.as_str()))
        .unwrap();
    assert!(row.details.len() < 600, "{}", row.details.len());
    let many: Vec<String> = (0..400)
        .map(|i| format!("item-{i}-{}", "y".repeat(20)))
        .collect();
    let row = log
        .try_record(AuditEvent::new(action::JOB_CANCELLED).detail("many", json!(many)))
        .unwrap();
    assert!(
        row.details.contains("\"truncated\":true"),
        "{}",
        row.details
    );
    assert!(row.details.len() <= MAX_DETAILS_BYTES);
}

// R-277
#[test]
fn r277_a_repeating_event_is_throttled_and_the_next_row_counts_what_was_dropped() {
    let t = Throttle::new(60_000, 1);
    assert_eq!(t.admit("k", 0), Some(0));
    for i in 1..100 {
        assert_eq!(t.admit("k", i * 100), None);
    }
    // another key is unaffected
    assert_eq!(t.admit("other", 5_000), Some(0));
    // the window ends: the next one gets through and reports the 99 dropped
    assert_eq!(t.admit("k", 60_000), Some(99));
    assert_eq!(t.admit("k", 60_001), None);

    let t = Throttle::new(1_000, 3);
    let admitted = (0..50).filter(|i| t.admit("k", *i).is_some()).count();
    assert_eq!(admitted, 3);
}

// R-295
#[test]
fn r295_queries_filter_by_time_job_peer_and_action_family() {
    for (name, log, clock) in logs() {
        clock.set(100);
        log.record(AuditEvent::new(action::TICKET_MINTED).detail("a", 1));
        clock.set(200);
        log.record(AuditEvent::new(action::GRANT_ISSUED).peer("did:key:zP"));
        clock.set(300);
        log.record(
            AuditEvent::new(action::JOB_STARTED)
                .job(7)
                .peer("did:key:zP"),
        );
        clock.set(400);
        log.record(
            AuditEvent::new(action::JOB_ENDED)
                .job(7)
                .by_actor("did:key:zP"),
        );
        clock.set(500);
        log.record(AuditEvent::new(action::JOB_STARTED).job(8));
        let q = |f: &dyn Fn(&mut AuditQuery)| {
            let mut q = AuditQuery {
                limit: 100,
                ..AuditQuery::default()
            };
            f(&mut q);
            log.query(&q)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(q(&|_| {}), vec![5, 4, 3, 2, 1], "{name}: newest first");
        assert_eq!(q(&|q| q.limit = 2), vec![5, 4], "{name}");
        assert_eq!(q(&|q| q.since_ms = Some(300)), vec![5, 4, 3], "{name}");
        assert_eq!(q(&|q| q.until_ms = Some(200)), vec![2, 1], "{name}");
        assert_eq!(q(&|q| q.job = Some(7)), vec![4, 3], "{name}");
        // the peer is matched as the subject or as the actor
        assert_eq!(
            q(&|q| q.peer = Some("did:key:zP".into())),
            vec![4, 3, 2],
            "{name}"
        );
        assert_eq!(
            q(&|q| q.action = Some("job".into())),
            vec![5, 4, 3],
            "{name}"
        );
        assert_eq!(
            q(&|q| q.action = Some("job.started".into())),
            vec![5, 3],
            "{name}"
        );
        // "job" is a family, not a string prefix: "jobx.y" does not match
        assert_eq!(
            q(&|q| q.action = Some("jo".into())),
            Vec::<i64>::new(),
            "{name}"
        );
    }
}

// R-281
#[test]
fn r281_a_failing_store_does_not_fail_the_caller_and_the_error_is_kept() {
    struct Broken;
    impl AuditLedger for Broken {
        fn append_with(
            &self,
            _: &dyn Fn(Option<&AuditRow>) -> Result<NewAudit>,
        ) -> Result<AuditRow> {
            Err(LedgerError::Poisoned)
        }
        fn query(&self, _: &AuditQuery) -> Result<Vec<AuditRow>> {
            Ok(vec![])
        }
        fn scan(&self, _: i64, _: usize) -> Result<Vec<AuditRow>> {
            Ok(vec![])
        }
        fn head(&self) -> Result<Option<AuditRow>> {
            Ok(None)
        }
        fn last_checkpoint_seq(&self) -> Result<Option<i64>> {
            Ok(None)
        }
        fn prune_before(
            &self,
            _: i64,
            _: &dyn Fn(&AuditRow, u64) -> Result<NewAudit>,
        ) -> Result<Option<crate::AuditPruned>> {
            Ok(None)
        }
    }
    let log = AuditLog::new(Arc::new(Broken), Arc::new(ManualClock::new(1)), "me");
    assert_eq!(log.last_error(), None);
    log.record(AuditEvent::new(action::JOB_STARTED));
    assert!(log.last_error().is_some());
}

#[test]
fn every_known_action_is_unique_and_dotted() {
    let mut seen = std::collections::BTreeSet::new();
    for a in action::KNOWN {
        assert!(a.contains('.'), "{a}");
        assert!(seen.insert(*a), "{a} twice");
    }
}

// R-291
#[test]
fn r291_rows_persist_across_a_reopen_and_the_chain_continues() {
    let dir = std::env::temp_dir().join(format!("audit-reopen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("l.db");
    let clock = Arc::new(ManualClock::new(10));
    {
        let s: Arc<dyn AuditLedger> = Arc::new(SqliteStore::open(&path).unwrap());
        let log = AuditLog::new(s, clock.clone(), "me").with_signer(ident(1));
        fill(&log, &clock, 3);
    }
    let s: Arc<dyn AuditLedger> = Arc::new(SqliteStore::open(&path).unwrap());
    let log = AuditLog::new(s, clock.clone(), "me").with_signer(ident(1));
    fill(&log, &clock, 2);
    let rep = log.verify().unwrap();
    assert!(rep.ok(), "{rep:?}");
    assert_eq!(rep.rows, 5);
    let _ = std::fs::remove_dir_all(&dir);
}
