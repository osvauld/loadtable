//! Standalone onboarding, NOT a worker. No daemon or peer endpoint is started.
fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(feature = "ui")]
    if args.is_empty() {
        args.push("--ui".into());
    }
    #[cfg(not(feature = "ui"))]
    let _ = &mut args;
    if let Err(error) = task_manager_setup::cli::run(&args) {
        eprintln!("tasks-mac-setup: {error}");
        std::process::exit(2);
    }
}
