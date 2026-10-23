use super::*;
use clap::CommandFactory;

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let mut v = vec!["tasks2"];
    v.extend_from_slice(args);
    Cli::try_parse_from(v)
}

fn run_args(args: &[&str]) -> RunArgs {
    match parse(args).unwrap().command {
        Command::Run(a) | Command::Enqueue(a) => a,
        other => panic!("expected run, got {other:?}"),
    }
}

#[test]
fn the_cli_definition_is_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn run_takes_options_then_the_command_after_a_double_dash() {
    let a = run_args(&[
        "run",
        "--who",
        "me",
        "--pin",
        "--broad-test",
        "--detach",
        "--timeout",
        "60",
        "--",
        "cargo",
        "test",
        "-p",
        "x",
    ]);
    assert_eq!(a.who.as_deref(), Some("me"));
    assert!(a.place.pin && a.broad_test && a.detach);
    assert_eq!(a.timeout, Some(60));
    assert_eq!(a.command, ["cargo", "test", "-p", "x"]);
    assert_eq!(a.place.placement(None).unwrap(), Placement::Pin);
}

#[test]
fn enqueue_and_submit_are_the_same_command() {
    assert_eq!(run_args(&["enqueue", "--", "true"]).command, ["true"]);
    assert_eq!(run_args(&["submit", "--", "true"]).command, ["true"]);
}

// R-190
#[test]
fn r190_a_missing_command_is_a_usage_error() {
    let e = parse(&["run", "--who", "me"]).unwrap_err();
    assert_eq!(e.exit_code(), 2);
    assert!(e.to_string().contains("<CMD>"), "{e}");
}

#[test]
fn r190_unknown_options_and_commands_are_specific_usage_errors() {
    let e = parse(&["run", "--bogus", "--", "x"]).unwrap_err();
    assert!(e.to_string().contains("--bogus"), "{e}");
    let e = parse(&["frobnicate"]).unwrap_err();
    assert!(e.to_string().contains("frobnicate"));
}

#[test]
fn r190_conflicting_placements_name_both_options_and_the_fix() {
    let p = PlaceArgs {
        pin: true,
        anywhere: true,
        ..PlaceArgs::default()
    };
    let CliError::Usage(m) = p.placement(None).unwrap_err() else {
        panic!("usage expected")
    };
    assert!(m.contains("--pin and --anywhere conflict"), "{m}");
    assert!(
        m.contains("keeps the job on this machine") && m.contains("choose any capable machine"),
        "{m}"
    );
    assert!(m.ends_with("pass only one of them"));
    let p = PlaceArgs {
        peer: Some("nixos".into()),
        here: true,
        ..PlaceArgs::default()
    };
    let CliError::Usage(m) = p.placement(None).unwrap_err() else {
        panic!("usage expected")
    };
    assert!(
        m.contains("--peer and --here conflict") && m.contains("sends it to nixos"),
        "{m}"
    );
}

#[test]
fn r190_a_device_with_a_remote_placement_names_the_conflict_and_the_fix() {
    let p = PlaceArgs {
        peer: Some("nixos".into()),
        ..PlaceArgs::default()
    };
    let CliError::Usage(m) = p.placement(Some("9BWAY1LSDC")).unwrap_err() else {
        panic!("usage expected")
    };
    assert!(
        m.contains("--device 9BWAY1LSDC and --peer NAME conflict"),
        "{m}"
    );
    assert!(
        m.contains("drop --peer NAME to run here, or drop --device"),
        "{m}"
    );
    let p = PlaceArgs {
        anywhere: true,
        ..PlaceArgs::default()
    };
    let CliError::Usage(m) = p.placement(Some("S")).unwrap_err() else {
        panic!("usage expected")
    };
    assert!(m.contains("--device S and --anywhere conflict"), "{m}");
    // a device is fine with --pin, --here or nothing
    assert_eq!(
        PlaceArgs {
            pin: true,
            ..PlaceArgs::default()
        }
        .placement(Some("S"))
        .unwrap(),
        Placement::Pin
    );
    assert_eq!(
        PlaceArgs::default().placement(Some("S")).unwrap(),
        Placement::Here
    );
}

#[test]
fn placements_map_to_the_core_type() {
    let peer = PlaceArgs {
        peer: Some("nixos".into()),
        ..PlaceArgs::default()
    };
    assert_eq!(
        peer.placement(None).unwrap(),
        Placement::Peer(MachineName("nixos".into()))
    );
    assert_eq!(
        PlaceArgs {
            anywhere: true,
            ..PlaceArgs::default()
        }
        .placement(None)
        .unwrap(),
        Placement::Anywhere
    );
    assert_eq!(
        PlaceArgs {
            here: true,
            ..PlaceArgs::default()
        }
        .placement(None)
        .unwrap(),
        Placement::Here
    );
    assert!(
        PlaceArgs {
            peer: Some(" ".into()),
            ..PlaceArgs::default()
        }
        .placement(None)
        .is_err()
    );
}

#[test]
fn logs_options_conflict_precisely() {
    assert!(parse(&["logs", "5", "--tail", "10", "--grep", "x"]).is_err());
    assert!(parse(&["logs", "5", "--follow", "--grep", "x"]).is_err());
    assert!(parse(&["logs", "5", "-C", "2"]).is_err());
    let Command::Logs(a) = parse(&["logs", "5", "--grep", "FAILED", "-C", "2", "-F"])
        .unwrap()
        .command
    else {
        panic!()
    };
    assert_eq!((a.id, a.context, a.fixed), (5, 2, true));
}

#[test]
fn list_log_search_and_control_commands_parse() {
    let Command::List(a) = parse(&[
        "list", "--who", "x", "--active", "--limit", "5", "--brief", "--json",
    ])
    .unwrap()
    .command
    else {
        panic!()
    };
    assert!(a.active && a.brief && a.json && a.limit == Some(5));
    let Command::LogSearch(a) = parse(&[
        "log-search",
        "boom",
        "--id",
        "1",
        "--id",
        "2",
        "--since",
        "2h",
        "-C",
        "1",
    ])
    .unwrap()
    .command
    else {
        panic!()
    };
    assert_eq!(
        (a.ids.clone(), a.since.as_deref(), a.context),
        (vec![1, 2], Some("2h"), 1)
    );
    assert!(matches!(parse(&["drain"]).unwrap().command, Command::Drain));
    assert!(matches!(
        parse(&["resume"]).unwrap().command,
        Command::Resume
    ));
    assert!(matches!(
        parse(&["execution", "stop"]).unwrap().command,
        Command::Execution { .. }
    ));
    let Command::Cancel { id, who, reason } =
        parse(&["cancel", "9", "--who", "me", "--reason", "stale"])
            .unwrap()
            .command
    else {
        panic!()
    };
    assert_eq!(
        (id, who.as_deref(), reason.as_deref()),
        (9, Some("me"), Some("stale"))
    );
    assert!(matches!(
        parse(&["suite", "run", "core", "--quick"]).unwrap().command,
        Command::Suite { .. }
    ));
    assert!(matches!(
        parse(&["policy", "suggest"]).unwrap().command,
        Command::Policy { .. }
    ));
    // R-407: `tasks2 cache` takes no arguments
    assert!(matches!(parse(&["cache"]).unwrap().command, Command::Cache));
    assert!(parse(&["cache", "--bogus"]).is_err());
    assert!(matches!(
        parse(&["--state-dir", "/s", "status"]).unwrap().command,
        Command::Status { .. }
    ));
    assert_eq!(
        parse(&["status", "--state-dir", "/s"]).unwrap().state_dir,
        Some("/s".into())
    );
}

#[test]
fn daemon_arguments_pass_through_untouched() {
    let Command::Daemon { args } =
        parse(&["daemon", "serve", "--state-dir", "/s", "--machine", "m"])
            .unwrap()
            .command
    else {
        panic!()
    };
    assert_eq!(args, ["serve", "--state-dir", "/s", "--machine", "m"]);
    let Command::Daemon { args } = parse(&["daemon", "--help"]).unwrap().command else {
        panic!()
    };
    assert_eq!(args, ["--help"]);
}

#[test]
fn help_documents_the_exit_codes() {
    let e = parse(&["--help"]).unwrap_err();
    let text = e.to_string();
    for want in [
        "EXIT CODES",
        "128+N",
        "141",
        "the job's own exit code",
        "no such job",
    ] {
        assert!(text.contains(want), "{want} in help");
    }
    assert_eq!(e.exit_code(), 0);
    assert_eq!(parse(&[]).unwrap_err().exit_code(), 2);
}

// R-283
#[test]
fn r283_audit_takes_filters_or_one_subcommand_but_not_both() {
    let a = match parse(&[
        "audit", "--since", "2h", "--peer", "nixos", "--job", "4", "--action", "grant", "--limit",
        "9", "--json",
    ])
    .unwrap()
    .command
    {
        Command::Audit(a) => a,
        other => panic!("{other:?}"),
    };
    assert!(a.sub.is_none() && a.json);
    assert_eq!(
        (
            a.since.as_deref(),
            a.peer.as_deref(),
            a.job,
            a.action_filter.as_deref(),
            a.limit
        ),
        (Some("2h"), Some("nixos"), Some(4), Some("grant"), Some(9))
    );
    assert!(matches!(
        parse(&["audit", "verify", "--json"]).unwrap().command,
        Command::Audit(AuditArgs {
            sub: Some(AuditCommand::Verify { json: true }),
            ..
        })
    ));
    assert!(matches!(
        parse(&["audit", "checkpoint"]).unwrap().command,
        Command::Audit(AuditArgs {
            sub: Some(AuditCommand::Checkpoint),
            ..
        })
    ));
    assert!(parse(&["audit", "verify", "--job", "3"]).is_err());
    assert!(parse(&["audit", "--job", "x"]).is_err());
}

// R-336
#[test]
fn r336_stalls_takes_since_until_limit_and_json() {
    match parse(&[
        "stalls", "--since", "2h", "--until", "1h", "--limit", "5", "--json",
    ])
    .unwrap()
    .command
    {
        Command::Stalls(a) => {
            assert_eq!(a.since.as_deref(), Some("2h"));
            assert_eq!(a.until.as_deref(), Some("1h"));
            assert_eq!(a.limit, Some(5));
            assert!(a.json);
        }
        other => panic!("{other:?}"),
    }
}
