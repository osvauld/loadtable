//! The SQLite [`AuditLedger`]. Rows are only ever inserted; the single delete is retention's
//! prefix removal, in the same transaction as the record that accounts for it.

use crate::audit::{
    ACTION_CHECKPOINT, AuditLedger, AuditPruned, AuditQuery, AuditRow, NewAudit, seal,
};
use crate::sqlite::SqliteStore;
use crate::{LedgerError, Result};
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};

const COLS: &str = "seq, at_ms, actor, owner, action, job, peer, target, details, prev_hash, hash";

fn row_of(r: &Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        seq: r.get(0)?,
        at_ms: r.get(1)?,
        actor: r.get(2)?,
        owner: r.get(3)?,
        action: r.get(4)?,
        job: r.get(5)?,
        peer: r.get(6)?,
        target: r.get(7)?,
        details: r.get(8)?,
        prev_hash: r.get(9)?,
        hash: r.get(10)?,
    })
}

fn head_on(tx: &Transaction<'_>) -> Result<Option<AuditRow>> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLS} FROM audit ORDER BY seq DESC LIMIT 1"),
            [],
            row_of,
        )
        .optional()?)
}

fn insert(tx: &Transaction<'_>, r: &AuditRow) -> Result<()> {
    tx.execute(
        &format!("INSERT INTO audit({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
        params![
            r.seq,
            r.at_ms,
            r.actor,
            r.owner,
            r.action,
            r.job,
            r.peer,
            r.target,
            r.details,
            r.prev_hash,
            r.hash
        ],
    )?;
    Ok(())
}

impl AuditLedger for SqliteStore {
    fn append_with(
        &self,
        make: &dyn Fn(Option<&AuditRow>) -> Result<NewAudit>,
    ) -> Result<AuditRow> {
        self.write(|tx, _| {
            let head = head_on(tx)?;
            let row = seal(head.as_ref(), &make(head.as_ref())?);
            insert(tx, &row)?;
            Ok(row)
        })
    }

    fn query(&self, q: &AuditQuery) -> Result<Vec<AuditRow>> {
        let mut wh = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(t) = q.since_ms {
            wh.push("at_ms >= ?");
            args.push(t.into());
        }
        if let Some(t) = q.until_ms {
            wh.push("at_ms <= ?");
            args.push(t.into());
        }
        if let Some(p) = &q.peer {
            wh.push("(peer = ? OR actor = ?)");
            args.push(p.clone().into());
            args.push(p.clone().into());
        }
        if let Some(j) = q.job {
            wh.push("job = ?");
            args.push(j.into());
        }
        if let Some(a) = &q.action {
            wh.push("(action = ? OR substr(action, 1, ?) = ?)");
            let prefix = format!("{a}.");
            args.push(a.clone().into());
            args.push(
                i64::try_from(prefix.chars().count())
                    .unwrap_or(i64::MAX)
                    .into(),
            );
            args.push(prefix.into());
        }
        let wh = if wh.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wh.join(" AND "))
        };
        args.push(i64::try_from(q.limit).unwrap_or(i64::MAX).into());
        let conn = self.lock()?;
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM audit {wh} ORDER BY seq DESC LIMIT ?"
        ))?;
        let rows = st.query_map(params_from_iter(args), row_of)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn scan(&self, after_seq: i64, limit: usize) -> Result<Vec<AuditRow>> {
        let conn = self.lock()?;
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM audit WHERE seq > ?1 ORDER BY seq LIMIT ?2"
        ))?;
        let rows = st.query_map(
            params![after_seq, i64::try_from(limit).unwrap_or(i64::MAX)],
            row_of,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn head(&self) -> Result<Option<AuditRow>> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLS} FROM audit ORDER BY seq DESC LIMIT 1"),
                [],
                row_of,
            )
            .optional()?)
    }

    fn last_checkpoint_seq(&self) -> Result<Option<i64>> {
        let conn = self.lock()?;
        Ok(conn.query_row(
            "SELECT MAX(seq) FROM audit WHERE action = ?1",
            params![ACTION_CHECKPOINT],
            |r| r.get(0),
        )?)
    }

    fn prune_before(
        &self,
        cutoff_ms: i64,
        make: &dyn Fn(&AuditRow, u64) -> Result<NewAudit>,
    ) -> Result<Option<AuditPruned>> {
        self.write(|tx, _| {
            // the prefix: every row before the first one that is recent enough
            let first_kept: Option<i64> = tx.query_row(
                "SELECT MIN(seq) FROM audit WHERE at_ms >= ?1",
                params![cutoff_ms],
                |r| r.get(0),
            )?;
            let last = match first_kept {
                Some(k) => tx
                    .query_row(
                        &format!(
                            "SELECT {COLS} FROM audit WHERE seq < ?1 ORDER BY seq DESC LIMIT 1"
                        ),
                        params![k],
                        row_of,
                    )
                    .optional()?,
                None => head_on(tx)?,
            };
            let Some(last) = last else {
                return Ok(None);
            };
            let rows: i64 = tx.query_row(
                "SELECT count(*) FROM audit WHERE seq <= ?1",
                params![last.seq],
                |r| r.get(0),
            )?;
            let head = head_on(tx)?;
            let record = seal(
                head.as_ref(),
                &make(&last, u64::try_from(rows).unwrap_or(0))?,
            );
            insert(tx, &record)?;
            tx.execute("DELETE FROM audit WHERE seq <= ?1", params![last.seq])?;
            Ok(Some(AuditPruned {
                rows: u64::try_from(rows).map_err(|_| LedgerError::OutOfRange("audit rows"))?,
                through_seq: last.seq,
                record_seq: record.seq,
            }))
        })
    }
}
