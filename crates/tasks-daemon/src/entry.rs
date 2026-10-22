//! Command line of the daemon: `tasks2d serve`, `tasks2d install`, `tasks2d policy-sample`.
//!
//! The binary `tasks2d` is a thin wrapper over [`main_from_env`]; the CLI crate can call
//! [`run`] with the arguments after `tasks2 daemon`.

use crate::config::{ConfigArgs, DaemonConfig};
use crate::error::DaemonError;
use crate::machine_policy::MachinePolicy;
use crate::server::{Daemon, DaemonOptions};
use std::path::PathBuf;
use std::process::ExitCode;
use tasks_platform::{Service, ServiceUnit};

/// The service name.
pub const SERVICE_NAME: &str = "tasks2d";

const USAGE: &str = "usage: tasks2d [serve] [OPTIONS]      run the daemon (also: tasks2 daemon serve)\n\
       tasks2d install [--dry-run] [OPTIONS]  write the user service unit and print the next command\n\
       tasks2d policy-sample                  print a machine-policy.toml listing every default\n\
\n\
OPTIONS\n\
  --state-dir DIR   ledger, socket, logs (default: $XDG_STATE_HOME/tasks2)\n\
  --config-dir DIR  holds machine-policy.toml (default: $XDG_CONFIG_HOME/tasks2)\n\
  --lock-dir DIR    slot lock files (default: STATE_DIR/slots; never TMPDIR)\n\
  --cache-dir DIR   where cache output lands (checked against the disk reserve)\n\
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

/// True when `exe` is the daemon binary itself (`tasks2d`), which takes `serve` directly. Any
/// other executable is the CLI (`tasks2`), whose daemon lives under `tasks2 daemon`.
fn is_daemon_binary(exe: &str) -> bool {
    std::path::Path::new(exe)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(SERVICE_NAME))
}

/// The service unit for this executable and these directories. Only directories the user
/// passed explicitly are baked in; the rest resolve from the service's own environment.
///
/// The unit starts `tasks2d serve ...` when `exe` is the daemon binary and
/// `tasks2 daemon serve ...` when it is the CLI, so the installed unit works whichever of the
/// two ran `install`.
pub fn service_unit(exe: &str, args: &ConfigArgs) -> ServiceUnit {
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
            let unit = service_unit(&exe.display().to_string(), &args);
            let native = tasks_platform::native();
            let text = install_plan(&unit, if dry_run { None } else { Some(&native) })?;
            print!("{text}");
            Ok(())
        }
        Command::Serve(args) => {
            let env = |k: &str| std::env::var(k).ok();
            let config = DaemonConfig::resolve(&args, &env)?;
            #[allow(unused_mut)]
            let mut opts = DaemonOptions::new(config);
            #[cfg(feature = "iroh")]
            {
                opts.peer_runtime = Some(Box::new(crate::peer_link::iroh_host::IrohFactory));
            }
            #[cfg(not(feature = "iroh"))]
            eprintln!(
                "tasks2d: this build has no peer transport (build with --features iroh); \
                 identity and pairing tickets work, but every job runs here"
            );
            let daemon = Daemon::start(opts)?;
            for note in &daemon.startup.notes {
                eprintln!("tasks2d: startup: {note}");
            }
            if !daemon.startup.interrupted.is_empty() {
                eprintln!(
                    "tasks2d: startup: {} job(s) left running by a previous daemon were marked interrupted and not re-run",
                    daemon.startup.interrupted.len()
                );
            }
            daemon.run()
        }
    }
}

/// `main` for the `tasks2d` binary.
pub fn main_from_env() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tasks2d: {e}");
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
    fn the_unit_starts_the_daemon_whichever_binary_ran_install() {
        let args = ConfigArgs {
            state_dir: Some("/s".into()),
            machine: Some("m".into()),
            ..ConfigArgs::default()
        };
        // the CLI binary: the daemon is its `daemon` subcommand
        let cli = service_unit("/home/u/.local/bin/tasks2", &args);
        assert_eq!(
            cli.argv,
            [
                "/home/u/.local/bin/tasks2",
                "daemon",
                "serve",
                "--state-dir",
                "/s",
                "--machine",
                "m"
            ]
        );
        // the daemon binary takes `serve` itself
        let d = service_unit("/usr/bin/tasks2d", &args);
        assert_eq!(&d.argv[..2], ["/usr/bin/tasks2d", "serve"]);
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
                .contains("Run `tasks2 daemon install`")
        );
        assert!(
            install_plan(&d, None)
                .unwrap()
                .contains("Run `tasks2d install`")
        );
    }

    #[test]
    fn install_plan_prints_the_unit_and_the_next_command_without_running_anything() {
        let args = ConfigArgs {
            state_dir: Some("/var/lib/tasks2".into()),
            ..ConfigArgs::default()
        };
        let unit = service_unit("/usr/local/bin/tasks2d", &args);
        assert_eq!(
            unit.argv,
            [
                "/usr/local/bin/tasks2d",
                "serve",
                "--state-dir",
                "/var/lib/tasks2"
            ]
        );
        let dry = install_plan(&unit, None).unwrap();
        assert!(dry.contains("dry run"));
        assert!(dry.contains("ExecStart=") || dry.contains("<key>ProgramArguments</key>"));
        let fake = FakePlatform::default();
        let real = install_plan(&unit, Some(&fake)).unwrap();
        assert!(real.contains("next, run:\n  fake-start tasks2d"), "{real}");
        assert!(real.contains("/fake/tasks2d.service"));
    }
}
