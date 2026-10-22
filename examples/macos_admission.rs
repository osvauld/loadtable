//! Read-only candidate resource probe. Never starts a daemon or acquires a job lease.
#[cfg(target_os = "macos")]
fn main() -> tinline_task_manager::Result<()> {
    use std::path::PathBuf;
    use tinline_task_manager::admission::{Thresholds, macos::Sample};
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: macos_admission ABSOLUTE_CACHE_PATH ABSOLUTE_STATE_PATH (read-only; no execution)".into());
    }
    let sample = Sample::read(&PathBuf::from(&args[0]), &PathBuf::from(&args[1]))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&sample.evaluate(&Thresholds::from_env()?)?)?
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_admission is a read-only native macOS probe, not a Linux PSI replacement");
    std::process::exit(2);
}
