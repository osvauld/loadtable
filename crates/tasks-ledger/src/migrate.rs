//! Ordered schema migrations keyed by `PRAGMA user_version`.

use crate::{LedgerError, Result};
use rusqlite::{Connection, TransactionBehavior};

/// The first schema: jobs as one JSON spec plus dependency edges.
const V1: &str = "
CREATE TABLE IF NOT EXISTS jobs(
    id INTEGER PRIMARY KEY,
    spec TEXT NOT NULL,
    phase TEXT NOT NULL,
    submitted_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    machine TEXT
);
CREATE TABLE IF NOT EXISTS job_deps(
    job INTEGER NOT NULL,
    after INTEGER NOT NULL,
    PRIMARY KEY(job, after)
);
";

/// Full job record, indexed phase tag, plans and timing samples.
const V2: &str = "
ALTER TABLE jobs ADD COLUMN owner TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN argv TEXT NOT NULL DEFAULT '[]';
ALTER TABLE jobs ADD COLUMN hints TEXT NOT NULL DEFAULT '{}';
ALTER TABLE jobs ADD COLUMN plan_id INTEGER;
ALTER TABLE jobs ADD COLUMN step_name TEXT;
ALTER TABLE jobs ADD COLUMN phase_tag TEXT NOT NULL DEFAULT 'queued';
ALTER TABLE jobs ADD COLUMN wait_text TEXT;
ALTER TABLE jobs ADD COLUMN exit_code INTEGER;
ALTER TABLE jobs ADD COLUMN signal INTEGER;
ALTER TABLE jobs ADD COLUMN timeout TEXT;
ALTER TABLE jobs ADD COLUMN summary TEXT;
ALTER TABLE jobs ADD COLUMN lease_pgid INTEGER;
ALTER TABLE jobs ADD COLUMN lease_start INTEGER;
ALTER TABLE jobs ADD COLUMN log_path TEXT;
ALTER TABLE jobs ADD COLUMN fwd_peer TEXT;
ALTER TABLE jobs ADD COLUMN fwd_job INTEGER;
ALTER TABLE jobs ADD COLUMN remote_phase TEXT;
ALTER TABLE jobs ADD COLUMN cancel_by TEXT;
ALTER TABLE jobs ADD COLUMN cancel_at_ms INTEGER;
UPDATE jobs SET
    owner = COALESCE(json_extract(spec, '$.owner'), ''),
    cwd = COALESCE(json_extract(spec, '$.cwd'), ''),
    argv = COALESCE(json_extract(spec, '$.argv'), '[]'),
    phase_tag = CASE
        WHEN phase = '\"Queued\"' THEN 'queued'
        WHEN phase LIKE '{\"Waiting\"%' THEN 'waiting'
        WHEN phase = '\"Running\"' THEN 'running'
        WHEN phase = '\"Passed\"' THEN 'passed'
        WHEN phase LIKE '{\"Failed\"%' THEN 'failed'
        WHEN phase LIKE '{\"TimedOut\"%' THEN 'timed_out'
        WHEN phase = '\"Cancelled\"' THEN 'cancelled'
        ELSE 'interrupted' END;
UPDATE jobs SET exit_code = 0 WHERE phase_tag = 'passed';
CREATE INDEX jobs_phase ON jobs(phase_tag, id);
CREATE INDEX jobs_spec ON jobs(owner, cwd, submitted_at_ms);
CREATE INDEX job_deps_after ON job_deps(after);
CREATE TABLE plans(
    id INTEGER PRIMARY KEY,
    owner TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE samples(
    id INTEGER PRIMARY KEY,
    recipe TEXT NOT NULL,
    target TEXT,
    machine TEXT NOT NULL,
    secs REAL NOT NULL,
    cache TEXT NOT NULL,
    outcome TEXT NOT NULL,
    at_ms INTEGER NOT NULL
);
CREATE INDEX samples_key ON samples(recipe, machine, target, at_ms);
";

/// Peer forwarding that survives a restart: the duplicate-submit key of a forwarded job and the
/// (peer, key) a received job came in under, unique so a repeat can never start a second job.
const V3: &str = "
ALTER TABLE jobs ADD COLUMN fwd_key TEXT;
ALTER TABLE jobs ADD COLUMN origin_peer TEXT;
ALTER TABLE jobs ADD COLUMN origin_key TEXT;
CREATE UNIQUE INDEX jobs_origin ON jobs(origin_peer, origin_key) WHERE origin_peer IS NOT NULL;
";

/// Why a job was cancelled, when the requester said.
const V4: &str = "
ALTER TABLE jobs ADD COLUMN cancel_reason TEXT;
";

/// Trust: paired peers, grants (issued and held, never deleted: revoking changes `state`), spent
/// invite nonces and small named values such as the ticket epoch.
const V5: &str = "
CREATE TABLE trust_peers(
    name TEXT PRIMARY KEY,
    did TEXT NOT NULL,
    node_id TEXT NOT NULL,
    relay TEXT,
    addrs TEXT NOT NULL DEFAULT '[]',
    paired_ms INTEGER NOT NULL
);
CREATE TABLE trust_grants(
    id TEXT PRIMARY KEY,
    direction TEXT NOT NULL,
    peer_did TEXT NOT NULL,
    peer_node TEXT NOT NULL,
    role TEXT NOT NULL,
    nonce TEXT NOT NULL,
    iat INTEGER NOT NULL,
    exp INTEGER NOT NULL,
    token TEXT NOT NULL,
    state TEXT NOT NULL,
    state_at_ms INTEGER NOT NULL
);
CREATE INDEX trust_grants_peer ON trust_grants(peer_did, direction);
CREATE TABLE trust_spent_nonces(
    nonce TEXT PRIMARY KEY,
    spent_at_ms INTEGER NOT NULL
);
CREATE TABLE trust_meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// The audit chain (design §8): append-only rows, each carrying the hash of the one before.
const V6: &str = "
CREATE TABLE audit(
    seq INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    actor TEXT NOT NULL,
    owner TEXT NOT NULL,
    action TEXT NOT NULL,
    job INTEGER,
    peer TEXT,
    target TEXT,
    details TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL
);
CREATE INDEX audit_at ON audit(at_ms);
CREATE INDEX audit_job ON audit(job) WHERE job IS NOT NULL;
CREATE INDEX audit_peer ON audit(peer) WHERE peer IS NOT NULL;
CREATE INDEX audit_action ON audit(action, seq);
";

/// The migrations in order; entry `i` upgrades version `i` to `i + 1`.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = 6;

fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Brings the database up to `migrations.len()`. A database already at a newer version is
/// refused untouched. Each step runs in its own immediate transaction that re-reads the version,
/// so two processes opening together apply each step once.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let supported = i64::try_from(migrations.len()).unwrap_or(i64::MAX);
    let found = version(conn)?;
    if found > supported {
        return Err(LedgerError::SchemaTooNew { found, supported });
    }
    for (i, sql) in migrations.iter().enumerate() {
        let target = i64::try_from(i + 1).unwrap_or(i64::MAX);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version(&tx)? >= target {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uv(c: &Connection) -> i64 {
        version(c).unwrap()
    }

    #[test]
    fn fresh_database_reaches_the_current_version() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, MIGRATIONS).unwrap();
        assert_eq!(uv(&c), SCHEMA_VERSION);
        assert_eq!(i64::try_from(MIGRATIONS.len()).unwrap(), SCHEMA_VERSION);
        migrate(&mut c, MIGRATIONS).unwrap();
        assert_eq!(uv(&c), SCHEMA_VERSION);
    }

    #[test]
    fn a_v3_database_upgrades_to_v4_and_keeps_its_rows() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &MIGRATIONS[..3]).unwrap();
        c.execute(
            "INSERT INTO jobs(spec, phase, phase_tag, submitted_at_ms, owner, cwd, argv, hints)
             VALUES ('{}', '\"Queued\"', 'queued', 1, 'o', '/', '[]', '{}')",
            [],
        )
        .unwrap();
        migrate(&mut c, MIGRATIONS).unwrap();
        assert_eq!(uv(&c), SCHEMA_VERSION);
        let reason: Option<String> = c
            .query_row("SELECT cancel_reason FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reason, None);
    }

    // R-238
    #[test]
    fn r238_a_v4_database_upgrades_to_v5_and_keeps_its_jobs() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &MIGRATIONS[..4]).unwrap();
        assert_eq!(uv(&c), 4);
        c.execute(
            "INSERT INTO jobs(spec, phase, phase_tag, submitted_at_ms, owner, cwd, argv, hints)
             VALUES ('{}', '\"Queued\"', 'queued', 1, 'o', '/', '[]', '{}')",
            [],
        )
        .unwrap();
        migrate(&mut c, MIGRATIONS).unwrap();
        assert_eq!(uv(&c), SCHEMA_VERSION);
        let jobs: i64 = c
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jobs, 1);
        for table in [
            "trust_peers",
            "trust_grants",
            "trust_spent_nonces",
            "trust_meta",
        ] {
            let n: i64 = c
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }

    // R-269
    #[test]
    fn r269_a_v5_database_upgrades_to_v6_and_keeps_its_jobs_and_trust_rows() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &MIGRATIONS[..5]).unwrap();
        assert_eq!(uv(&c), 5);
        c.execute(
            "INSERT INTO jobs(spec, phase, phase_tag, submitted_at_ms, owner, cwd, argv, hints)
             VALUES ('{}', '\"Queued\"', 'queued', 1, 'o', '/', '[]', '{}')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO trust_peers(name, did, node_id, relay, addrs, paired_ms)
             VALUES ('nixos', 'did:key:zX', 'ab', NULL, '[]', 7)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO trust_grants(id, direction, peer_did, peer_node, role, nonce, iat, exp,
                 token, state, state_at_ms)
             VALUES ('g', 'issued', 'did:key:zX', 'ab', 'submit', 'n', 1, 2, 'g1.x', 'live', 3)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO trust_spent_nonces(nonce, spent_at_ms) VALUES ('n', 4)",
            [],
        )
        .unwrap();
        migrate(&mut c, MIGRATIONS).unwrap();
        assert_eq!(uv(&c), 6);
        for (table, want) in [
            ("jobs", 1),
            ("trust_peers", 1),
            ("trust_grants", 1),
            ("trust_spent_nonces", 1),
            ("audit", 0),
        ] {
            let n: i64 = c
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, want, "{table}");
        }
    }

    #[test]
    fn steps_apply_in_order_and_only_once() {
        let mut c = Connection::open_in_memory().unwrap();
        let steps = [
            "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);",
            "UPDATE t SET a = a + 10;",
        ];
        migrate(&mut c, &steps[..1]).unwrap();
        assert_eq!(uv(&c), 1);
        migrate(&mut c, &steps).unwrap();
        migrate(&mut c, &steps).unwrap();
        let a: i64 = c.query_row("SELECT a FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(a, 11);
        assert_eq!(uv(&c), 2);
    }

    #[test]
    fn a_failing_step_rolls_back_and_keeps_the_old_version() {
        let mut c = Connection::open_in_memory().unwrap();
        let steps = [
            "CREATE TABLE t(a);",
            "CREATE TABLE u(a); SELECT nonsense FROM nowhere;",
        ];
        assert!(migrate(&mut c, &steps).is_err());
        assert_eq!(uv(&c), 1);
        let u: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'u'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(u, 0);
    }

    #[test]
    fn a_newer_database_is_refused_and_left_alone() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        match migrate(&mut c, MIGRATIONS) {
            Err(LedgerError::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (SCHEMA_VERSION + 5, SCHEMA_VERSION));
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
        assert_eq!(uv(&c), SCHEMA_VERSION + 5);
        let tables: i64 = c
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 0);
    }
}
