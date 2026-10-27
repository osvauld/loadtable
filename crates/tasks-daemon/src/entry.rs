//! Command line of the daemon: `loadtabled serve`, `loadtabled install`, `loadtabled policy-sample`.
//!
//! The binary `loadtabled` is a thin wrapper over [`main_from_env`]; the CLI crate can call
//! [`run`] with the arguments after `loadtable daemon`.

use crate::config::{ConfigArgs, DaemonConfig};
use crate::error::DaemonError;
use crate::machine_policy::MachinePolicy;
use crate::server::{Daemon, DaemonOptions};
use std::path::PathBuf;
use std::process::ExitCode;
use tasks_platform::{Service, ServiceUnit};

/// The service name.
pub const SERVICE_NAME: &str = "loadtabled";
/// The service and daemon binary before the rename (R-804); a `tasks2d` link still runs the
/// daemon.
pub const OLD_SERVICE_NAME: &str = "tasks2d";

const USAGE: &str = "usage: loadtabled [serve] [OPTIONS]      run the daemon (also: loadtable daemon serve)\n\
       loadtabled install [--dry-run] [OPTIONS]  write the user service unit and print the next command\n\
       loadtabled policy-sample                  print a machine-policy.toml listing every default\n\
\n\
OPTIONS\n\
  --state-dir DIR   ledger, socket, logs (default: $XDG_STATE_HOME/loadtable)\n\
  --config-dir DIR  holds machine-policy.toml (default: $XDG_CONFIG_HOME/loadtable)\n\
  --lock-dir DIR    slot lock files (default: STATE_DIR/slots; never TMPDIR)\n\
  --cache-dir DIR   cache root: sccache, gradle, outputs (default: ~/build-cache; its disk is\n                    checked against the reserve)\n\
  --machine NAME    this machine's name in placement";

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the daemon.
    Serve(ConfigArgs),
    /// Install the service unit.
    Install {
        /// Directories to bake into the unit.
        args: ConfigArgs,
        /// Print the plan without writing the unit file.
        dry_run: bool,
    },
    /// Print the sample policy.
    PolicySample,
    /// Print usage.
    Help,
}

/// Parses `args` (without the program name).
pub fn parse_args(args: &[String]) -> Result<Command, DaemonError> {
    let mut it = args.iter();
    let sub: &str = match args.first().map(String::as_str) {
        Some(s @ ("serve" | "install" | "policy-sample")) => {
            it.next();
            s
        }
        Some("-h" | "--help" | "help") => return Ok(Command::Help),
        _ => "serve",
    };
    let mut cfg = ConfigArgs::default();
    let mut dry_run = false;
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| DaemonError::Usage(format!("{name} needs a value")))
        };
        match a.as_str() {
            "--state-dir" => cfg.state_dir = Some(PathBuf::from(value("--state-dir")?)),
            "--config-dir" => cfg.config_dir = Some(PathBuf::from(value("--config-dir")?)),
            "--lock-dir" => cfg.lock_dir = Some(PathBuf::from(value("--lock-dir")?)),
            "--cache-dir" => cfg.cache_dir = Some(PathBuf::from(value("--cache-dir")?)),
            "--machine" => cfg.machine = Some(value("--machine")?),
            "--dry-run" if sub == "install" => dry_run = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => {
                return Err(DaemonError::Usage(format!(
                    "unknown argument {other:?}\n{USAGE}"
                )));
            }
        }
    }
    Ok(match sub {
        "install" => Command::Install { args: cfg, dry_run },
        "policy-sample" => Command::PolicySample,
        _ => Command::Serve(cfg),
    })
}

/// True when `exe` is the daemon binary itself (`loadtabled`), which takes `serve` directly. Any
/// other executable is the CLI (`loadtable`), whose daemon lives under `loadtable daemon`.
fn is_daemon_binary(exe: &str) -> bool {
    std::path::Path::new(exe)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(SERVICE_NAME) || n.starts_with(OLD_SERVICE_NAME))
}

/// The `loadtabled` that sits next to `exe` and can run, if any (R-681). The CLI is built without
/// the peer transport unless asked, so a service should prefer the daemon binary beside it.
fn sibling_daemon(exe: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let candidate = exe.parent()?.join(SERVICE_NAME);
    let meta = std::fs::metadata(&candidate).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .then(|| candidate.display().to_string())
}

/// What `install` should warn about (R-681): a unit that runs the CLI's embedded daemon in a
/// build without the peer transport can never reach a peer.
fn install_warning(unit: &ServiceUnit, has_transport: bool) -> Option<String> {
    let runs_cli = unit.argv.first().is_some_and(|e| !is_daemon_binary(e));
    (runs_cli && !has_transport).then(|| {
        "warning: this service cannot reach peers; install loadtabled with --features iroh \
         and run `loadtabled install`"
            .to_string()
    })
}

/// The service unit for this executable and these directories. Only directories the user
/// passed explicitly are baked in; the rest resolve from the service's own environment.
///
/// The unit starts `loadtabled serve ...` when `exe` is the daemon binary or when the CLI has a
/// `loadtabled` beside it (`sibling`), and `loadtable daemon serve ...` otherwise, so the installed
/// unit works whichever of the two ran `install`.
pub fn service_unit(exe: &str, sibling: Option<&str>, args: &ConfigArgs) -> ServiceUnit {
    let exe = match sibling {
        Some(d) if !is_daemon_binary(exe) => d,
        _ => exe,
    };
    let mut argv = vec![exe.to_string()];
    if !is_daemon_binary(exe) {
        argv.push("daemon".to_string());
    }
    argv.push("serve".to_string());
    for (flag, v) in [
        ("--state-dir", &args.state_dir),
        ("--config-dir", &args.config_dir),
        ("--lock-dir", &args.lock_dir),
        ("--cache-dir", &args.cache_dir),
    ] {
        if let Some(p) = v {
            argv.push(flag.to_string());
            argv.push(p.display().to_string());
        }
    }
    if let Some(m) = &args.machine {
        argv.push("--machine".to_string());
        argv.push(m.clone());
    }
    ServiceUnit {
        name: SERVICE_NAME.to_string(),
        argv,
    }
}

/// The command that installs `unit`: its invocation with `install` in place of `serve`.
fn install_command(unit: &ServiceUnit) -> String {
    let mut words: Vec<String> = unit
        .argv
        .iter()
        .take_while(|a| a.as_str() != "serve")
        .map(|a| {
            std::path::Path::new(a)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(a)
                .to_string()
        })
        .collect();
    words.push("install".to_string());
    words.join(" ")
}

/// The unit text for this operating system.
pub fn render_unit(unit: &ServiceUnit) -> String {
    if cfg!(target_os = "macos") {
        tasks_platform::render_launchd_plist(unit)
    } else {
        tasks_platform::render_systemd_unit(unit)
    }
}

/// The install plan as text. With `service` it writes the user unit file (never overwriting a
/// different one) and reports the command to run next; without it, it only prints the unit.
/// Nothing is started and nothing privileged is run.
pub fn install_plan(
    unit: &ServiceUnit,
    service: Option<&dyn Service>,
) -> Result<String, DaemonError> {
    let mut out = format!(
        "service {} would run: {}\n\n",
        unit.name,
        unit.argv.join(" ")
    );
    out.push_str(&render_unit(unit));
    match service {
        None => out.push_str(&format!(
            "\ndry run: nothing written. Run `{}` to write the unit.\n",
            install_command(unit)
        )),
        Some(s) => {
            let plan = s.install(unit)?;
            let state = if plan.contents_written {
                "written"
            } else {
                "already installed, unchanged"
            };
            out.push_str(&format!(
                "\nunit {state}: {}\nnext, run:\n  {}\n",
                plan.path.display(),
                plan.next_command
            ));
        }
    }
    Ok(out)
}

/// Runs a parsed command. `serve` blocks until a shutdown request is accepted.
pub fn run(args: &[String]) -> Result<(), DaemonError> {
    match parse_args(args)? {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::PolicySample => {
            print!("{}", MachinePolicy::sample_text());
            Ok(())
        }
        Command::Install { args, dry_run } => {
            let exe = std::env::current_exe()
                .map_err(|e| DaemonError::io(std::path::Path::new("current_exe"), e))?;
            let sibling = sibling_daemon(&exe);
            let unit = service_unit(&exe.display().to_string(), sibling.as_deref(), &args);
            if let Some(w) = install_warning(&unit, cfg!(feature = "iroh")) {
                eprintln!("loadtabled: {w}");
            }
            let native = tasks_platform::native();
            let text = install_plan(&unit, if dry_run { None } else { Some(&native) })?;
            print!("{text}");
            Ok(())
        }
        Command::Serve(args) => {
            let env = |k: &str| tasks_core::names::env_or_old(&|v| std::env::var(v).ok(), k);
            // a tasks2 install moves to the LoadTable names before anything opens it (R-804)
            let mut moved = crate::rename::move_default_dirs(&args, &env)?;
            let config = DaemonConfig::resolve(&args, &env)?;
            moved.extend(crate::rename::move_cache_outputs(&config.cache_dir)?);
            for note in &moved {
                crate::daemon_log::say(format_args!("rename: {note}"));
            }
            #[allow(unused_mut)]
            let mut opts = DaemonOptions::new(config);
            #[cfg(feature = "iroh")]
            {
                opts.peer_runtime = Some(Box::new(crate::peer_link::iroh_host::IrohFactory));
            }
            #[cfg(not(feature = "iroh"))]
            crate::daemon_log::say(
                "this build has no peer transport (build with --features iroh); \
                 identity and pairing tickets work, but every job runs here",
            );
            // registered before the socket is listening: a SIGTERM that arrives as soon as the
            // daemon answers is seen, not handled by the default action (R-685)
            let signals = termination_flags();
            let daemon = Daemon::start(opts)?;
            for note in &daemon.startup.notes {
                crate::daemon_log::say(format_args!("startup: {note}"));
            }
            if !daemon.startup.interrupted.is_empty() {
                crate::daemon_log::say(format_args!(
                    "startup: {} job(s) left running by a previous daemon were marked interrupted and not re-run",
                    daemon.startup.interrupted.len()
                ));
            }
            watch_termination(daemon.scheduler().clone(), signals);
            daemon.run()
        }
    }
}

/// What a termination signal does to a daemon (R-685).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnSignal {
    /// Nothing was received.
    Keep,
    /// Idle: leave the serve loop, clean up and exit.
    Shutdown,
    /// Jobs run: die as an unhandled signal would, leaving them to be marked interrupted by the
    /// next daemon. A clean stop never abandons running jobs behind the daemon's back.
    Die,
}

fn on_signal(pending: Option<i32>, running_jobs: usize) -> OnSignal {
    match (pending, running_jobs) {
        (None, _) => OnSignal::Keep,
        (Some(_), 0) => OnSignal::Shutdown,
        (Some(_), _) => OnSignal::Die,
    }
}

/// The termination signals and whether each arrived.
type SignalFlags = Vec<(i32, std::sync::Arc<std::sync::atomic::AtomicBool>)>;

/// Catches SIGTERM and SIGINT from now on; [`watch_termination`] acts on them.
fn termination_flags() -> SignalFlags {
    use signal_hook::consts::{SIGINT, SIGTERM};
    [SIGTERM, SIGINT]
        .into_iter()
        .filter_map(|sig| {
            let f = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            signal_hook::flag::register(sig, f.clone()).ok()?;
            Some((sig, f))
        })
        .collect()
}

/// Makes SIGTERM and SIGINT stop an idle daemon cleanly. Without this the CLI's embedded daemon
/// inherited the CLI's flag-only handlers and ignored SIGTERM (R-685); `loadtabled` had no
/// handler and died with its socket file left behind.
fn watch_termination(sch: std::sync::Arc<crate::scheduler::Scheduler>, flags: SignalFlags) {
    use std::sync::atomic::Ordering;
    let _ = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            while !sch.is_shutdown() {
                let pending = flags
                    .iter()
                    .find(|(_, f)| f.load(Ordering::SeqCst))
                    .map(|(s, _)| *s);
                match on_signal(pending, sch.running_count()) {
                    OnSignal::Keep => {}
                    OnSignal::Shutdown => {
                        if sch.shutdown_if_idle() {
                            return;
                        }
                        // a job started between the check and the request: look again
                    }
                    OnSignal::Die => {
                        if let Some(sig) = pending {
                            let _ = signal_hook::low_level::emulate_default_handler(sig);
                        }
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
}

/// `main` for the `loadtabled` binary.
pub fn main_from_env() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            crate::daemon_log::say(&e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_platform::fake::FakePlatform;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_parse_and_unknown_ones_are_refused() {
        assert_eq!(parse_args(&a(&["--help"])).unwrap(), Command::Help);
        let Command::Serve(c) =
            parse_args(&a(&["serve", "--state-dir", "/s", "--machine", "m"])).unwrap()
        else {
            panic!("serve expected");
        };
        assert_eq!(c.state_dir, Some(PathBuf::from("/s")));
        assert_eq!(c.machine.as_deref(), Some("m"));
        let Command::Serve(c) = parse_args(&a(&["--state-dir", "/s"])).unwrap() else {
            panic!("bare flags mean serve");
        };
        assert_eq!(c.state_dir, Some(PathBuf::from("/s")));
        assert!(matches!(
            parse_args(&a(&["install", "--dry-run"])).unwrap(),
            Command::Install { dry_run: true, .. }
        ));
        assert!(parse_args(&a(&["serve", "--dry-run"])).is_err());
        assert!(parse_args(&a(&["serve", "--state-dir"])).is_err());
        assert!(parse_args(&a(&["serve", "--bogus"])).is_err());
        assert_eq!(
            parse_args(&a(&["policy-sample"])).unwrap(),
            Command::PolicySample
        );
    }

    #[test]
    fn r685_a_signal_stops_an_idle_daemon_and_never_abandons_running_jobs() {
        assert_eq!(on_signal(None, 0), OnSignal::Keep);
        assert_eq!(on_signal(None, 3), OnSignal::Keep);
        assert_eq!(on_signal(Some(15), 0), OnSignal::Shutdown);
        assert_eq!(on_signal(Some(2), 0), OnSignal::Shutdown);
        assert_eq!(on_signal(Some(15), 2), OnSignal::Die);
    }

    #[test]
    fn r681_the_cli_installs_the_daemon_binary_beside_it() {
        let args = ConfigArgs::default();
        let beside = service_unit(
            "/home/u/.local/bin/loadtable",
            Some("/home/u/.local/bin/loadtabled"),
            &args,
        );
        assert_eq!(beside.argv, ["/home/u/.local/bin/loadtabled", "serve"]);
        // without one beside it the unit keeps the embedded daemon
        let alone = service_unit("/home/u/.local/bin/loadtable", None, &args);
        assert_eq!(
            alone.argv,
            ["/home/u/.local/bin/loadtable", "daemon", "serve"]
        );
        // the daemon binary ignores any sibling
        let d = service_unit("/a/loadtabled", Some("/b/loadtabled"), &args);
        assert_eq!(d.argv, ["/a/loadtabled", "serve"]);
    }

    #[test]
    fn r681_a_unit_that_cannot_reach_peers_is_warned_about() {
        let args = ConfigArgs::default();
        let cli = service_unit("/bin/loadtable", None, &args);
        let d = service_unit("/bin/loadtabled", None, &args);
        assert!(
            install_warning(&cli, false)
                .unwrap()
                .contains("cannot reach peers")
        );
        assert!(install_warning(&cli, true).is_none());
        // loadtabled without the transport warns at its own startup, not here
        assert!(install_warning(&d, false).is_none());
    }

    #[test]
    fn r681_the_sibling_must_exist_and_be_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("r681-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("loadtable");
        assert_eq!(sibling_daemon(&exe), None);
        let d = dir.join(SERVICE_NAME);
        std::fs::write(&d, "x").unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(sibling_daemon(&exe), None);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sibling_daemon(&exe), Some(d.display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_unit_starts_the_daemon_whichever_binary_ran_install() {
        let args = ConfigArgs {
            state_dir: Some("/s".into()),
            machine: Some("m".into()),
            ..ConfigArgs::default()
        };
        // the CLI binary: the daemon is its `daemon` subcommand
        let cli = service_unit("/home/u/.local/bin/loadtable", None, &args);
        assert_eq!(
            cli.argv,
            [
                "/home/u/.local/bin/loadtable",
                "daemon",
                "serve",
                "--state-dir",
                "/s",
                "--machine",
                "m"
            ]
        );
        // the daemon binary takes `serve` itself
        let d = service_unit("/usr/bin/loadtabled", None, &args);
        assert_eq!(&d.argv[..2], ["/usr/bin/loadtabled", "serve"]);
        // the arguments of the unit parse as the daemon's own command line
        let after_daemon: Vec<String> = cli.argv[2..].to_vec();
        assert!(matches!(
            parse_args(&after_daemon).unwrap(),
            Command::Serve(c) if c.machine.as_deref() == Some("m")
        ));
        // the dry-run hint names the command that was actually run
        assert!(
            install_plan(&cli, None)
                .unwrap()
                .contains("Run `loadtable daemon install`")
        );
        assert!(
            install_plan(&d, None)
                .unwrap()
                .contains("Run `loadtabled install`")
        );
    }

    #[test]
    fn install_plan_prints_the_unit_and_the_next_command_without_running_anything() {
        let args = ConfigArgs {
            state_dir: Some("/var/lib/loadtable".into()),
            ..ConfigArgs::default()
        };
        let unit = service_unit("/usr/local/bin/loadtabled", None, &args);
        assert_eq!(
            unit.argv,
            [
                "/usr/local/bin/loadtabled",
                "serve",
                "--state-dir",
                "/var/lib/loadtable"
            ]
        );
        let dry = install_plan(&unit, None).unwrap();
        assert!(dry.contains("dry run"));
        assert!(dry.contains("ExecStart=") || dry.contains("<key>ProgramArguments</key>"));
        let fake = FakePlatform::default();
        let real = install_plan(&unit, Some(&fake)).unwrap();
        assert!(
            real.contains("next, run:\n  fake-start loadtabled"),
            "{real}"
        );
        assert!(real.contains("/fake/loadtabled.service"));
    }
}
