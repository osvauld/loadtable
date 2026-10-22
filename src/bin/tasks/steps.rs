//! `tasks steps PLAN.json`: run a plan of named steps with `after` dependencies. Every step is an
//! ordinary `tasks run` job (this binary again), so admission, placement, ETA, logs and cancel
//! behave as for any job; this driver only decides which steps may start and reports per step.
//! The pure part (plan, scheduler, summary) is `tinline_task_manager::steps`.
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;
use tinline_task_manager::Result;
use tinline_task_manager::steps::{OnFailure, Plan, Report, Scheduler, job_id_from_line};

pub const USAGE: &str = "usage: tasks steps [--who OWNER] [--anywhere | --peer NAME | --pin] [--keep-going] [--verbose] PLAN.json|-
  PLAN is JSON: {\"steps\":[{\"name\":\"build\",\"argv\":[\"cargo\",\"test\",\"--no-run\"]},
                          {\"name\":\"peer\",\"argv\":[\"cargo\",\"test\",\"--test\",\"peer\"],\"after\":[\"build\"]}]}
  A step may also set \"run\": true (run slot) and \"broad_test\": true. Each step is one `tasks run`
  job; steps with no unfinished `after` start together. The first failure stops new steps from
  starting (running ones finish); --keep-going skips only the steps that depend on a failure.
  Exit status 0 when every step passed, 1 otherwise. - reads the plan from standard input.
  Each job's submit/admission boilerplate is hidden; --verbose shows every line.";

#[derive(Debug, Default, PartialEq)]
struct Options {
    owner: Option<String>,
    /// `--anywhere`, `--pin` or `--peer NAME`, passed to every step unchanged.
    place: Vec<String>,
    keep_going: bool,
    verbose: bool,
    plan: Option<String>,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut out = Options::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--who" => out.owner = Some(rest.next().ok_or(USAGE)?.clone()),
            "--anywhere" | "--pin" => {
                if !out.place.is_empty() {
                    return Err("one of --anywhere, --peer NAME or --pin".into());
                }
                out.place.push(arg.clone());
            }
            "--peer" => {
                if !out.place.is_empty() {
                    return Err("one of --anywhere, --peer NAME or --pin".into());
                }
                out.place = vec![arg.clone(), rest.next().ok_or(USAGE)?.clone()];
            }
            "--keep-going" => out.keep_going = true,
            "--verbose" => out.verbose = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?}; {USAGE}").into());
            }
            path => {
                if out.plan.replace(path.to_string()).is_some() {
                    return Err(format!("one plan only; {USAGE}").into());
                }
            }
        }
    }
    if out.plan.is_none() {
        return Err(USAGE.into());
    }
    Ok(out)
}

/// Arguments of the `tasks run` that executes one step.
fn step_args(opts: &Options, owner: &str, step: &tinline_task_manager::steps::Step) -> Vec<String> {
    let mut args = vec!["run".to_string(), "--who".into(), owner.into()];
    if step.run {
        args.push("--run".into());
    }
    if step.broad_test {
        args.push("--broad-test".into());
    }
    args.extend(opts.place.iter().cloned());
    args.push("--".into());
    args.extend(step.argv.iter().cloned());
    args
}

/// Lines every `tasks run` prints around the command's own output (submit notice, admission and
/// trust banners); hidden unless `--verbose`. Job ids are still read from them.
fn is_boilerplate(line: &str) -> bool {
    const PREFIXES: [&str; 6] = [
        "tasks: submitted job ",
        "tasks: if this client stops",
        "tasks: open-file limit",
        "TRUSTED LIVE COMMAND",
        "Native Rust admission granted",
        "live cwd=",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

enum Event {
    Line(usize, String),
    Done(usize, bool, f64),
}

fn pump(
    i: usize,
    source: impl Read + Send + 'static,
    tx: mpsc::Sender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(source).lines().map_while(|l| l.ok()) {
            let _ = tx.send(Event::Line(i, line));
        }
    })
}

fn spawn_step(i: usize, args: Vec<String>, tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let ok = match std::env::current_exe().and_then(|exe| {
            Command::new(exe)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }) {
            Ok(mut child) => {
                let readers: Vec<_> = [
                    child.stdout.take().map(|s| pump(i, s, tx.clone())),
                    child.stderr.take().map(|s| pump(i, s, tx.clone())),
                ]
                .into_iter()
                .flatten()
                .collect();
                let status = child.wait();
                for r in readers {
                    let _ = r.join();
                }
                status.is_ok_and(|s| s.success())
            }
            Err(e) => {
                let _ = tx.send(Event::Line(
                    i,
                    format!("tasks steps: cannot start the job: {e}"),
                ));
                false
            }
        };
        let _ = tx.send(Event::Done(i, ok, started.elapsed().as_secs_f64()));
    });
}

pub fn run(args: &[String]) -> Result<()> {
    let opts = parse(args)?;
    let source = opts.plan.as_deref().unwrap_or("-");
    let text = if source == "-" {
        let mut text = String::new();
        std::io::stdin().take(1 << 20).read_to_string(&mut text)?;
        text
    } else {
        std::fs::read_to_string(source).map_err(|e| format!("{source}: {e}"))?
    };
    let plan = Plan::parse(&text)?;
    for note in plan.notes() {
        eprintln!("tasks steps: note: {note}");
    }
    let owner = opts
        .owner
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "local-agent".into()));
    if owner.trim().is_empty() || owner.len() > 128 {
        return Err("bounded owner required".into());
    }
    let mut sched = Scheduler::new(
        plan,
        if opts.keep_going {
            OnFailure::Continue
        } else {
            OnFailure::Stop
        },
    );
    let (tx, rx) = mpsc::channel();
    let mut jobs: Vec<Option<i64>> = vec![None; sched.plan().steps.len()];
    let launch = |sched: &mut Scheduler| {
        for i in sched.start_ready() {
            let step = &sched.plan().steps[i];
            eprintln!("tasks steps: starting {}", step.name);
            spawn_step(i, step_args(&opts, &owner, step), tx.clone());
        }
    };
    launch(&mut sched);
    while !sched.done() {
        let Ok(event) = rx.recv() else { break };
        match event {
            Event::Line(i, line) => {
                if let Some(id) = job_id_from_line(&line) {
                    jobs[i] = Some(id);
                }
                if opts.verbose || !is_boilerplate(&line) {
                    println!("[{}] {line}", sched.plan().steps[i].name);
                }
            }
            Event::Done(i, ok, seconds) => {
                sched.finish(
                    i,
                    ok,
                    Report {
                        job: jobs[i],
                        seconds: Some(seconds),
                    },
                );
                let (finished, total) = sched.progress();
                eprintln!(
                    "tasks steps: {} {} ({finished}/{total} finished)",
                    sched.plan().steps[i].name,
                    if ok { "passed" } else { "FAILED" }
                );
                launch(&mut sched);
            }
        }
    }
    print!("{}", sched.summary());
    if !sched.passed() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn options_parse_and_reject_conflicts() {
        let o = parse(&args("--who me --anywhere --keep-going plan.json")).unwrap();
        assert_eq!(o.owner.as_deref(), Some("me"));
        assert_eq!(o.place, ["--anywhere"]);
        assert!(o.keep_going && o.plan.as_deref() == Some("plan.json"));
        assert_eq!(
            parse(&args("--peer nixos -")).unwrap().place,
            ["--peer", "nixos"]
        );
        assert!(parse(&args("--anywhere --pin p.json")).is_err());
        assert!(parse(&args("--peer nixos --anywhere p.json")).is_err());
        assert!(parse(&args("--bogus p.json")).is_err());
        assert!(parse(&args("a.json b.json")).is_err());
        assert!(parse(&args("--who me")).is_err());
    }

    #[test]
    fn run_boilerplate_is_hidden_but_real_output_and_results_are_not() {
        assert!(is_boilerplate(
            "tasks: submitted job 4213; cwd /x; argv echo"
        ));
        assert!(is_boilerplate(
            "TRUSTED LIVE COMMAND: cwd is NOT snapshotted"
        ));
        assert!(is_boilerplate(
            "Native Rust admission granted (shared flock protocol)"
        ));
        assert!(!is_boilerplate("first step"));
        assert!(!is_boilerplate("job 4213 passed (exit 0) owner=steps"));
        assert!(!is_boilerplate("error[E0308]: mismatched types"));
        assert!(parse(&args("--verbose p.json")).unwrap().verbose);
    }

    #[test]
    fn a_step_becomes_one_run_command_with_the_shared_placement() {
        let o = parse(&args("--anywhere p.json")).unwrap();
        let step = tinline_task_manager::steps::Step {
            name: "peer".into(),
            argv: vec![
                "cargo".into(),
                "test".into(),
                "--test".into(),
                "peer".into(),
            ],
            after: vec![],
            run: true,
            broad_test: false,
        };
        assert_eq!(
            step_args(&o, "me", &step),
            [
                "run",
                "--who",
                "me",
                "--run",
                "--anywhere",
                "--",
                "cargo",
                "test",
                "--test",
                "peer"
            ]
        );
    }
}
