//! `tasks2 identity`, `pair` and `peers`: this machine's identity and its paired machines.
//!
//! `identity` reads the keystore in the state directory and needs no daemon. The rest ask the
//! daemon, which owns the ledger and the endpoint. Secrets: the backup phrase is printed only to
//! a terminal, never as JSON, never in a log, and tickets (which are not secrets but expire) go
//! to standard output alone so a script can capture them.

use crate::args::{IdentityCommand, PairCommand, PeersCommand};
use crate::commands::Ctx;
use crate::error::{CliError, EXIT_FAILURE, EXIT_OK, EXIT_USAGE};
use std::io::Write;
use std::path::Path;
use tasks_daemon::api::{Request, Response};
use tasks_daemon::config::{ConfigArgs, DaemonConfig};
use tasks_daemon::trust_control::PeerTrustView;
use tasks_identity::{Keystore, read_public};

/// Parses `90s`, `15m`, `2h` or a bare number of seconds.
pub fn parse_ttl(text: &str) -> Result<u64, CliError> {
    let t = text.trim();
    let bad = || {
        CliError::Usage(format!(
            "{text:?} is not a duration; write 90s, 15m or 2h (a bare number is seconds)"
        ))
    };
    let (num, unit) = match t.chars().last() {
        Some('s') => (t.strip_suffix('s'), 1),
        Some('m') => (t.strip_suffix('m'), 60),
        Some('h') => (t.strip_suffix('h'), 3600),
        Some(c) if c.is_ascii_digit() => (Some(t), 1),
        _ => (None, 1),
    };
    let n: u64 = num.and_then(|n| n.trim().parse().ok()).ok_or_else(bad)?;
    n.checked_mul(unit).filter(|s| *s > 0).ok_or_else(bad)
}

/// `tasks2 identity show`. Returns the exit code.
pub fn identity(
    action: &IdentityCommand,
    state_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    tty: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let IdentityCommand::Show { mnemonic, json } = action;
    let args = ConfigArgs {
        state_dir: state_dir.map(Path::to_path_buf),
        ..ConfigArgs::default()
    };
    let cfg = match DaemonConfig::resolve(&args, &|k| env(k)) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "tasks2: cannot work out the state directory ({e})");
            return EXIT_USAGE;
        }
    };
    if *mnemonic && (*json || !tty) {
        let _ = writeln!(
            err,
            "tasks2: the backup phrase is printed only to a terminal, and never as JSON; \
             run `tasks2 identity show --mnemonic` in a terminal"
        );
        return EXIT_USAGE;
    }
    let info = match read_public(&cfg.state_dir) {
        Ok(i) => i,
        Err(_) if !Keystore::path_in(&cfg.state_dir).exists() => {
            let _ = writeln!(
                err,
                "tasks2: this machine has no identity yet; it is created when the daemon first \
                 starts (`tasks2 daemon serve`, or any command that starts it)"
            );
            return EXIT_FAILURE;
        }
        Err(e) => {
            let _ = writeln!(err, "tasks2: {e}");
            return EXIT_FAILURE;
        }
    };
    if *json {
        let v = serde_json::json!({
            "name": cfg.machine,
            "did": info.did,
            "node_id": info.node_id,
        });
        let _ = writeln!(out, "{v}");
        return EXIT_OK;
    }
    let _ = writeln!(out, "name     {}", cfg.machine);
    let _ = writeln!(out, "did      {}", info.did);
    let _ = writeln!(out, "node id  {}", info.node_id);
    if *mnemonic {
        let ks = match Keystore::load(&Keystore::path_in(&cfg.state_dir)) {
            Ok(k) => k,
            Err(e) => {
                let _ = writeln!(err, "tasks2: {e}");
                return EXIT_FAILURE;
            }
        };
        let phrase = match ks.mnemonic() {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(err, "tasks2: {e}");
                return EXIT_FAILURE;
            }
        };
        let _ = writeln!(
            err,
            "WARNING: anyone with these 24 words can act as this machine. Write them down, keep \
             them offline, and never paste them into a chat, a ticket or a log."
        );
        let _ = writeln!(out, "backup   {}", phrase.as_str());
    }
    EXIT_OK
}

/// `tasks2 pair …`.
pub fn pair(ctx: &mut Ctx<'_>, action: &PairCommand) -> Result<i32, CliError> {
    match action {
        PairCommand::Invite {
            role,
            both_ways,
            ttl,
        } => {
            let roles: Vec<String> = role
                .split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect();
            if roles.is_empty() {
                return Err(CliError::Usage(
                    "name at least one role: --role submit,observe".into(),
                ));
            }
            let ttl_secs = ttl.as_deref().map(parse_ttl).transpose()?;
            let Response::Invite(v) = ctx.client.call(&Request::PairInvite {
                roles,
                both_ways: *both_ways,
                ttl_secs,
            })?
            else {
                return Err(CliError::Failed("unexpected reply to pair invite".into()));
            };
            writeln!(ctx.out, "{}", v.ticket)?;
            let left = v
                .expires_at
                .saturating_sub(u64::try_from((ctx.now_ms)() / 1000).unwrap_or(0));
            writeln!(
                ctx.err,
                "ticket for {}{}: works once, expires in {}. On the other machine run:\n  \
                 tasks2 pair accept TICKET",
                v.roles.join(","),
                if v.both_ways { ", both ways" } else { "" },
                human_secs(left)
            )?;
            Ok(EXIT_OK)
        }
        PairCommand::Accept { ticket } => {
            let Response::Paired(p) = ctx.client.call(&Request::PairAccept {
                ticket: ticket.trim().to_string(),
            })?
            else {
                return Err(CliError::Failed("unexpected reply to pair accept".into()));
            };
            writeln!(ctx.out, "paired with {} ({})", p.name, p.did)?;
            writeln!(ctx.err, "see the grants with: tasks2 peers")?;
            Ok(EXIT_OK)
        }
        PairCommand::Reset => {
            let Response::PairReset { epoch } = ctx.client.call(&Request::PairReset)? else {
                return Err(CliError::Failed("unexpected reply to pair reset".into()));
            };
            writeln!(
                ctx.out,
                "every unredeemed ticket is now invalid (ticket epoch {epoch})"
            )?;
            Ok(EXIT_OK)
        }
    }
}

/// `tasks2 peers` and `tasks2 peers revoke NAME`.
pub fn peers(
    ctx: &mut Ctx<'_>,
    action: Option<&PeersCommand>,
    json: bool,
) -> Result<i32, CliError> {
    if let Some(PeersCommand::Revoke { name }) = action {
        let Response::Revoked(r) = ctx
            .client
            .call(&Request::PeerRevoke { name: name.clone() })?
        else {
            return Err(CliError::Failed("unexpected reply to peers revoke".into()));
        };
        writeln!(
            ctx.out,
            "revoked {} grant(s) of {}; {} live connection(s) closed",
            r.revoked, r.name, r.closed
        )?;
        return Ok(EXIT_OK);
    }
    let Response::Peers { peers } = ctx.client.call(&Request::Peers)? else {
        return Err(CliError::Failed("unexpected reply to peers".into()));
    };
    if json {
        let text = serde_json::to_string(&peers)
            .map_err(|e| CliError::Failed(format!("cannot render JSON: {e}")))?;
        writeln!(ctx.out, "{text}")?;
        return Ok(EXIT_OK);
    }
    if peers.is_empty() {
        writeln!(
            ctx.out,
            "no paired machines; pair one with `tasks2 pair invite` here and `tasks2 pair accept` there"
        )?;
        return Ok(EXIT_OK);
    }
    let now = (ctx.now_ms)() / 1000;
    let rows: Vec<[String; 7]> = peers.iter().map(|p| peer_row(p, now)).collect();
    let head = [
        "NAME", "DID", "WE MAY", "THEY MAY", "EXPIRES", "PATH", "RTT",
    ];
    let mut widths = head.map(str::len);
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r.iter()) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(widths.iter())
            .map(|(c, w)| format!("{c:<w$}", w = *w))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    writeln!(ctx.out, "{}", line(&head.map(str::to_string)))?;
    for r in &rows {
        writeln!(ctx.out, "{}", line(r))?;
    }
    Ok(EXIT_OK)
}

fn peer_row(p: &PeerTrustView, now: i64) -> [String; 7] {
    let roles = |v: &[tasks_daemon::trust_control::RoleView]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.iter()
                .map(|r| r.role.as_str())
                .collect::<Vec<_>>()
                .join(",")
        }
    };
    let expires = if p.revoked {
        "revoked".to_string()
    } else {
        match p.expires_at {
            Some(e) => {
                let left = i64::try_from(e).unwrap_or(i64::MAX) - now;
                if left <= 0 {
                    "expired".to_string()
                } else {
                    format!("in {}", human_secs(u64::try_from(left).unwrap_or(0)))
                }
            }
            None => "-".to_string(),
        }
    };
    [
        p.name.clone(),
        short_did(&p.did),
        roles(&p.roles_out),
        roles(&p.roles_in),
        expires,
        p.path.clone(),
        p.rtt_ms
            .map_or_else(|| "-".to_string(), |r| format!("{r} ms")),
    ]
}

/// `did:key:z6MkhaXg…WRuf`: enough to tell machines apart in a table.
pub fn short_did(did: &str) -> String {
    let n = did.chars().count();
    if n <= 24 {
        return did.to_string();
    }
    let head: String = did.chars().take(16).collect();
    let tail: String = did.chars().skip(n - 6).collect();
    format!("{head}…{tail}")
}

/// `2 d 3 h`, `14 m`, `40 s`.
pub fn human_secs(s: u64) -> String {
    if s >= 2 * 86_400 {
        format!("{} d", s / 86_400)
    } else if s >= 3600 {
        format!("{} h", s / 3600)
    } else if s >= 60 {
        format!("{} m", s / 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeClient;
    use crate::testutil::{Harness, NOW};
    use std::sync::Arc;
    use tasks_daemon::trust_control::{
        InviteView, PairedView, PeerTrustView, RevokeView, RoleView,
    };

    fn peer(
        name: &str,
        out: &[&str],
        inn: &[&str],
        exp: Option<u64>,
        revoked: bool,
    ) -> PeerTrustView {
        let roles = |v: &[&str]| {
            v.iter()
                .map(|r| RoleView {
                    role: (*r).into(),
                    expires_at: exp.unwrap_or(0),
                })
                .collect()
        };
        PeerTrustView {
            name: name.into(),
            did: "did:key:z6MknncVmedphgDSbQoYLyNaFRFdB2WtyjxcFdKV6zTfcYvs".into(),
            node_id: "ab".repeat(32),
            roles_out: roles(out),
            roles_in: roles(inn),
            expires_at: exp,
            revoked,
            path: "direct".into(),
            rtt_ms: Some(3),
        }
    }

    // R-264
    #[test]
    fn r264_durations_parse_in_seconds_minutes_and_hours() {
        assert_eq!(parse_ttl("90s").unwrap(), 90);
        assert_eq!(parse_ttl("15m").unwrap(), 900);
        assert_eq!(parse_ttl("2h").unwrap(), 7200);
        assert_eq!(parse_ttl("45").unwrap(), 45);
        for bad in [
            "",
            "m",
            "0",
            "0m",
            "-5m",
            "1d",
            "abc",
            "1.5h",
            "99999999999999999999h",
        ] {
            assert!(matches!(parse_ttl(bad), Err(CliError::Usage(_))), "{bad}");
        }
    }

    // R-265
    #[test]
    fn r265_pair_invite_prints_only_the_ticket_on_stdout_and_sends_the_roles() {
        let client = Arc::new(FakeClient::from_fn(|req| {
            assert!(matches!(req, Request::PairInvite { .. }));
            Ok(Response::Invite(InviteView {
                ticket: "tasks1.abc".into(),
                roles: vec!["submit".into(), "observe".into()],
                both_ways: true,
                expires_at: NOW as u64 / 1000 + 900,
            }))
        }));
        let h = Harness::default();
        let r = h.run(
            &client,
            &[
                "pair",
                "invite",
                "--role",
                "submit,observe",
                "--both-ways",
                "--ttl",
                "15m",
            ],
        );
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(r.out, "tasks1.abc\n", "stdout carries the ticket alone");
        assert!(
            r.err.contains("works once") && r.err.contains("15 m"),
            "{}",
            r.err
        );
        assert!(
            r.err.contains("both ways") && r.err.contains("tasks2 pair accept"),
            "{}",
            r.err
        );
        match &client.requests()[0] {
            Request::PairInvite {
                roles,
                both_ways,
                ttl_secs,
            } => {
                assert_eq!(roles, &["submit", "observe"]);
                assert!(*both_ways);
                assert_eq!(*ttl_secs, Some(900));
            }
            other => panic!("{other:?}"),
        }
    }

    // R-265
    #[test]
    fn r265_pair_invite_defaults_and_bad_input_never_reach_the_daemon() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Invite(InviteView {
                ticket: "tasks1.x".into(),
                roles: vec!["submit".into(), "observe".into()],
                both_ways: false,
                expires_at: NOW as u64 / 1000 + 900,
            }))
        }));
        let h = Harness::default();
        assert_eq!(h.run(&client, &["pair", "invite"]).code, 0);
        match &client.requests()[0] {
            Request::PairInvite {
                roles,
                both_ways,
                ttl_secs,
            } => {
                assert_eq!(roles, &["submit", "observe"]);
                assert!(!*both_ways && ttl_secs.is_none());
            }
            other => panic!("{other:?}"),
        }
        let r = h.run(&client, &["pair", "invite", "--ttl", "soon"]);
        assert_eq!(r.code, 2);
        assert!(r.err.contains("not a duration"), "{}", r.err);
        let r = h.run(&client, &["pair", "invite", "--role", ","]);
        assert_eq!(r.code, 2);
        assert_eq!(client.requests().len(), 1);
    }

    // R-266
    #[test]
    fn r266_pair_accept_and_reset_report_what_happened() {
        let client = Arc::new(FakeClient::from_fn(|req| match req {
            Request::PairAccept { ticket } => {
                assert_eq!(ticket, "tasks1.abc", "surrounding space is trimmed");
                Ok(Response::Paired(PairedView {
                    name: "nixos".into(),
                    did: "did:key:zN".into(),
                    node_id: "cd".repeat(32),
                }))
            }
            Request::PairReset => Ok(Response::PairReset { epoch: 4 }),
            other => panic!("{other:?}"),
        }));
        let h = Harness::default();
        let r = h.run(&client, &["pair", "accept", " tasks1.abc "]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(r.out, "paired with nixos (did:key:zN)\n");
        let r = h.run(&client, &["pair", "reset"]);
        assert!(r.out.contains("ticket epoch 4"), "{}", r.out);
    }

    // R-266
    #[test]
    fn r266_a_daemon_refusal_reaches_the_user_with_its_reason() {
        use tasks_daemon::api::ErrorCode;
        let client = Arc::new(FakeClient::from_fn(|_| {
            Err(crate::testutil::api_err(
                ErrorCode::Refused,
                "the ticket was already redeemed",
            ))
        }));
        let r = Harness::default().run(&client, &["pair", "accept", "tasks1.abc"]);
        assert_eq!(r.code, 2);
        assert!(r.err.contains("already redeemed"), "{}", r.err);
    }

    // R-267
    #[test]
    fn r267_peers_lists_did_roles_each_way_expiry_path_and_round_trip() {
        let exp = NOW as u64 / 1000 + 364 * 86_400;
        let client = Arc::new(FakeClient::from_fn(move |_| {
            Ok(Response::Peers {
                peers: vec![
                    peer(
                        "nixos",
                        &["submit", "observe"],
                        &["observe"],
                        Some(exp),
                        false,
                    ),
                    peer("old", &[], &[], None, true),
                ],
            })
        }));
        let h = Harness::default();
        let r = h.run(&client, &["peers"]);
        assert_eq!(r.code, 0, "{}", r.err);
        let lines: Vec<&str> = r.out.lines().collect();
        assert!(
            lines[0].starts_with("NAME") && lines[0].contains("WE MAY") && lines[0].contains("RTT")
        );
        assert!(lines[1].starts_with("nixos"), "{}", r.out);
        assert!(lines[1].contains("did:key:z6MknncV…TfcYvs"), "{}", r.out);
        assert!(
            lines[1].contains("submit,observe") && lines[1].contains("in 364 d"),
            "{}",
            r.out
        );
        assert!(
            lines[1].contains("direct") && lines[1].contains("3 ms"),
            "{}",
            r.out
        );
        assert!(
            lines[2].contains("revoked") && lines[2].contains('-'),
            "{}",
            r.out
        );
        // JSON carries the full DID
        let r = h.run(&client, &["peers", "--json"]);
        let v: serde_json::Value = serde_json::from_str(r.out.trim()).unwrap();
        assert_eq!(
            v[0]["did"],
            "did:key:z6MknncVmedphgDSbQoYLyNaFRFdB2WtyjxcFdKV6zTfcYvs"
        );
        assert_eq!(v[0]["roles_out"][0]["role"], "submit");
    }

    // R-267
    #[test]
    fn r267_no_peers_says_how_to_pair_and_revoke_reports_the_closed_connections() {
        let client = Arc::new(FakeClient::from_fn(|req| match req {
            Request::Peers => Ok(Response::Peers { peers: vec![] }),
            Request::PeerRevoke { name } => Ok(Response::Revoked(RevokeView {
                name: name.clone(),
                revoked: 2,
                closed: 1,
            })),
            other => panic!("{other:?}"),
        }));
        let h = Harness::default();
        let r = h.run(&client, &["peers"]);
        assert!(r.out.contains("tasks2 pair invite") && r.out.contains("tasks2 pair accept"));
        let r = h.run(&client, &["peers", "revoke", "nixos"]);
        assert_eq!(
            r.out,
            "revoked 2 grant(s) of nixos; 1 live connection(s) closed\n"
        );
    }

    #[test]
    fn r267_dids_and_durations_read_short() {
        assert_eq!(short_did("did:key:zShort"), "did:key:zShort");
        let long = "did:key:z6MknncVmedphgDSbQoYLyNaFRFdB2WtyjxcFdKV6zTfcYvs";
        let s = short_did(long);
        assert_eq!(s, "did:key:z6MknncV…TfcYvs");
        assert_eq!(human_secs(40), "40 s");
        assert_eq!(human_secs(900), "15 m");
        assert_eq!(human_secs(7200), "2 h");
        assert_eq!(human_secs(3 * 86_400), "3 d");
    }

    fn state_with_identity() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let ks = Keystore::open_or_create(d.path()).unwrap();
        let did = tasks_identity::Signer::did(ks.identity()).to_string();
        (d, did)
    }

    fn id_args(d: &Path, extra: &[&str]) -> (IdentityCommand, Option<std::path::PathBuf>) {
        let mnemonic = extra.contains(&"--mnemonic");
        let json = extra.contains(&"--json");
        (
            IdentityCommand::Show { mnemonic, json },
            Some(d.to_path_buf()),
        )
    }

    fn env(k: &str) -> Option<String> {
        (k == "HOME").then(|| "/nonexistent".to_string())
    }

    // R-268
    #[test]
    fn r268_identity_show_prints_did_name_and_node_id_without_a_daemon() {
        let (d, did) = state_with_identity();
        let (cmd, sd) = id_args(d.path(), &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            identity(&cmd, sd.as_deref(), &env, false, &mut out, &mut err),
            0
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(&did), "{out}");
        assert!(out.contains("name     "), "{out}");
        let node = tasks_identity::read_public(d.path()).unwrap().node_id;
        assert!(out.contains(&format!("node id  {node}")), "{out}");
        assert!(!out.contains("backup"), "no phrase unless asked");
        // JSON
        let (cmd, sd) = id_args(d.path(), &["--json"]);
        let mut out = Vec::new();
        identity(&cmd, sd.as_deref(), &env, false, &mut out, &mut err);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["did"], did.as_str());
        assert_eq!(v["node_id"], node.as_str());
    }

    // R-268
    #[test]
    fn r268_the_backup_phrase_goes_only_to_a_terminal_with_a_warning() {
        let (d, _) = state_with_identity();
        let phrase = Keystore::load(&Keystore::path_in(d.path()))
            .unwrap()
            .mnemonic()
            .unwrap()
            .to_string();
        // not a terminal: refused, nothing printed on stdout
        let (cmd, sd) = id_args(d.path(), &["--mnemonic"]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = identity(&cmd, sd.as_deref(), &env, false, &mut out, &mut err);
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err.clone())
                .unwrap()
                .contains("only to a terminal")
        );
        assert!(!String::from_utf8(err).unwrap().contains(&phrase));
        // a terminal: printed, with the warning on stderr
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = identity(&cmd, sd.as_deref(), &env, true, &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).unwrap().contains(&phrase));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("WARNING") && !err.contains(&phrase));
        // never as JSON, even on a terminal
        let (cmd, sd) = id_args(d.path(), &["--mnemonic", "--json"]);
        let mut out = Vec::new();
        let code = identity(&cmd, sd.as_deref(), &env, true, &mut out, &mut Vec::new());
        assert_eq!(code, 2);
        assert!(out.is_empty());
        // and the flag is a refusal through the whole command line too (the test's stdout is
        // not a terminal)
        let h = Harness::default();
        let client = Arc::new(FakeClient::from_fn(|_| unreachable_reply()));
        let state = d.path().display().to_string();
        let r = h.run(
            &client,
            &["--state-dir", &state, "identity", "show", "--mnemonic"],
        );
        assert_eq!(r.code, 2);
        assert!(!r.out.contains(&phrase) && !r.err.contains(&phrase));
        assert!(client.requests().is_empty(), "identity needs no daemon");
    }

    fn unreachable_reply() -> Result<Response, crate::client::ClientError> {
        Err(crate::client::ClientError::BadReply(
            "no daemon expected".into(),
        ))
    }

    // R-268
    #[test]
    fn r268_a_machine_without_an_identity_is_told_when_it_appears() {
        let d = tempfile::tempdir().unwrap();
        let (cmd, sd) = id_args(d.path(), &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = identity(&cmd, sd.as_deref(), &env, false, &mut out, &mut err);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("created when the daemon first starts")
        );
    }
}
