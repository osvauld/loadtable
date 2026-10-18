//! Client-side argument parsing for `run`/`enqueue`, `list` and `phone-test`.
//! Errors name the exact problem and the correct usage instead of a generic refusal.
use tinline_task_manager::Result;

pub const RUN_USAGE: &str = "tasks run|enqueue [--who OWNER] [--run | --offload | --device SERIAL] [--peer NAME [--fetch REL]... | --anywhere] [--follow] [--broad-test] -- COMMAND [ARGS...]";
pub const PLACE_USAGE: &str =
    "tasks place [--run | --device SERIAL] [--peer NAME] [--json] -- COMMAND [ARGS...]";
const LIST_USAGE: &str =
    "tasks list [--who OWNER] [--active] [--limit N] [--json | --table | --brief]";
const PHONE_USAGE: &str =
    "tasks phone-test [--who OWNER] SOURCE [--device SERIAL] [--follow | --full-log | --tail N]";

/// The admission class requested on the command line; `None` means compile.
#[derive(Debug, PartialEq)]
pub enum Class {
    Run,
    Offload,
    Device(String),
}

#[derive(Debug, Default, PartialEq)]
pub struct Enqueue {
    pub owner: Option<String>,
    pub class: Option<Class>,
    pub follow: bool,
    /// Forward the command to this peer daemon (`tasks peer list`).
    pub peer: Option<String>,
    /// Artifacts to fetch back from the peer's checkout after a passed job.
    pub fetch: Vec<String>,
    /// Let placement choose this machine or a worker peer (`tasks place` shows the choice).
    pub anywhere: bool,
    /// Deliberately build every test target of a filtered test command (the policy's
    /// `broad_test = "refuse"` escape); recorded on the job.
    pub broad_test: bool,
}

/// Parses the options before `--`. `--follow` only makes sense when this client waits.
pub fn enqueue(options: &[String], waits: bool) -> Result<Enqueue> {
    let mut parsed = Enqueue::default();
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        match option.as_str() {
            "--who" => {
                let owner = rest
                    .next()
                    .ok_or(format!("--who needs an OWNER value; usage: {RUN_USAGE}"))?;
                parsed.owner = Some(owner.clone());
            }
            "--run" | "--offload" | "--device" => {
                let class = if option == "--run" {
                    Class::Run
                } else if option == "--offload" {
                    Class::Offload
                } else {
                    Class::Device(
                        rest.next()
                            .ok_or(format!("--device needs a SERIAL value; usage: {RUN_USAGE}"))?
                            .clone(),
                    )
                };
                match (&parsed.class, &class) {
                    (None, _) => parsed.class = Some(class),
                    (Some(Class::Run), Class::Run) => {
                        return Err("--run was given twice".into());
                    }
                    (Some(Class::Device(_)), Class::Device(_)) => {
                        return Err("--device was given twice; one job locks one device".into());
                    }
                    (Some(Class::Offload), Class::Offload) => {
                        return Err("--offload was given twice".into());
                    }
                    (Some(Class::Offload), _) | (_, Class::Offload) => {
                        return Err(
                            "--offload conflicts with --run/--device: --offload selects its own \
                             admission class (offload slot, no CPU PSI gate) for a tasks-offload \
                             or offload-portable-desktop.sh client only. Use: tasks run --who \
                             OWNER --offload -- tasks-offload ... -- COMMAND"
                                .into(),
                        );
                    }
                    _ => {
                        return Err(
                            "--run and --device conflict: --device SERIAL already selects \
                             its own admission class (exclusive device lock, no compile slot), so \
                             drop --run. Use: tasks run --who OWNER --device SERIAL -- COMMAND; \
                             --run alone is for already-built commands that need no device"
                                .into(),
                        );
                    }
                }
            }
            "--peer" => {
                let name = rest
                    .next()
                    .ok_or(format!("--peer needs a NAME value; usage: {RUN_USAGE}"))?;
                if parsed.peer.replace(name.clone()).is_some() {
                    return Err("--peer was given twice; one job runs on one peer".into());
                }
            }
            "--anywhere" => {
                if parsed.anywhere {
                    return Err("--anywhere was given twice".into());
                }
                parsed.anywhere = true;
            }
            "--fetch" => {
                let rel = rest
                    .next()
                    .ok_or(format!("--fetch needs a relative PATH; usage: {RUN_USAGE}"))?;
                parsed.fetch.push(rel.clone());
            }
            "--broad-test" => parsed.broad_test = true,
            "--follow" if waits => parsed.follow = true,
            "--follow" => {
                return Err(
                    "--follow streams the log while this client waits; use it with tasks run \
                     (enqueue returns immediately)"
                        .into(),
                );
            }
            other => {
                return Err(
                    format!("unknown option {other:?} before --; usage: {RUN_USAGE}").into(),
                );
            }
        }
    }
    if parsed.peer.is_some() && parsed.class == Some(Class::Offload) {
        return Err(
            "--peer and --offload conflict: --peer already uses an offload slot for the local \
             bridge; the command runs in the peer's compile slot (or --run / --device there)"
                .into(),
        );
    }
    if parsed.anywhere && parsed.peer.is_some() {
        return Err(
            "--anywhere and --peer conflict: --peer NAME forwards to that peer; --anywhere lets \
             placement choose (tasks place -- COMMAND shows its choice)"
                .into(),
        );
    }
    if parsed.anywhere && parsed.class == Some(Class::Offload) {
        return Err(
            "--anywhere and --offload conflict: placement forwards through the peer link itself"
                .into(),
        );
    }
    if !parsed.fetch.is_empty() && parsed.peer.is_none() && !parsed.anywhere {
        return Err("--fetch only applies with --peer NAME (or --anywhere)".into());
    }
    Ok(parsed)
}

/// `tasks place` options: a dry run of placement for one command.
#[derive(Debug, Default, PartialEq)]
pub struct Place {
    pub class: Option<Class>,
    pub peer: Option<String>,
    pub json: bool,
}

pub fn place(options: &[String]) -> Result<Place> {
    let mut parsed = Place::default();
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        match option.as_str() {
            "--run" if parsed.class.is_none() => parsed.class = Some(Class::Run),
            "--device" if parsed.class.is_none() => {
                parsed.class = Some(Class::Device(
                    rest.next()
                        .ok_or(format!(
                            "--device needs a SERIAL value; usage: {PLACE_USAGE}"
                        ))?
                        .clone(),
                ))
            }
            "--peer" if parsed.peer.is_none() => {
                parsed.peer = Some(
                    rest.next()
                        .ok_or(format!("--peer needs a NAME value; usage: {PLACE_USAGE}"))?
                        .clone(),
                )
            }
            "--json" => parsed.json = true,
            other => {
                return Err(format!(
                    "unknown or repeated option {other:?} before --; usage: {PLACE_USAGE}"
                )
                .into());
            }
        }
    }
    Ok(parsed)
}

#[derive(Debug, Default, PartialEq)]
pub struct List {
    pub who: Option<String>,
    pub active: bool,
    pub limit: Option<usize>,
    pub json: bool,
}

/// Plain `tasks list` (no options) keeps the full JSON response for scripts; it is
/// handled before this parser. With any option the default is the compact table and
/// `--json` returns the same response shape with only the selected jobs.
pub fn list(options: &[String]) -> Result<List> {
    let mut parsed = List::default();
    let mut table = false;
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        match option.as_str() {
            "--who" => {
                parsed.who = Some(
                    rest.next()
                        .ok_or(format!("--who needs an OWNER value; usage: {LIST_USAGE}"))?
                        .clone(),
                );
            }
            "--active" => parsed.active = true,
            "--limit" => {
                let value = rest
                    .next()
                    .ok_or(format!("--limit needs a number; usage: {LIST_USAGE}"))?;
                parsed.limit = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or(format!("--limit needs a positive number, not {value:?}"))?,
                );
            }
            "--json" => parsed.json = true,
            "--table" | "--brief" => table = true,
            other => {
                return Err(format!("unknown list option {other:?}; usage: {LIST_USAGE}").into());
            }
        }
    }
    if table && parsed.json {
        return Err("--json and --table/--brief are alternatives; pick one".into());
    }
    Ok(parsed)
}

/// How a waiting client shows a job's log.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Output {
    /// Phase changes while waiting, then the retained log tail (the `tasks run` default).
    Full,
    /// Stream new log text while the job runs; nothing is reprinted at the end.
    Follow,
    /// Progress lines (Android steps) while waiting, then at most this many log lines.
    Progress { tail: usize },
}

pub const PHONE_TAIL_LINES: usize = 40;

#[derive(Debug, PartialEq)]
pub struct PhoneTest {
    pub owner: String,
    pub source: String,
    pub serial: Option<String>,
    pub output: Output,
}

pub fn phone_test(args: &[String], default_owner: String) -> Result<PhoneTest> {
    let mut owner = default_owner;
    let (mut source, mut serial, mut output) = (None, None, None);
    let mut rest = args.iter();
    let set_output = |output: &mut Option<Output>, value: Output| {
        if output.replace(value).is_some() {
            return Err(format!(
                "--follow, --full-log and --tail are alternatives; usage: {PHONE_USAGE}"
            ));
        }
        Ok(())
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--who" => owner = rest.next().ok_or(PHONE_USAGE)?.clone(),
            "--device" if serial.is_none() => {
                serial = Some(rest.next().ok_or(PHONE_USAGE)?.clone())
            }
            "--follow" => set_output(&mut output, Output::Follow)?,
            "--full-log" => set_output(&mut output, Output::Full)?,
            "--tail" => {
                let value = rest.next().ok_or(PHONE_USAGE)?;
                let tail = value
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(format!("--tail needs a positive line count, not {value:?}"))?;
                set_output(&mut output, Output::Progress { tail })?;
            }
            s if source.is_none() && !s.starts_with('-') => source = Some(s.to_owned()),
            _ => return Err(format!("usage: {PHONE_USAGE}").into()),
        }
    }
    Ok(PhoneTest {
        owner,
        source: source.ok_or(format!("SOURCE is required; usage: {PHONE_USAGE}"))?,
        serial,
        output: output.unwrap_or(Output::Progress {
            tail: PHONE_TAIL_LINES,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn run_with_device_names_the_conflict_and_the_fix() {
        for order in [
            "--device S --run",
            "--run --device S",
            "--who x --device S --run",
        ] {
            let error = enqueue(&args(order), true).unwrap_err().to_string();
            assert!(error.contains("--run and --device conflict"), "{error}");
            assert!(error.contains("drop --run"), "{error}");
            assert!(error.contains("--device SERIAL -- COMMAND"), "{error}");
        }
    }

    #[test]
    fn enqueue_options_parse_and_reject_precisely() {
        assert_eq!(
            enqueue(&args("--who a --device S --follow"), true).unwrap(),
            Enqueue {
                owner: Some("a".into()),
                class: Some(Class::Device("S".into())),
                follow: true,
                ..Enqueue::default()
            }
        );
        assert_eq!(
            enqueue(&args("--run"), false).unwrap().class,
            Some(Class::Run)
        );
        assert_eq!(enqueue(&[], true).unwrap(), Enqueue::default());
        let cases = [
            ("--run --run", "--run was given twice"),
            ("--device a --device b", "--device was given twice"),
            ("--who", "--who needs an OWNER"),
            ("--device", "--device needs a SERIAL"),
            ("--bogus", "unknown option \"--bogus\""),
        ];
        for (input, expected) in cases {
            let error = enqueue(&args(input), true).unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
        let error = enqueue(&args("--follow"), false).unwrap_err().to_string();
        assert!(error.contains("use it with tasks run"), "{error}");
    }

    #[test]
    fn peer_and_fetch_options_parse_and_conflict() {
        let parsed = enqueue(
            &args("--who a --peer worker --run --fetch out/x --fetch y"),
            true,
        )
        .unwrap();
        assert_eq!(parsed.peer.as_deref(), Some("worker"));
        assert_eq!(parsed.class, Some(Class::Run));
        assert_eq!(parsed.fetch, ["out/x", "y"]);
        for (input, expected) in [
            ("--peer a --peer b", "--peer was given twice"),
            ("--peer", "--peer needs a NAME"),
            ("--peer a --offload", "--peer and --offload conflict"),
            ("--fetch x", "--fetch only applies with --peer"),
        ] {
            let error = enqueue(&args(input), true).unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
    }

    #[test]
    fn anywhere_and_place_options_parse_and_conflict() {
        let parsed = enqueue(&args("--who a --anywhere --fetch out"), true).unwrap();
        assert!(parsed.anywhere);
        assert_eq!(parsed.fetch, ["out"]);
        assert!(enqueue(&args("--anywhere --run"), true).unwrap().anywhere);
        for (input, expected) in [
            ("--anywhere --anywhere", "--anywhere was given twice"),
            ("--anywhere --peer w", "--anywhere and --peer conflict"),
            ("--anywhere --offload", "--anywhere and --offload conflict"),
        ] {
            let error = enqueue(&args(input), true).unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
        assert_eq!(
            place(&args("--device S --peer w --json")).unwrap(),
            Place {
                class: Some(Class::Device("S".into())),
                peer: Some("w".into()),
                json: true,
            }
        );
        assert_eq!(place(&[]).unwrap(), Place::default());
        for bad in ["--run --run", "--peer", "--offload", "--who a"] {
            assert!(place(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn offload_class_parses_and_conflicts_like_run() {
        assert_eq!(
            enqueue(&args("--who a --offload --follow"), true).unwrap(),
            Enqueue {
                owner: Some("a".into()),
                class: Some(Class::Offload),
                follow: true,
                ..Enqueue::default()
            }
        );
        assert_eq!(
            enqueue(&args("--offload"), false).unwrap().class,
            Some(Class::Offload)
        );
        let error = enqueue(&args("--offload --offload"), true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--offload was given twice"), "{error}");
        for order in [
            "--offload --run",
            "--run --offload",
            "--offload --device S",
            "--device S --offload",
        ] {
            let error = enqueue(&args(order), true).unwrap_err().to_string();
            assert!(error.contains("conflict"), "{order}: {error}");
            assert!(error.contains("--offload"), "{order}: {error}");
            assert!(error.contains("tasks-offload"), "{order}: {error}");
        }
        assert!(RUN_USAGE.contains("--offload"), "{RUN_USAGE}");
    }

    #[test]
    fn list_options() {
        assert_eq!(
            list(&args("--who a --active --limit 5 --json")).unwrap(),
            List {
                who: Some("a".into()),
                active: true,
                limit: Some(5),
                json: true,
            }
        );
        assert!(!list(&args("--brief")).unwrap().json);
        assert!(!list(&args("--table --active")).unwrap().json);
        for bad in [
            "--limit 0",
            "--limit x",
            "--limit",
            "--who",
            "--json --table",
            "--x",
        ] {
            assert!(list(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn phone_test_output_modes() {
        let parse = |s: &str| phone_test(&args(s), "me".into());
        let default = parse("/src").unwrap();
        assert_eq!(default.owner, "me");
        assert_eq!(
            default.output,
            Output::Progress {
                tail: PHONE_TAIL_LINES
            }
        );
        assert_eq!(parse("/src --follow").unwrap().output, Output::Follow);
        assert_eq!(parse("--full-log /src").unwrap().output, Output::Full);
        let tail = parse("--who w /src --device S --tail 7").unwrap();
        assert_eq!(
            (tail.owner.as_str(), tail.serial.as_deref(), tail.output),
            ("w", Some("S"), Output::Progress { tail: 7 })
        );
        for bad in [
            "/src --follow --full-log",
            "/src --tail 0",
            "--follow",
            "/a /b",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
