//! The daemon's ledger listing, kept per ledger generation instead of rebuilt per request.
//!
//! Every `list`, `status` and the per-request reconcile check used to read and parse the
//! whole job history (and, for `list`, all runtime estimates) and serialise it again. With
//! ~730 jobs that was ~1 MB of JSON per request; an idle window or a waiting client polling
//! every few seconds kept the daemon at a few percent of a core. [`Listing`] keeps the parsed
//! jobs, the estimates and their serialised JSON until the ledger changes.
//!
//! Generation: SQLite's `PRAGMA data_version` changes whenever ANOTHER connection (worker,
//! stage supervisors, sampler, health, other processes) committed; this connection's own
//! writes show up in `total_changes()`. Both are read per request (microseconds); anything
//! different reloads everything. A failed read never serves a stale cache.
use crate::{Job, PROTOCOL_VERSION, Phase, Response, Result, RuntimeEstimate, Store, execution};
use std::sync::Arc;

/// What identifies one committed ledger state as seen by one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation {
    data_version: i64,
    total_changes: u64,
}

impl Store {
    /// The ledger generation as seen by this connection (see the module docs).
    pub fn generation(&self) -> Result<Generation> {
        let data_version = self
            .0
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))?;
        Ok(Generation {
            data_version,
            total_changes: self.0.total_changes(),
        })
    }
}

#[derive(Default)]
pub struct Listing {
    generation: Option<Generation>,
    jobs: Arc<Vec<Job>>,
    estimates: Arc<Vec<RuntimeEstimate>>,
    jobs_json: Option<Arc<str>>,
    estimates_json: Option<Arc<str>>,
    /// How often the ledger was actually re-read (tests, benches).
    pub reloads: u64,
}

impl Listing {
    /// Re-reads jobs and estimates when the ledger changed since the last call.
    pub fn refresh(&mut self, store: &Store) -> Result<()> {
        let generation = store.generation()?;
        if self.generation == Some(generation) {
            return Ok(());
        }
        // Clear first: an error below must not leave an older listing marked current.
        self.generation = None;
        self.jobs_json = None;
        self.estimates_json = None;
        let jobs = store.jobs()?;
        let estimates = store.estimates()?;
        self.jobs = Arc::new(jobs);
        self.estimates = Arc::new(estimates);
        self.generation = Some(generation);
        self.reloads += 1;
        Ok(())
    }

    /// The whole job history (cheap clone).
    pub fn jobs(&self) -> Arc<Vec<Job>> {
        self.jobs.clone()
    }

    /// Jobs that have not finished (queued, approved, active), oldest first.
    pub fn unfinished(&self) -> Vec<Job> {
        self.jobs
            .iter()
            .filter(|j| !j.phase.terminal())
            .cloned()
            .collect()
    }

    /// Unfinished jobs plus the newest `n` finished ones.
    pub fn recent(&self, n: usize) -> Vec<Job> {
        crate::recent((*self.jobs).clone(), n)
    }

    pub fn any_active(&self) -> bool {
        self.jobs.iter().any(|j| j.phase.active())
    }

    pub fn job(&self, id: i64) -> Option<Job> {
        self.jobs.iter().find(|j| j.id == id).cloned()
    }

    pub fn phase_of(&self, id: i64) -> Option<Phase> {
        self.jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.phase.clone())
    }

    /// The `list` reply as one JSON line (with `\n`), byte-identical to serialising
    /// [`Response`] with the full listing; the arrays come from the cache.
    pub fn list_reply(&mut self, execution_enabled: bool) -> Result<Vec<u8>> {
        if self.jobs_json.is_none() {
            self.jobs_json = Some(serde_json::to_string(&*self.jobs)?.into());
        }
        if self.estimates_json.is_none() {
            self.estimates_json = Some(serde_json::to_string(&*self.estimates)?.into());
        }
        let jobs = self.jobs_json.as_deref().unwrap_or("[]");
        let estimates = self.estimates_json.as_deref().unwrap_or("[]");
        let mut out = Vec::with_capacity(jobs.len() + estimates.len() + 128);
        out.extend_from_slice(b"{\"version\":");
        out.extend_from_slice(PROTOCOL_VERSION.to_string().as_bytes());
        out.extend_from_slice(b",\"execution_enabled\":");
        out.extend_from_slice(if execution_enabled { b"true" } else { b"false" });
        out.extend_from_slice(b",\"jobs\":");
        out.extend_from_slice(jobs.as_bytes());
        out.extend_from_slice(b",\"estimates\":");
        out.extend_from_slice(estimates.as_bytes());
        out.extend_from_slice(b",\"error\":null,\"log\":null}\n");
        Ok(out)
    }

    /// A reply carrying only `jobs` (no estimates): what `list_active`, `logs` and the
    /// worker switches answer with.
    pub fn reply_with(jobs: Vec<Job>, log: Option<execution::LogTail>) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            execution_enabled: false,
            submitted_id: None,
            jobs,
            estimates: Vec::new(),
            error: None,
            log,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cost, JobSpec, Request, handle};

    fn spec(owner: &str) -> JobSpec {
        JobSpec {
            suite: "local.command".into(),
            owner: owner.into(),
            source: "/tmp/synthetic-checkout".into(),
            revision: "live-workspace".into(),
            configuration: r#"{"argv":["true"],"resource":"run"}"#.into(),
            cost: Cost::CpuSimulation,
        }
    }

    fn temp_db(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tasks-listing-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tasks.sqlite")
    }

    #[test]
    fn list_reply_is_byte_identical_to_the_serialised_response() {
        let mut store = Store::memory().unwrap();
        for owner in ["synthetic-a", "synthetic-b", "synthetic-c"] {
            store.submit(&spec(owner)).unwrap();
        }
        store
            .record_runtime("local.command", "{}", "build", 12.5)
            .unwrap();
        let mut listing = Listing::default();
        listing.refresh(&store).unwrap();
        for enabled in [false, true] {
            let mut expected = handle(
                &mut store,
                Request {
                    version: PROTOCOL_VERSION,
                    action: crate::Action::List,
                },
            );
            expected.execution_enabled = enabled;
            let mut expected = serde_json::to_vec(&expected).unwrap();
            expected.push(b'\n');
            assert_eq!(
                String::from_utf8(listing.list_reply(enabled).unwrap()).unwrap(),
                String::from_utf8(expected).unwrap()
            );
        }
    }

    #[test]
    fn reloads_only_when_this_or_another_connection_commits() {
        let path = temp_db("generation");
        let mut daemon = Store::open(&path).unwrap();
        let mut other = Store::open(&path).unwrap();
        let mut listing = Listing::default();
        listing.refresh(&daemon).unwrap();
        for _ in 0..50 {
            listing.refresh(&daemon).unwrap();
        }
        assert_eq!(listing.reloads, 1, "an unchanged ledger is read once");
        assert!(listing.jobs().is_empty());

        // A commit on this connection.
        let own = daemon.submit(&spec("synthetic-own")).unwrap();
        listing.refresh(&daemon).unwrap();
        assert_eq!(listing.reloads, 2);
        assert_eq!(listing.phase_of(own.id), Some(Phase::Queued));

        // A commit by another connection (the worker, a stage supervisor, ...).
        let theirs = other.submit(&spec("synthetic-other")).unwrap();
        listing.refresh(&daemon).unwrap();
        assert_eq!(listing.reloads, 3);
        assert_eq!(listing.jobs().len(), 2);
        other.cancel(theirs.id).unwrap();
        listing.refresh(&daemon).unwrap();
        assert_eq!(listing.phase_of(theirs.id), Some(Phase::Cancelled));

        // The cached reply follows the reload, too.
        let reply: Response = serde_json::from_slice(&listing.list_reply(false).unwrap()).unwrap();
        assert_eq!(reply.jobs.len(), 2);
        // Estimates recorded by another connection invalidate as well.
        other
            .record_runtime("local.command", "{}", "build", 3.0)
            .unwrap();
        listing.refresh(&daemon).unwrap();
        let reply: Response = serde_json::from_slice(&listing.list_reply(false).unwrap()).unwrap();
        assert_eq!(reply.estimates.len(), 1);
        drop((daemon, other));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unfinished_keeps_queued_approved_and_active_only() {
        let mut store = Store::memory().unwrap();
        let queued = store.submit(&spec("synthetic-q")).unwrap();
        let cancelled = store.submit(&spec("synthetic-c")).unwrap();
        store.cancel(cancelled.id).unwrap();
        let mut listing = Listing::default();
        listing.refresh(&store).unwrap();
        let ids: Vec<i64> = listing.unfinished().iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![queued.id]);
        assert!(!listing.any_active());
    }
}
