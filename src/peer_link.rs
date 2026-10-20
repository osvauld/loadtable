//! Forwarding a live command to a peer daemon: the job contract and the ledger link.
//!
//! A forwarded job is an ordinary `local.command` job on THIS machine whose recipe uses the
//! offload slot and whose argv is `tasks peer-forward --state DIR --peer NAME --who OWNER
//! --resource R [--fetch REL]... -- COMMAND...`. It is submitted, approved, admitted, logged and
//! cancelled like any other job; its command is a thin bridge to this daemon's peer runtime,
//! which syncs the checkout, submits `COMMAND` to the peer's own ledger, streams the peer's log
//! into this job's log and exits with the peer job's exit code. [`forwarded_spec`] is the seam
//! placement/MCP can call: it turns a live-command spec plus a target peer into that job.
//!
//! [`PeerLink`] is what both ledgers record once a forwarded job is terminal: in the execution
//! JSON under `peer` (JSON-level merge like `metrics`; older releases ignore it).

use crate::local_command::{Recipe, Resource};
use crate::{JobSpec, Phase, Result, Store};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `tasks` subcommand that a forwarded job runs.
pub const FORWARD_SUBCOMMAND: &str = "peer-forward";

/// What runs on the peer: the admission class there (never the offload slot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardResource {
    Compile,
    Run,
    Device(String),
}

impl ForwardResource {
    fn text(&self) -> String {
        match self {
            Self::Compile => "compile".into(),
            Self::Run => "run".into(),
            Self::Device(s) => format!("device:{s}"),
        }
    }
    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "compile" => Self::Compile,
            "run" => Self::Run,
            other => match other.strip_prefix("device:") {
                Some(serial) if !serial.is_empty() => Self::Device(serial.into()),
                _ => return Err(format!("invalid --resource {other:?}").into()),
            },
        })
    }
}

/// Arguments of `tasks peer-forward`, parsed identically by the bridge and the daemon (which
/// reads them from the approved ledger record, never from the bridge).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardArgs {
    pub state: PathBuf,
    pub peer: String,
    pub who: String,
    pub resource: ForwardResource,
    pub fetch: Vec<String>,
    pub argv: Vec<String>,
}

fn safe_relpath(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && value.len() <= 1024
        && !value.chars().any(|c| c.is_control() || c == '\\')
        && value.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

impl ForwardArgs {
    pub fn to_argv(&self, exe: &str) -> Vec<String> {
        let mut out = vec![
            exe.to_owned(),
            FORWARD_SUBCOMMAND.to_owned(),
            "--state".into(),
            self.state.display().to_string(),
            "--peer".into(),
            self.peer.clone(),
            "--who".into(),
            self.who.clone(),
            "--resource".into(),
            self.resource.text(),
        ];
        for f in &self.fetch {
            out.push("--fetch".into());
            out.push(f.clone());
        }
        out.push("--".into());
        out.extend(self.argv.iter().cloned());
        out
    }

    /// Parses the arguments after `peer-forward`.
    pub fn parse(args: &[String]) -> Result<Self> {
        let split = args
            .iter()
            .position(|a| a == "--")
            .ok_or("peer-forward requires -- before the command")?;
        let (mut state, mut peer, mut who, mut resource) = (None, None, None, None);
        let mut fetch = Vec::new();
        let mut it = args[..split].iter();
        while let Some(flag) = it.next() {
            let value = it
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?
                .clone();
            match flag.as_str() {
                "--state" => state = Some(PathBuf::from(value)),
                "--peer" => peer = Some(value),
                "--who" => who = Some(value),
                "--resource" => resource = Some(ForwardResource::parse(&value)?),
                "--fetch" => {
                    if !safe_relpath(&value) {
                        return Err(format!(
                            "--fetch must be a normalized relative path: {value:?}"
                        )
                        .into());
                    }
                    fetch.push(value)
                }
                other => return Err(format!("unknown peer-forward option {other:?}").into()),
            }
        }
        let state: PathBuf = state.ok_or("peer-forward needs --state")?;
        if !state.is_absolute() {
            return Err("peer-forward --state must be absolute".into());
        }
        let argv = args[split + 1..].to_vec();
        if argv.is_empty() {
            return Err("peer-forward needs a command after --".into());
        }
        Ok(Self {
            state,
            peer: peer.ok_or("peer-forward needs --peer")?,
            who: who.ok_or("peer-forward needs --who")?,
            resource: resource.unwrap_or(ForwardResource::Compile),
            fetch,
            argv,
        })
    }

    /// The forward arguments of a recipe argv, if it is a forwarded job.
    pub fn from_recipe_argv(argv: &[String]) -> Option<Result<Self>> {
        is_forward_argv(argv).then(|| Self::parse(&argv[2..]))
    }
}

/// `tasks peer-forward ...` as the program (no env prefix: the daemon builds it).
pub fn is_forward_argv(argv: &[String]) -> bool {
    argv.len() >= 2
        && Path::new(&argv[0])
            .file_name()
            .is_some_and(|n| n == "tasks")
        && argv[1] == FORWARD_SUBCOMMAND
}

/// THE seam for placement/MCP: a live-command spec (`local.command`, resource compile/run/
/// device) plus a target peer name becomes the local forwarding job. `state` is the local
/// daemon's state directory and `exe` the `tasks` binary that bridges to it. The result is
/// validated like any submission (device denylist on the whole argv, bounded argv).
pub fn forwarded_spec(
    spec: &JobSpec,
    peer: &str,
    fetch: &[String],
    state: &Path,
    exe: &Path,
) -> Result<JobSpec> {
    let inner = Recipe::from_spec_unvalidated(spec)?;
    let resource = match inner.resource {
        Resource::Compile => ForwardResource::Compile,
        Resource::Run => ForwardResource::Run,
        Resource::Device(serial) => ForwardResource::Device(serial),
        Resource::Offload => {
            return Err("an offload client cannot be forwarded to a peer".into());
        }
        Resource::Service => {
            return Err("a service runs on the machine it was submitted to".into());
        }
    };
    if !task_manager_peer::config::valid_name(peer) {
        return Err(format!("invalid peer name {peer:?}").into());
    }
    let args = ForwardArgs {
        state: state.to_owned(),
        peer: peer.to_owned(),
        who: spec.owner.clone(),
        resource,
        fetch: fetch.to_vec(),
        argv: inner.argv,
    };
    for f in fetch {
        if !safe_relpath(f) {
            return Err(format!("--fetch must be a normalized relative path: {f:?}").into());
        }
    }
    let exe = exe.to_str().ok_or("UTF-8 tasks path required")?;
    Recipe {
        broad_test: inner.broad_test,
        argv: args.to_argv(exe),
        resource: Resource::Offload,
    }
    .spec(spec.owner.clone(), PathBuf::from(&spec.source))
}

/// The link both ledgers record for a forwarded job (execution JSON key `peer`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerLink {
    /// `outbound`: this job forwarded to `peer`; `inbound`: this job was forwarded by `peer`.
    pub direction: String,
    pub peer: String,
    pub peer_endpoint: String,
    /// The job id on the other machine.
    pub peer_job_id: Option<i64>,
    /// `WHO@origin host`, the owner the remote ledger shows.
    pub owner: String,
    /// Idempotency key of the submission.
    pub key: String,
    pub transport: String,
    /// The remote job's last known phase (outbound only).
    #[serde(default)]
    pub remote_phase: Option<String>,
    /// `terminal`, `uncertain`, `not_submitted`, `interrupted` (outbound only).
    #[serde(default)]
    pub remote_state: Option<String>,
    #[serde(default)]
    pub sync_s: Option<f64>,
    #[serde(default)]
    pub fetch_s: Option<f64>,
}

/// Adds `peer` to a TERMINAL job's execution JSON (JSON-level merge, other keys untouched,
/// never overwrites). Returns whether it was written.
pub fn write_peer_link(store: &mut Store, id: i64, link: &PeerLink) -> Result<bool> {
    use rusqlite::{OptionalExtension, TransactionBehavior, params};
    let tx = store
        .0
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let phase: Option<String> = tx
        .query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    if !phase.is_some_and(|p| Phase::from_db(&p).is_ok_and(|p| p.terminal())) {
        return Ok(false);
    }
    let data: Option<String> = tx
        .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(data) = data else { return Ok(false) };
    let mut value: serde_json::Value = serde_json::from_str(&data)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    if object.get("peer").is_some_and(|m| !m.is_null()) {
        return Ok(false);
    }
    object.insert("peer".into(), serde_json::to_value(link)?);
    tx.execute(
        "UPDATE executions SET data=?2 WHERE job_id=?1",
        params![id, serde_json::to_string(&value)?],
    )?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn forward_args_round_trip_and_reject_unsafe_fetch() {
        let args = ForwardArgs {
            state: "/s".into(),
            peer: "worker".into(),
            who: "agent".into(),
            resource: ForwardResource::Device("SER1AL".into()),
            fetch: vec!["out/bin".into()],
            argv: argv(&["sh", "-c", "echo -- hi"]),
        };
        let line = args.to_argv("/usr/bin/tasks");
        assert!(is_forward_argv(&line));
        assert_eq!(ForwardArgs::from_recipe_argv(&line).unwrap().unwrap(), args);
        let mut bad = line.clone();
        bad[11] = "../escape".into();
        assert!(ForwardArgs::from_recipe_argv(&bad).unwrap().is_err());
        assert!(ForwardArgs::from_recipe_argv(&argv(&["tasks", "run"])).is_none());
    }

    #[test]
    fn forwarded_spec_uses_the_offload_slot_and_keeps_owner_and_cwd() {
        let inner = Recipe {
            broad_test: false,
            argv: argv(&["cargo", "test", "-j", "8"]),
            resource: Resource::Run,
        };
        // The inner -j is the PEER's business; the local spec is not a cargo command.
        let spec = JobSpec {
            suite: crate::local_command::SUITE.into(),
            configuration: serde_json::to_string(&inner).unwrap(),
            cost: crate::Cost::CpuSimulation,
            owner: "agent".into(),
            source: "/work/repo".into(),
            revision: "live-workspace".into(),
        };
        let out = forwarded_spec(
            &spec,
            "worker",
            &[],
            Path::new("/st"),
            Path::new("/bin/tasks"),
        )
        .unwrap();
        let recipe: Recipe = serde_json::from_str(&out.configuration).unwrap();
        assert_eq!(recipe.resource, Resource::Offload);
        assert_eq!(out.owner, "agent");
        assert_eq!(out.source, "/work/repo");
        let args = ForwardArgs::from_recipe_argv(&recipe.argv)
            .unwrap()
            .unwrap();
        assert_eq!(args.resource, ForwardResource::Run);
        assert_eq!(args.argv, inner.argv);
        // The denylist still applies to the whole forwarded argv on this host.
        let denied = Recipe {
            broad_test: false,
            argv: argv(&["adb", "-s", "ZD22257P6G", "shell"]),
            resource: Resource::Run,
        };
        let spec = JobSpec {
            configuration: serde_json::to_string(&denied).unwrap(),
            ..spec
        };
        assert!(
            forwarded_spec(
                &spec,
                "worker",
                &[],
                Path::new("/st"),
                Path::new("/bin/tasks")
            )
            .is_err()
        );
    }
}

/// One configured peer, as `tasks peer list` shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerView {
    pub name: String,
    pub endpoint_id: String,
    pub addr: Option<String>,
    pub role: String,
    pub approve: String,
}

/// The daemon's peer runtime: configuration, endpoint and link records.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeersReport {
    pub enabled: bool,
    pub config: Option<String>,
    pub error: Option<String>,
    pub endpoint_id: Option<String>,
    pub bind: Option<String>,
    #[serde(default)]
    pub peers: Vec<PeerView>,
    /// Outbound link records (jobs this daemon forwarded), newest last.
    #[serde(default)]
    pub forwarded: Vec<serde_json::Value>,
    /// Inbound records (jobs peers forwarded here), newest last.
    #[serde(default)]
    pub inbound: Vec<serde_json::Value>,
    /// The latest status/0 snapshot of every worker/both peer, kept by the peer runtime
    /// (read-only; older daemons omit the key). Ages are filled at reply time.
    #[serde(default)]
    pub snapshots: Vec<PeerSnapshot>,
}

/// Link state of a peer's status stream as the runtime last saw it.
pub const STREAM_CONNECTING: &str = "connecting";
pub const STREAM_ONLINE: &str = "online";
pub const STREAM_OFFLINE: &str = "offline";
pub const STREAM_STATUS_ERROR: &str = "status_error";

/// One worker/both peer's latest snapshot (cluster snapshot v1, `history` rings dropped) and
/// how fresh it is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    /// Configured peer name (`[[peer]] name`).
    pub name: String,
    pub role: String,
    /// `connecting` (no attempt finished yet), `online`, `offline`, `status_error` (reachable,
    /// but its status sample failed; the last snapshot is kept).
    pub state: String,
    /// When `state` last changed (Unix seconds).
    pub since: f64,
    /// The last connection or status error, if any.
    #[serde(default)]
    pub error: Option<String>,
    /// When the last live snapshot arrived (this machine's clock).
    #[serde(default)]
    pub received_at: Option<f64>,
    /// Seconds since `received_at`, at reply time.
    #[serde(default)]
    pub age_s: Option<f64>,
    /// No live snapshot, or older than `stale_after_s`, at reply time.
    #[serde(default)]
    pub stale: bool,
    pub stale_after_s: f64,
    #[serde(default)]
    pub snapshot: Option<serde_json::Value>,
}

impl PeerSnapshot {
    /// The stream is up (a live snapshot arrived and no loss was seen since).
    pub fn online(&self) -> bool {
        matches!(self.state.as_str(), STREAM_ONLINE | STREAM_STATUS_ERROR)
    }
    /// Fills `age_s`/`stale` for a reply at `now`.
    pub fn refresh(&mut self, now: f64) {
        self.age_s = self.received_at.map(|t| (now - t).max(0.0));
        self.stale = self.age_s.is_none_or(|age| age > self.stale_after_s);
    }
}

impl PeersReport {
    /// Fills every snapshot's age and stale flag for a reply at `now`.
    pub fn refresh(&mut self, now: f64) {
        for s in &mut self.snapshots {
            s.refresh(now);
        }
    }
}

/// Reply to `Action::Peers`, a separate shape like `StatusResponse`. A daemon that predates
/// peers answers the action with an `unknown variant` error in `error`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PeersResponse {
    pub version: u32,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub peers: Option<PeersReport>,
}

/// `Action::Peers` on the daemon socket. An older daemon's `unknown variant` answer becomes a
/// clear error naming the fallback (the daemon predates peer forwarding).
#[cfg(unix)]
pub fn request_peers(path: &Path, timeout: std::time::Duration) -> Result<PeersReport> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let request = serde_json::to_vec(&crate::Request {
        version: crate::PROTOCOL_VERSION,
        action: crate::Action::Peers,
    })?;
    stream.write_all(&request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream.take(4 * 1024 * 1024)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err("incomplete peers reply from the daemon".into());
    }
    let reply: PeersResponse = serde_json::from_str(&line)?;
    match (reply.peers, reply.error) {
        (Some(report), _) => Ok(report),
        (None, Some(e)) if e.contains("unknown variant") => Err(format!(
            "the running daemon predates peer forwarding (it answered: {e}); a release with \
             `tasks peer` is needed on this machine, nothing was submitted"
        )
        .into()),
        (None, e) => Err(e.unwrap_or_else(|| "empty peers reply".into()).into()),
    }
}

/// Log search on peer `name` through this daemon's peer runtime (`<state>/peer.sock`,
/// `search_logs` bridge request -> the peer's `jobs/0` `search_logs`). Returns the peer's
/// `log_search::Report` as JSON. Fails when the peer link is not running or the peer refuses.
#[cfg(unix)]
pub fn request_peer_search(
    state_dir: &Path,
    name: &str,
    query: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Read, Write};
    let socket = state_dir.join("peer.sock");
    let mut stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|e| format!("{}: {e} (is the peer link enabled?)", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let request = serde_json::json!({"op": "search_logs", "peer": name, "query": query});
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream.take(8 * 1024 * 1024)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(format!("incomplete search reply for peer {name}").into());
    }
    let event: serde_json::Value = serde_json::from_str(&line)?;
    match event["event"].as_str() {
        Some("line") => Ok(event["value"].clone()),
        Some("error") => Err(event["message"]
            .as_str()
            .unwrap_or("peer search failed")
            .to_owned()
            .into()),
        _ => Err(format!("unexpected bridge reply: {line}").into()),
    }
}
