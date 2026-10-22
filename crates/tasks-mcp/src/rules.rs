//! Submit rules for MCP submissions: exactly what `tasks2 run` applies (R-193), from the shared
//! `tasks_policy::submit_rules`, before any daemon request.

use std::path::Path;
use tasks_policy::submit_rules::{self, BroadTestMode, KnownTargets, PlacementClass, SubmitRules};

/// Test targets of the package a `cargo test` names, counted from the layout on disk. Only
/// used to word the refusal; `None` when the package cannot be found.
pub fn scan_known_targets(cwd: &Path, packages: &[String]) -> Option<KnownTargets> {
    let mut dirs = vec![];
    match packages {
        [] => dirs.push(cwd.to_path_buf()),
        [p] => {
            dirs.push(cwd.join("crates").join(p));
            dirs.push(cwd.join(p));
        }
        _ => return None,
    }
    let dir = dirs.into_iter().find(|d| d.join("Cargo.toml").is_file())?;
    let names = |sub: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.join(sub))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs") && p.is_file() {
                    p.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else if p.join("main.rs").is_file() {
                    p.file_name().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
        v.sort();
        v
    };
    let tests = names("tests");
    let bins = names("src/bin").len() + usize::from(dir.join("src/main.rs").is_file());
    let lib = usize::from(dir.join("src/lib.rs").is_file());
    Some(KnownTargets {
        total: lib + bins + tests.len(),
        test_names: tests,
    })
}

/// What the rules decided: warnings and notes to show with the result, or the refusal text.
pub fn check_command(
    argv: &[String],
    cwd: &Path,
    escape: bool,
    mode: BroadTestMode,
    anywhere: bool,
) -> Result<Vec<String>, String> {
    let packages = submit_rules::packages_of(argv);
    let known = scan_known_targets(cwd, &packages);
    let verdict = submit_rules::check(&SubmitRules {
        argv,
        broad_test_escape: escape,
        broad_mode: mode,
        known: known.as_ref(),
        max_cargo_jobs: None,
    });
    if !verdict.allowed() {
        return Err(verdict
            .refusals
            .iter()
            .map(|r| r.message.clone())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let mut notes = verdict.warnings.clone();
    if anywhere && let PlacementClass::StaysLocal(why) = submit_rules::placement_class(argv) {
        notes.push(format!(
            "placement anywhere has no effect here, the job stays on this machine: {why}"
        ));
    }
    Ok(notes)
}
