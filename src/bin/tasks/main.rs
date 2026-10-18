//! One binary for everyone: agents use the subcommands, people open the window. Any
//! command that needs the queue starts the background daemon if it is not running.
//! Features: `ui` (the window and its modules) and `mcp` (`tasks mcp`) are default; a worker
//! build without them keeps the daemon, CLI, peer link and status.
mod alerts;
mod daemon;
mod display;
mod doctor;
mod follow;
#[cfg(feature = "ui")]
mod log_view;
#[cfg(feature = "mcp")]
mod mcp;
#[cfg(feature = "mcp")]
mod mcp_compute;
mod options;
mod status;
#[cfg(feature = "ui")]
mod ui;
#[cfg(feature = "ui")]
mod ui_legacy;
#[cfg(feature = "ui")]
mod ui_model;
#[cfg(feature = "ui")]
mod ui_source;

use std::fs;
use std::os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::{
    Action, JobSpec, Result, RuntimeSample, request_with_reconnect, state_dir,
};

const USAGE: &str = "usage: tasks [ui [--fixtures DIR]]  # DIR: cluster snapshot files; demo, contacts nothing
       tasks list  # full JSON response (unchanged, for scripts)
       tasks list [--who OWNER] [--active] [--limit N] [--json | --table | --brief]
         # with any option: compact table by default (--active adds live lock holders); --json = filtered JSON
       tasks submit SPEC.json | approve JOB_ID | cancel JOB_ID
       tasks logs JOB_ID [--json]  # plain log text by default; --json preserves v2 response
       tasks run [--who OWNER] [--run | --offload | --device SERIAL] [--follow] -- COMMAND [ARGS...]
         # --run: already-built command, run slot; --device: exclusive device lock (never with --run)
         # --offload: tasks-offload/offload-portable-desktop.sh client only; own slots, no CPU PSI gate
         # --follow: stream the job log while it runs (default: phases, then the log at the end)
       tasks enqueue [--who OWNER] [--run | --offload | --device SERIAL] -- COMMAND [ARGS...]
       tasks run|enqueue --peer NAME [--run | --device SERIAL] [--fetch REL]... -- COMMAND [ARGS...]
         # forward to a peer daemon over iroh: sync this checkout, run there under ITS admission,
         # stream its log here, exit with its exit code (needs a peer config; tasks peer list)
       tasks run|enqueue --anywhere [--run | --device SERIAL] [--fetch REL]... -- COMMAND [ARGS...]
         # placement picks this machine or a worker peer (sooner finish; ties to the worker) and
         # sets Cargo's jobs to that machine's cap; device class and --run stay local
       tasks place [--run | --device SERIAL] [--peer NAME] [--json] -- COMMAND [ARGS...]
         # dry run: print where --anywhere (or --peer) would run it and why; submits nothing
       tasks peer init --bind IP:PORT [--config PATH] | id | add NAME ENDPOINT_ID ADDR [--role worker|host|both|observer] [--approve auto|manual] [--config PATH] | list [--json] | status-watch NAME
       tasks approve-command JOB_ID  # trusted LIVE cwd/argv, not a source snapshot
       # local.command cost=cpu_simulation is a legacy label, NOT the resource class
       tasks approve-android-trial JOB_ID   # EXPERIMENTAL build-only; trusts local source/host config
       tasks catalog | cache | cache-plan | admission | start-worker | worker
       tasks android-submit [--who OWNER] SOURCE REVISION phone|emulator|x86_64,arm64-v8a
       tasks android-plan SOURCE REVISION [emulator|phone|x86_64,arm64-v8a]  # default emulator
       tasks android-capture SOURCE REVISION NEW_DESTINATION
       tasks android-host SOURCE [emulator|phone|x86_64,arm64-v8a]  # read-only host audit
       # phone = arm64-v8a only, emulator = x86_64 only; SPEC.json config may use either
       tasks devices [--json]  # read-only: connected phones/emulators, ABI, ready, lock free/busy
       tasks phone-test [--who OWNER] SOURCE [--device SERIAL] [--follow | --full-log | --tail N]
         # build phone APK, install -r, launch; default: step progress, then the last 40 log lines
       tasks status [--json]  # this machine: CPU/memory/PSI, slots, jobs with live use, progress, ETA
       tasks tmp-usage [--json] [PATH...]  # read-only bounded /tmp+cache inventory; never deletes
       tasks alerts [--json] [--all] [--severity info|warn|critical] [--node NAME]
         # health alerts of this machine and its peers, disks, missing capabilities
       tasks alerts clear NODE [CAPABILITY]  # drop a node's missing-capability mark(s)
       tasks doctor [--json]  # node-setup checks with fix lines; exit 1 when this machine is unhealthy
       tasks record SUITE CONFIG STAGE SECONDS
       tasks daemon
       tasks mcp [--no-start-daemon]  # MCP server for agents (stdio JSON-RPC)";

fn main() {
    if let Err(error) = run() {
        eprintln!("tasks: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let run_now = args.first().is_some_and(|s| s == "run");
    let plain_logs = args.first().is_some_and(|s| s == "logs") && args.len() == 2;
    let mut output = options::Output::Full;
    // A placement decision to record once the job id is known (`--anywhere`).
    let mut placed: Option<tinline_task_manager::placement::Decision> = None;
    let action = match args.first().map(String::as_str) {
        None | Some("ui") if args.len() <= 1 => return open_ui(None),
        Some("ui") if args.len() == 3 && args[1] == "--fixtures" => {
            return open_ui(Some(std::path::PathBuf::from(&args[2])));
        }
        Some("daemon") if args.len() == 1 => return daemon::run(),
        Some("peer-forward") => {
            std::process::exit(tinline_task_manager::peer::bridge::run(&args[1..]));
        }
        Some("peer") => return peer_command(&args[1..]),
        Some("mcp") => return mcp_command(&args[1..]),
        Some("place") => return place_command(&args[1..]),
        Some("catalog") if args.len() == 1 => {
            println!(
                "{}",
                serde_json::to_string_pretty(&tinline_task_manager::catalog::catalog())?
            );
            return Ok(());
        }
        Some("admission") if args.len() == 1 => {
            let status = tinline_task_manager::admission::status()?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            if let Some(policy) = &status.policy {
                eprint!("{}", policy.summary());
            }
            return Ok(());
        }
        Some("cache") if args.len() == 1 => {
            println!(
                "{}",
                serde_json::to_string_pretty(&tinline_task_manager::cache::report(&state_dir())?)?
            );
            return Ok(());
        }
        Some("cache-plan") if args.len() == 1 => {
            println!(
                "{}",
                serde_json::to_string_pretty(&tinline_task_manager::retention::plan()?)?
            );
            return Ok(());
        }
        Some("android-plan") if matches!(args.len(), 3 | 4) => {
            let abis = tinline_task_manager::android::Abis::parse(
                args.get(3).map(String::as_str).unwrap_or("x86_64"),
            )?;
            let plan =
                tinline_task_manager::android::preflight(Path::new(&args[1]), &args[2], abis)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            return Ok(());
        }
        Some("android-host") if matches!(args.len(), 2 | 3) => {
            let abis = tinline_task_manager::android::Abis::parse(
                args.get(2).map(String::as_str).unwrap_or("x86_64"),
            )?;
            let host =
                tinline_task_manager::android_build::Host::discover(Path::new(&args[1]), abis)?;
            host.verify()?;
            println!("{}", serde_json::to_string_pretty(&host)?);
            return Ok(());
        }
        Some("devices") if args.len() == 1 || (args.len() == 2 && args[1] == "--json") => {
            let devices = tinline_task_manager::device::list()?;
            if args.len() == 2 {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                print_devices(&devices);
            }
            return Ok(());
        }
        Some("phone-test") => return phone_test(&args[1..]),
        Some("status") if args.len() == 1 || (args.len() == 2 && args[1] == "--json") => {
            return status::run(args.len() == 2);
        }
        Some("tmp-usage") => return tinline_task_manager::tmp_usage::cli(&args[1..]),
        Some("alerts") => return alerts::run(&args[1..]),
        Some("doctor") => {
            if !doctor::run(&args[1..])? {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some("__phone-install") if args.len() == 3 => {
            return tinline_task_manager::device::install_and_launch(&args[1], Path::new(&args[2]));
        }
        Some("android-capture") if args.len() == 4 => {
            let receipt = tinline_task_manager::workspace_snapshot::capture(
                Path::new(&args[1]),
                &args[2],
                Path::new(&args[3]),
            )?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            return Ok(());
        }
        Some("worker") if args.len() == 1 => return tinline_task_manager::worker::run(),
        Some("start-worker") if args.len() == 1 => Action::StartWorker,
        Some("approve") if args.len() == 2 => Action::Approve(args[1].parse()?),
        Some("approve-command") if args.len() == 2 => Action::ApproveCommand(args[1].parse()?),
        Some("enqueue" | "run") => {
            use tinline_task_manager::local_command::{Recipe, Resource};
            let split = args.iter().position(|s| s == "--").ok_or(format!(
                "{} requires -- before the command; usage: {}",
                args[0],
                options::RUN_USAGE
            ))?;
            let parsed = options::enqueue(&args[1..split], run_now)?;
            let owner = parsed
                .owner
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "local-agent".into()));
            if parsed.follow {
                output = options::Output::Follow;
            }
            if owner.trim().is_empty() || owner.len() > 128 {
                return Err("bounded owner required".into());
            }
            let resource = match parsed.class {
                None => Resource::Compile,
                Some(options::Class::Run) => Resource::Run,
                Some(options::Class::Offload) => Resource::Offload,
                Some(options::Class::Device(serial)) => {
                    Resource::Device(tinline_task_manager::device::lock_serial(&serial)?)
                }
            };
            let recipe = Recipe {
                broad_test: parsed.broad_test,
                argv: args[split + 1..].to_vec(),
                resource,
            };
            // Submission policy (crate::policy): refuse before anything is queued, or warn.
            match recipe.policy_verdict(
                &std::env::current_dir()?,
                &tinline_task_manager::policy::effective(),
            ) {
                tinline_task_manager::policy::Verdict::Refused(message) => {
                    eprintln!("tasks: {message}");
                    std::process::exit(2);
                }
                tinline_task_manager::policy::Verdict::Warn(lines) => {
                    for line in lines {
                        eprintln!("tasks: warning: {line}");
                    }
                }
                tinline_task_manager::policy::Verdict::Allowed => {}
            }
            match &parsed.peer {
                None if parsed.anywhere => {
                    let (spec, decision) = anywhere_spec(recipe, owner, &parsed.fetch)?;
                    placed = decision.decided().then_some(decision);
                    Action::Submit(spec)
                }
                None => Action::Submit(recipe.spec(owner, std::env::current_dir()?)?),
                Some(peer) => Action::Submit(forward_spec(
                    peer,
                    &parsed.fetch,
                    recipe.unchecked_spec(owner, std::env::current_dir()?)?,
                )?),
            }
        }
        Some("android-submit") => {
            // Submission only; approve-android-trial and start-worker stay separate.
            let (owner, rest) = match &args[1..] {
                [flag, owner, rest @ ..] if flag == "--who" => (owner.clone(), rest),
                rest => (
                    std::env::var("USER").unwrap_or_else(|_| "local-agent".into()),
                    rest,
                ),
            };
            let [source, revision, target] = rest else {
                return Err(USAGE.into());
            };
            if owner.trim().is_empty() || owner.len() > 128 {
                return Err("bounded owner required".into());
            }
            Action::Submit(JobSpec {
                suite: tinline_task_manager::catalog::ANDROID_SUITE.into(),
                configuration: tinline_task_manager::catalog::android_configuration(target)?,
                cost: tinline_task_manager::Cost::CpuSimulation,
                source: source.clone(),
                revision: revision.clone(),
                owner,
            })
        }
        Some("approve-android-trial") if args.len() == 2 => {
            Action::ApproveAndroidTrial(args[1].parse()?)
        }
        Some("logs") if args.len() == 2 || (args.len() == 3 && args[2] == "--json") => {
            Action::Logs(args[1].parse()?)
        }
        Some("__stage") if args.len() == 5 => {
            return tinline_task_manager::worker::stage(
                args[1].parse()?,
                &args[2],
                tinline_task_manager::execution::ProcessIdentity {
                    pid: args[3].parse()?,
                    start_ticks: args[4].parse()?,
                },
            );
        }
        Some("__admit") if args.len() == 5 => {
            return tinline_task_manager::worker::admit(
                args[1].parse()?,
                &args[2],
                tinline_task_manager::execution::ProcessIdentity {
                    pid: args[3].parse()?,
                    start_ticks: args[4].parse()?,
                },
            );
        }
        Some("__command") if args.len() == 5 => {
            return tinline_task_manager::worker::command(
                args[1].parse()?,
                &args[2],
                tinline_task_manager::execution::ProcessIdentity {
                    pid: args[3].parse()?,
                    start_ticks: args[4].parse()?,
                },
            );
        }
        Some("list") if args.len() == 1 => Action::List,
        Some("list") => return list(&args[1..]),
        Some("submit") if args.len() == 2 => {
            let mut spec: JobSpec = serde_json::from_slice(&fs::read(&args[1])?)?;
            if spec.suite == tinline_task_manager::catalog::ANDROID_SUITE {
                spec.configuration =
                    tinline_task_manager::catalog::android_configuration(&spec.configuration)?;
            }
            Action::Submit(spec)
        }
        Some("cancel") if args.len() == 2 => Action::Cancel(args[1].parse()?),
        Some("record") if args.len() == 5 => Action::Record(RuntimeSample {
            suite: args[1].clone(),
            configuration: args[2].clone(),
            stage: args[3].clone(),
            seconds: args[4].parse()?,
        }),
        _ => return Err(USAGE.into()),
    };
    let socket = state_dir().join("taskd.sock");
    let focus = match &action {
        Action::Logs(id)
        | Action::Approve(id)
        | Action::ApproveCommand(id)
        | Action::ApproveAndroidTrial(id) => Some(*id),
        _ => None,
    };
    let submitting = matches!(action, Action::Submit(_));
    let response = request_with_reconnect(&socket, action, || ensure_daemon(&socket))?;
    if plain_logs {
        if let Some(error) = &response.error {
            eprintln!("tasks: {error}");
        } else if let Some(log) = &response.log {
            print!("{}", log.text);
        }
    } else if !run_now || response.error.is_some() {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    // Human-only notes go to stderr; stdout JSON is unchanged for scripts.
    if let Some(job) = focus.and_then(|id| response.jobs.iter().find(|j| j.id == id)) {
        eprintln!(
            "{}",
            display::job_status_line(job, response.execution_enabled)
        );
    }
    if response.error.is_some() {
        std::process::exit(1);
    }
    if let (Some(decision), Some(id)) = (&placed, response.submitted_id)
        && let Err(e) = tinline_task_manager::placement::record(
            &fs::canonicalize(state_dir()).unwrap_or_else(|_| state_dir()),
            id,
            decision,
        )
    {
        eprintln!("tasks: could not record the placement of job {id}: {e}");
    }
    if submitting && let Some(id) = response.submitted_id {
        let job = response.jobs.iter().find(|j| j.id == id);
        let cwd = job.map_or("?", |j| j.spec.source.as_str());
        if run_now {
            eprintln!("{}", display::submit_line(id, cwd, job));
        } else {
            eprintln!(
                "tasks: enqueued job {id}; cwd {cwd}; approve with tasks approve-command {id}, then tasks start-worker"
            );
        }
        if let Some(warning) = display::shared_cwd_warning(&response.jobs, id) {
            eprintln!("{warning}");
        }
    }
    if run_now {
        let id = response
            .submitted_id
            .ok_or("daemon did not return submitted_id; do not resubmit blindly")?;
        // The submit line above carries the scripts' `submitted job N;` prefix;
        // this adds the never-resubmit guidance once, before any wait can fail.
        eprintln!(
            "tasks: if this client stops or times out, {}",
            tinline_task_manager::live_job_hint(id)
        );
        tinline_task_manager::print_hint_on_detach(id);
        let waited = approve_and_start(&socket, Action::ApproveCommand(id), id)
            .and_then(|()| wait_job(&socket, id, output));
        match waited {
            Ok((_, code)) => std::process::exit(code),
            Err(e) => {
                return Err(
                    format!("{e}\ntasks: {}", tinline_task_manager::live_job_hint(id)).into(),
                );
            }
        }
    }
    Ok(())
}

/// `tasks run --peer NAME`: checks that this machine's daemon has a peer runtime that knows
/// NAME as a worker, then wraps the command into the local forwarding job (the seam
/// `peer_link::forwarded_spec`).
fn forward_spec(peer: &str, fetch: &[String], inner: JobSpec) -> Result<JobSpec> {
    let socket = state_dir().join("taskd.sock");
    ensure_daemon(&socket)?;
    let mut report =
        tinline_task_manager::peer_link::request_peers(&socket, Duration::from_secs(30))?;
    // A daemon that just started may still be binding its endpoint (enabled=false, no error).
    let end = Instant::now() + Duration::from_secs(10);
    while !report.enabled && report.error.is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
        report = tinline_task_manager::peer_link::request_peers(&socket, Duration::from_secs(30))?;
    }
    if !report.enabled {
        return Err(format!(
            "peer forwarding is not enabled in this machine's daemon ({}; config {}). Set it up \
             with `tasks peer init --bind IP:PORT`, add the peer, then restart the daemon when it \
             is idle",
            report.error.unwrap_or_default(),
            report.config.unwrap_or_default()
        )
        .into());
    }
    match report.peers.iter().find(|p| p.name == peer) {
        None => {
            return Err(format!(
                "unknown peer {peer:?}; configured: {}",
                report
                    .peers
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.role))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
        Some(p) if !matches!(p.role.as_str(), "worker" | "both") => {
            return Err(format!(
                "peer {peer} has role {}; only a worker/both peer runs forwarded jobs",
                p.role
            )
            .into());
        }
        Some(_) => {}
    }
    tinline_task_manager::peer_link::forwarded_spec(
        &inner,
        peer,
        fetch,
        &fs::canonicalize(state_dir())?,
        &std::env::current_exe()?,
    )
}

/// `tasks run --anywhere`: the library placement over the peer link, then the job for the
/// chosen machine (a local live command or the forwarding job). Prints the decision.
fn anywhere_spec(
    recipe: tinline_task_manager::local_command::Recipe,
    owner: String,
    fetch: &[String],
) -> Result<(JobSpec, tinline_task_manager::placement::Decision)> {
    use tinline_task_manager::placement;
    let socket = state_dir().join("taskd.sock");
    ensure_daemon(&socket)?;
    let mut inputs = placement::gather(&socket, Duration::from_secs(15))?;
    let cwd = std::env::current_dir().ok();
    placement::retain_needed(&mut inputs.peers, &recipe.argv, cwd.as_deref());
    let class = place_class(&recipe.resource);
    let decision =
        placement::decide_with(&recipe.argv, &class, &placement::Request::Anywhere, &inputs)?;
    eprintln!("tasks: {}", decision.line());
    for note in &decision.notes {
        eprintln!("tasks: note: {note}");
    }
    let spec = placement::job_spec(
        &decision,
        recipe.resource,
        recipe.broad_test,
        owner,
        std::env::current_dir()?,
        fetch,
        &fs::canonicalize(state_dir())?,
        &std::env::current_exe()?,
    )?;
    Ok((spec, decision))
}

fn place_class(
    resource: &tinline_task_manager::local_command::Resource,
) -> tinline_task_manager::placement::Class {
    use tinline_task_manager::local_command::Resource;
    use tinline_task_manager::placement::Class;
    match resource {
        Resource::Run => Class::Run,
        Resource::Device(serial) => Class::Device(serial.clone()),
        Resource::Compile | Resource::Offload => Class::Compile,
    }
}

/// `tasks place [--run | --device S] [--peer NAME] [--json] -- CMD`: a dry run of placement.
fn place_command(args: &[String]) -> Result<()> {
    use tinline_task_manager::placement;
    let split = args.iter().position(|s| s == "--").ok_or(format!(
        "tasks place requires -- before the command; usage: {}",
        options::PLACE_USAGE
    ))?;
    let parsed = options::place(&args[..split])?;
    let argv = args[split + 1..].to_vec();
    if argv.is_empty() {
        return Err(format!("usage: {}", options::PLACE_USAGE).into());
    }
    let class = match &parsed.class {
        None => placement::Class::Compile,
        Some(options::Class::Run) => placement::Class::Run,
        Some(options::Class::Device(s)) => {
            placement::Class::Device(tinline_task_manager::device::lock_serial(s)?)
        }
        Some(options::Class::Offload) => return Err(options::PLACE_USAGE.into()),
    };
    let request = match &parsed.peer {
        Some(name) => placement::Request::parse(name)?,
        None => placement::Request::Anywhere,
    };
    let socket = state_dir().join("taskd.sock");
    ensure_daemon(&socket)?;
    let mut inputs = placement::gather(&socket, Duration::from_secs(15))?;
    let cwd = std::env::current_dir().ok();
    placement::retain_needed(&mut inputs.peers, &argv, cwd.as_deref());
    let decision = placement::decide_with(&argv, &class, &request, &inputs)?;
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&decision)?);
        return Ok(());
    }
    println!("{}", decision.line());
    for s in &decision.scores {
        println!(
            "  {:<16} total ~{:>4}  wait ~{:>4} [{}]  run ~{:>4} [{}]  cargo jobs {}",
            s.machine,
            placement::duration(s.total_s),
            placement::duration(s.wait_s),
            s.wait_basis,
            placement::duration(s.run_s),
            s.run_basis,
            s.cargo_jobs.map_or("?".into(), |j| j.to_string())
        );
    }
    for s in &decision.skipped {
        println!("  {:<16} skipped: {}", s.machine, s.why);
    }
    for note in &decision.notes {
        println!("  note: {note}");
    }
    println!("  would run: {}", decision.argv.join(" "));
    Ok(())
}

/// `tasks peer ...`: set-up and inspection of the peer link.
fn peer_command(args: &[String]) -> Result<()> {
    use tinline_task_manager::peer::net::{PeerConfig, config};
    let default_config = || -> Result<std::path::PathBuf> {
        PeerConfig::default_path().ok_or_else(|| "HOME is not set".into())
    };
    match args.first().map(String::as_str) {
        Some("init") => {
            let (mut bind, mut path) = (None, None);
            let mut it = args[1..].iter();
            while let Some(flag) = it.next() {
                let value = it.next().ok_or(format!("{flag} needs a value"))?.clone();
                match flag.as_str() {
                    "--bind" => bind = Some(value),
                    "--config" => path = Some(std::path::PathBuf::from(value)),
                    other => return Err(format!("unknown option {other}").into()),
                }
            }
            let path = match path {
                Some(p) => p,
                None => default_config()?,
            };
            if path.exists() {
                let c = PeerConfig::load(&path)?;
                println!("{}", c.secret_key()?.public());
                eprintln!("tasks: {} already exists; endpoint id printed above", path.display());
                return Ok(());
            }
            let bind = bind.ok_or("tasks peer init needs --bind IP:PORT (this machine's LAN address and a UDP port, e.g. 192.168.1.10:7842)")?;
            bind.parse::<std::net::SocketAddr>()
                .map_err(|e| format!("--bind {bind}: {e}"))?;
            let dir = path.parent().ok_or("config path has no directory")?;
            fs::create_dir_all(dir)?;
            let key_path = dir.join("peer.key");
            let key = if key_path.exists() {
                config::load_secret_key(&key_path)?
            } else {
                config::create_secret_key(&key_path)?
            };
            let text = format!(
                r#"# tasks peer link (iroh, LAN only). Restart an idle daemon to apply changes.
secret_key = "peer.key"
bind = "{bind}"
# checkouts = "~/offload/checkouts"
# reattach_timeout_s = 3600

# [[peer]]
# name = "worker"
# endpoint_id = "<its `tasks peer id`>"
# addr = "192.168.1.7:7842"
# role = "worker"   # worker | host | both | observer
# approve = "auto"  # for a host peer: auto | manual
"#
            );
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?
                .write_all(text.as_bytes())?;
            println!("{}", key.public());
            eprintln!(
                "tasks: wrote {} and {}; endpoint id printed above (give it to the other machine's [[peer]] entry)",
                path.display(),
                key_path.display()
            );
            Ok(())
        }
        Some("add") => peer_add(&args[1..], default_config),
        Some("id") if args.len() == 1 => {
            let c = PeerConfig::load(&default_config()?)?;
            println!("{}", c.secret_key()?.public());
            Ok(())
        }
        Some("list") if args.len() <= 2 => {
            let socket = state_dir().join("taskd.sock");
            ensure_daemon(&socket)?;
            let report =
                tinline_task_manager::peer_link::request_peers(&socket, Duration::from_secs(30))?;
            if args.get(1).is_some_and(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            println!(
                "peer link: {}{}",
                if report.enabled {
                    "enabled"
                } else if report.error.is_none() {
                    "starting"
                } else {
                    "disabled"
                },
                report
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            );
            if let Some(id) = &report.endpoint_id {
                println!("endpoint {id} on {}", report.bind.as_deref().unwrap_or("?"));
            }
            for p in &report.peers {
                println!(
                    "  {:<16} {:<8} {} {}",
                    p.name,
                    p.role,
                    p.addr.as_deref().unwrap_or("-"),
                    p.endpoint_id
                );
            }
            for l in report.forwarded.iter().rev().take(10) {
                println!(
                    "  job {} -> {} #{} {} {}",
                    l["local_job_id"],
                    l["peer"].as_str().unwrap_or("?"),
                    l["remote_job_id"],
                    l["stage"].as_str().unwrap_or("?"),
                    l["remote_state"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Some("status-watch") if args.len() == 2 => {
            use std::io::{BufRead, BufReader, Write};
            let sock = state_dir().join("peer.sock");
            let mut stream = UnixStream::connect(&sock)
                .map_err(|e| format!("{}: {e} (is the peer link enabled? tasks peer list)", sock.display()))?;
            let request = serde_json::json!({"op": "watch", "peer": args[1]});
            stream.write_all(format!("{request}
").as_bytes())?;
            let mut out = std::io::stdout().lock();
            for line in BufReader::new(stream).lines() {
                let event: serde_json::Value = serde_json::from_str(&line?)?;
                match event["event"].as_str() {
                    Some("line") => {
                        writeln!(out, "{}", event["value"])?;
                        out.flush()?;
                    }
                    Some("error") => return Err(event["message"].to_string().into()),
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Err("usage: tasks peer init --bind IP:PORT [--config PATH] | id | add NAME ENDPOINT_ID ADDR [--role worker|host|both|observer] [--approve auto|manual] [--config PATH] | list [--json] | status-watch NAME".into()),
    }
}

/// `tasks peer add NAME ENDPOINT_ID ADDR`: append one `[[peer]]` entry (default role `worker`,
/// as when pairing a new worker from the line it prints at first boot). The whole resulting
/// file is validated before it replaces the old one (atomically, 0600); nothing is restarted.
fn peer_add(
    args: &[String],
    default_config: impl Fn() -> Result<std::path::PathBuf>,
) -> Result<()> {
    use tinline_task_manager::peer::net::{PeerConfig, config};
    let mut positional = Vec::new();
    let (mut role, mut approve, mut path) = ("worker".to_owned(), None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if !arg.starts_with("--") {
            positional.push(arg.clone());
            continue;
        }
        let value = it.next().ok_or(format!("{arg} needs a value"))?.clone();
        match arg.as_str() {
            "--role" => role = value,
            "--approve" => approve = Some(value),
            "--config" => path = Some(std::path::PathBuf::from(value)),
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    let [name, id, addr] = <[String; 3]>::try_from(positional).map_err(|_| {
        "usage: tasks peer add NAME ENDPOINT_ID ADDR [--role worker|host|both|observer] [--approve auto|manual] [--config PATH]"
    })?;
    // Checked before formatting, so no value can change the TOML structure.
    if !config::valid_name(&name) {
        return Err(format!("peer name {name:?} must match [A-Za-z0-9._-]{{1,64}}").into());
    }
    let key: tinline_task_manager::peer::net::iroh::PublicKey = id
        .trim()
        .parse()
        .map_err(|e| format!("endpoint id {id:?}: {e}"))?;
    let addr: std::net::SocketAddr = addr.parse().map_err(|e| format!("addr {addr:?}: {e}"))?;
    if !matches!(role.as_str(), "worker" | "host" | "both" | "observer") {
        return Err(format!("--role {role:?}: worker | host | both | observer").into());
    }
    if let Some(a) = &approve
        && !matches!(a.as_str(), "auto" | "manual")
    {
        return Err(format!("--approve {a:?}: auto | manual").into());
    }
    let path = match path {
        Some(p) => p,
        None => default_config()?,
    };
    let current = fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e} (create it first with `tasks peer init --bind IP:PORT`)",
            path.display()
        )
    })?;
    let mut entry = format!(
        "\n[[peer]]\nname = \"{name}\"\nendpoint_id = \"{key}\"\naddr = \"{addr}\"\nrole = \"{role}\"\n"
    );
    if let Some(a) = approve {
        entry.push_str(&format!("approve = \"{a}\"\n"));
    }
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&entry);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    PeerConfig::parse(&text, &path, home.as_deref())?;
    let dir = path.parent().ok_or("config path has no directory")?;
    let tmp = dir.join(format!(
        ".{}.add-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("peer.toml"),
        std::process::id()
    ));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    println!(
        "added peer {name} ({role}) {key} at {addr} to {}",
        path.display()
    );
    eprintln!("tasks: restart the daemon when it is idle (tasks list --active) to apply it");
    Ok(())
}

fn call(socket: &Path, action: Action) -> Result<tinline_task_manager::Response> {
    request_with_reconnect(socket, action, || ensure_daemon(socket))
}

fn approve_and_start(socket: &Path, approve: Action, id: i64) -> Result<()> {
    for action in [approve, Action::StartWorker] {
        if let Some(e) = call(socket, action)?.error {
            return Err(format!("job {id}: {e}; inspect before retry").into());
        }
    }
    Ok(())
}

/// Follows one job to a terminal phase, prints its log and summary, returns the job
/// and the exit code `tasks run` uses (0 passed, 130 cancelled, else the stage code).
/// `output` chooses whether the log streams live, prints at the end, or is reduced to
/// progress lines and a bounded tail.
fn wait_job(
    socket: &Path,
    id: i64,
    output: options::Output,
) -> Result<(tinline_task_manager::Job, i32)> {
    use options::Output;
    let mut previous = None;
    let mut live = LiveLog {
        id,
        output,
        follower: follow::LogFollower::new(follow::job_log(&state_dir(), id)),
        lines: follow::Lines::default(),
        streaming: output != Output::Full,
        at_line_start: true,
    };
    // `Job(id)` returns one job; a daemon older than it answers "unknown variant".
    let mut single = true;
    loop {
        let mut r = call(
            socket,
            if single {
                Action::Job(id)
            } else {
                Action::List
            },
        )?;
        if single
            && r.error
                .as_deref()
                .is_some_and(|e| e.contains("unknown variant"))
        {
            single = false;
            r = call(socket, Action::List)?;
        }
        if let Some(e) = r.error {
            return Err(format!("job {id}: {e}").into());
        }
        let j = r
            .jobs
            .into_iter()
            .find(|j| j.id == id)
            .ok_or("submitted job missing")?;
        if previous.as_ref() != Some(&j.phase) {
            eprintln!("tasks: job {id} {:?}", j.phase);
            previous = Some(j.phase.clone());
        }
        live.pump(false);
        if j.phase.terminal() {
            live.pump(true);
            let followed = output == Output::Follow && live.streaming && live.follower.offset() > 0;
            let text = if followed {
                // Already printed; only make sure the summary starts on its own line.
                String::new()
            } else {
                let logs = call(socket, Action::Logs(id))?;
                if let Some(error) = logs.error {
                    eprintln!("tasks: job {id} log unavailable: {error}");
                }
                let text = logs.log.map(|log| log.text).unwrap_or_default();
                match output {
                    Output::Progress { tail } => {
                        let (shown, omitted) = follow::tail(&text, tail);
                        if omitted > 0 {
                            eprintln!(
                                "tasks: job {id}: {omitted} earlier log lines omitted; tasks logs {id} (or --full-log) shows the retained log"
                            );
                        }
                        shown.to_owned()
                    }
                    _ => text,
                }
            };
            // Like the live stream, ignore a closed stdout (`| head`): print!
            // would panic with exit 101 and hide the job's own exit code.
            let mut tail = text;
            if !tail.is_empty() && !tail.ends_with('\n') {
                tail.push('\n');
            }
            if followed && !live.at_line_start {
                tail.push('\n');
            }
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout
                .write_all(tail.as_bytes())
                .and_then(|()| stdout.flush());
            let code = if j.phase == tinline_task_manager::Phase::Passed {
                0
            } else if j.phase == tinline_task_manager::Phase::Cancelled {
                130
            } else {
                j.execution
                    .as_ref()
                    .and_then(|e| e.stages.last())
                    .and_then(|s| s.exit_code)
                    .filter(|c| *c != 0)
                    .unwrap_or(1)
            };
            let phase = serde_json::to_value(&j.phase)?;
            println!(
                "job {id} {} (exit {code}) owner={}",
                phase.as_str().unwrap_or("unknown"),
                j.spec.owner
            );
            return Ok((j, code));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Streams a job's log file while a client waits (`--follow`, phone-test progress).
struct LiveLog {
    id: i64,
    output: options::Output,
    follower: follow::LogFollower,
    lines: follow::Lines,
    /// Cleared when the file cannot be read; the log is then shown at the end.
    streaming: bool,
    at_line_start: bool,
}

impl LiveLog {
    fn pump(&mut self, drain: bool) {
        use std::io::Write;
        if !self.streaming {
            return;
        }
        let id = self.id;
        let bytes = if drain {
            self.follower.drain()
        } else {
            self.follower.poll()
        };
        match bytes {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) if self.output == options::Output::Follow => {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&bytes).and_then(|_| stdout.flush());
                self.at_line_start = bytes.ends_with(b"\n");
            }
            Ok(bytes) => {
                for line in self.lines.push(&bytes) {
                    if let Some(step) = follow::progress(&line) {
                        eprintln!("tasks: job {id} step: {step}");
                    }
                }
            }
            Err(error) => {
                eprintln!("tasks: job {id}: cannot stream log ({error}); showing it at the end");
                self.streaming = false;
            }
        }
    }
}

fn print_devices(devices: &[tinline_task_manager::device::Device]) {
    if devices.is_empty() {
        println!("no devices connected (adb devices is empty)");
        return;
    }
    println!(
        "{:<22} {:<9} {:<13} {:<12} {:<18} AVAILABILITY",
        "SERIAL", "KIND", "STATE", "ABI", "MODEL"
    );
    let denylist = tinline_task_manager::device_denylist::Denylist::load();
    for d in devices {
        let denied = match &denylist {
            Ok(list) => list.denied(&d.serial).is_some(),
            Err(_) => true,
        };
        let availability = match (&d.holder, d.busy) {
            _ if denied => "DENIED: agents must never use this device".into(),
            (Some(who), _) => format!("busy: {who}"),
            (None, true) => "busy".into(),
            (None, false) => "free".into(),
        };
        let state = if d.ready() { "ready" } else { d.state.as_str() };
        println!(
            "{:<22} {:<9} {:<13} {:<12} {:<18} {availability}",
            d.serial,
            d.kind,
            state,
            d.abis.first().map(String::as_str).unwrap_or("-"),
            d.model.as_deref().unwrap_or("-"),
        );
    }
}

/// Phone build (arm64-v8a only) through the typed Android trial, then install + launch
/// in a separate job that holds only the device lock. Invoking this command is the
/// explicit per-job trial approval; nothing else is approved or replayed.
fn phone_test(args: &[String]) -> Result<()> {
    use tinline_task_manager::local_command::{Recipe, Resource};
    use tinline_task_manager::{catalog, device};
    let options::PhoneTest {
        owner,
        source,
        serial,
        output,
    } = options::phone_test(
        args,
        std::env::var("USER").unwrap_or_else(|_| "local-agent".into()),
    )?;
    if owner.trim().is_empty() || owner.len() > 128 {
        return Err("bounded owner required".into());
    }
    let source = fs::canonicalize(source)?;
    // Fail before compiling if the phone is missing, unauthorized or not arm64.
    let phone = device::select_phone(&device::list()?, serial.as_deref())?;
    if phone.busy {
        eprintln!(
            "tasks: {} is busy ({}); install will wait for it",
            phone.serial,
            phone.holder.as_deref().unwrap_or("unknown holder")
        );
    }
    let head = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let revision = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if !head.status.success() {
        return Err("SOURCE must be a Git checkout with a HEAD commit".into());
    }
    let socket = state_dir().join("taskd.sock");
    let submitted = call(
        &socket,
        Action::Submit(JobSpec {
            suite: catalog::ANDROID_SUITE.into(),
            configuration: catalog::android_configuration("phone")?,
            cost: tinline_task_manager::Cost::CpuSimulation,
            source: source.to_str().ok_or("UTF-8 source path required")?.into(),
            revision,
            owner: owner.clone(),
        }),
    )?;
    if let Some(e) = submitted.error {
        return Err(e.into());
    }
    let build = submitted
        .submitted_id
        .ok_or("daemon did not return submitted_id; do not resubmit blindly")?;
    eprintln!(
        "tasks: phone build job {build} for {}; Ctrl-C detaches",
        phone.serial
    );
    approve_and_start(&socket, Action::ApproveAndroidTrial(build), build)?;
    let (job, code) = wait_job(&socket, build, output)?;
    if code != 0 {
        std::process::exit(code);
    }
    let artifact = job
        .execution
        .and_then(|e| e.build_artifact)
        .ok_or("build passed without an APK receipt; not installing")?;
    let apk = fs::canonicalize(state_dir())?
        .join("jobs")
        .join(build.to_string())
        .join(&artifact.path);
    let recipe = Recipe {
        broad_test: false,
        argv: vec![
            std::env::current_exe()?
                .to_str()
                .ok_or("UTF-8 tasks path required")?
                .into(),
            "__phone-install".into(),
            phone.serial.clone(),
            apk.to_str().ok_or("UTF-8 APK path required")?.into(),
        ],
        resource: Resource::Device(phone.lock.clone()),
    };
    let submitted = call(&socket, Action::Submit(recipe.spec(owner, source)?))?;
    if let Some(e) = submitted.error {
        return Err(e.into());
    }
    let install = submitted
        .submitted_id
        .ok_or("daemon did not return submitted_id; do not resubmit blindly")?;
    eprintln!("tasks: install job {install} on {}", phone.serial);
    approve_and_start(&socket, Action::ApproveCommand(install), install)?;
    std::process::exit(wait_job(&socket, install, output)?.1);
}

/// `tasks list` with options: filtered table (default) or filtered JSON (`--json`).
fn list(args: &[String]) -> Result<()> {
    let options = options::list(args)?;
    let socket = state_dir().join("taskd.sock");
    let mut response = call(&socket, Action::List)?;
    if let Some(error) = &response.error {
        eprintln!("tasks: {error}");
        std::process::exit(1);
    }
    let all = std::mem::take(&mut response.jobs);
    response.jobs = display::select(
        all.clone(),
        options.who.as_deref(),
        options.active,
        options.limit,
    );
    if options.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    let now = tinline_task_manager::execution::now();
    let home = std::env::var("HOME").ok();
    print!(
        "{}",
        display::job_table(&response.jobs, now, home.as_deref())
    );
    if options.active {
        match tinline_task_manager::admission::status() {
            Ok(status) => {
                let note =
                    |slot: String, n: &tinline_task_manager::admission::Note| display::Holder {
                        slot,
                        who: n.who.clone(),
                        pid: Some(n.pid),
                        since: n.since,
                    };
                let holders: Vec<_> = status
                    .resources
                    .iter()
                    .filter(|r| r.busy)
                    .map(|r| {
                        let slot = display::slot_name(
                            &r.path.file_name().unwrap_or_default().to_string_lossy(),
                        );
                        match &r.holder {
                            Some(n) => note(slot, n),
                            None => display::Holder {
                                slot,
                                who: "(no live holder note)".into(),
                                pid: None,
                                since: now as f64,
                            },
                        }
                    })
                    .collect();
                let waiting: Vec<_> = status
                    .waiting
                    .iter()
                    .map(|n| note(n.slot.clone().unwrap_or_else(|| "-".into()), n))
                    .collect();
                print!(
                    "{}",
                    display::holders_section(&holders, &waiting, &all, now)
                );
            }
            Err(error) => eprintln!("tasks: live admission unavailable: {error}"),
        }
    }
    eprintln!("tasks: {}", display::TABLE_LEGEND);
    Ok(())
}

#[cfg(feature = "mcp")]
fn mcp_command(args: &[String]) -> Result<()> {
    mcp::run(args)
}

#[cfg(not(feature = "mcp"))]
fn mcp_command(_args: &[String]) -> Result<()> {
    Err(without_feature("mcp"))
}

/// The error of a subcommand whose Cargo feature is off; `main` prints it and exits 2.
#[cfg(not(all(feature = "ui", feature = "mcp")))]
fn without_feature(feature: &str) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "built without {feature} (worker build): `tasks {feature}` needs a build with the \
         `{feature}` Cargo feature (default features); the daemon, CLI and peer link work here"
    )
    .into()
}

#[cfg(not(feature = "ui"))]
fn open_ui(_fixtures: Option<std::path::PathBuf>) -> Result<()> {
    Err(without_feature("ui"))
}

#[cfg(feature = "ui")]
fn open_ui(fixtures: Option<std::path::PathBuf>) -> Result<()> {
    let socket = state_dir().join("taskd.sock");
    // The window keeps retrying if this fails, and shows why in its status line.
    // A fixture demo contacts nothing, not even the daemon.
    if fixtures.is_none()
        && let Err(error) = ensure_daemon(&socket)
    {
        eprintln!("tasks: {error}");
    }
    ui::run(socket, fixtures).map_err(|e| e.into())
}

/// Starts `tasks daemon` detached (own process group, so closing a terminal or window
/// does not stop the queue). Concurrent starters are safe: the loser fails the lock.
pub(crate) fn ensure_daemon(socket: &Path) -> Result<()> {
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let state = state_dir();
    fs::create_dir_all(&state)?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    let log = fs::File::options()
        .create(true)
        .append(true)
        .open(state.join("taskd.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    // A client running inside an admitted job holds that job's slot lock descriptors (they
    // are deliberately inherited by the job's processes). A daemon it autostarts outlives the
    // job and must not keep them: mark every inherited descriptor above stdio close-on-exec.
    // SAFETY: only an async-signal-safe syscall between fork and exec; failure (kernels
    // before 5.11) leaves the previous behaviour.
    unsafe {
        command.pre_exec(|| {
            libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    // Reap it if it exits while this process (e.g. the window) is still running.
    std::thread::spawn(move || child.wait());
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!(
        "daemon did not start; see {}",
        state.join("taskd.log").display()
    )
    .into())
}
