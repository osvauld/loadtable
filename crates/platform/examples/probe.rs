//! Read-only native smoke probe; no scheduler, worker, daemon or networking.
//! cargo run -p task-manager-platform --example probe -- CACHE_PATH STATE_PATH
use std::path::Path;
use task_manager_platform::{ResourceSample, capabilities, children_usage, identity};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: probe ABSOLUTE_CACHE_PATH ABSOLUTE_STATE_PATH".into());
    }
    let sample = ResourceSample::read(Path::new(&args[0]), Path::new(&args[1]))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "capabilities": capabilities(),
            "identity": identity(std::process::id())?,
            "resources": sample,
            // This probe has no pressure sampler. Never substitute zero-valued PSI.
            "pressure": null,
            "children_usage": children_usage()?,
            "worker_execution_validated": false,
        }))?
    );
    Ok(())
}
