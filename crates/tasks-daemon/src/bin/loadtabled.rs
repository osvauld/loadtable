//! The `loadtabled` binary: the daemon, its service installer and the sample policy.

fn main() -> std::process::ExitCode {
    tasks_daemon::entry::main_from_env()
}
