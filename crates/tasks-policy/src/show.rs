//! `tasks2 policy show`: a readable rendering of the effective policy.

use crate::error::PolicyError;
use crate::model::{Policy, SplitMode};
use crate::suggest::describe;
use crate::timing::{Candidates, Timing, candidate_key, measure, primary, recipe_of};
use std::fmt::Write;
use std::path::Path;

/// Renders the policy; with `timing`, adds known durations for steps and noted/known targets.
pub fn show(
    policy: &Policy,
    file: &Path,
    timing: Option<&Timing<'_>>,
    candidates: &Candidates,
) -> Result<String, PolicyError> {
    let mut out = format!(
        "run policy: {} (version {})\n",
        file.display(),
        policy.version
    );
    for (name, suite) in &policy.suites {
        let _ = writeln!(out, "\nsuite {name}");
        if let Some(n) = &suite.notes {
            for l in n.lines() {
                let _ = writeln!(out, "  note: {l}");
            }
        }
        if !suite.tools.is_empty() {
            let _ = writeln!(out, "  tools: {}", suite.tools.join(", "));
        }
        for s in &suite.steps {
            let _ = writeln!(out, "  step {}: {}", s.name, s.command.join(" "));
            let mut attrs = Vec::new();
            if !s.after.is_empty() {
                attrs.push(format!("after {}", s.after.join(", ")));
            }
            if s.split == Some(SplitMode::CargoTest) {
                attrs.push("split per cargo test target".into());
            }
            if let Some(p) = &s.placement {
                attrs.push(format!("placement {p}"));
            }
            if let Some(t) = s.timeout_secs {
                attrs.push(format!("timeout {t}s"));
            }
            if s.skip_in_quick {
                attrs.push("skipped in quick runs".into());
            }
            if let Some(p) = s.priority.filter(|p| !p.is_normal()) {
                attrs.push(format!("priority {p}"));
            }
            if !attrs.is_empty() {
                let _ = writeln!(out, "    {}", attrs.join("; "));
            }
            if let (Some(t), Some((recipe, _))) = (timing, recipe_of(&s.command)) {
                match primary(measure(t, &recipe, None)?) {
                    Some(e) => {
                        let _ = writeln!(out, "    known: {}", describe(&e));
                    }
                    None => {
                        let _ = writeln!(out, "    known: no history");
                    }
                }
                let mut labels: Vec<String> = suite.targets.keys().cloned().collect();
                if let Some(c) = candidates.get(&candidate_key(name, &s.name)) {
                    labels.extend(c.iter().cloned());
                }
                labels.sort();
                labels.dedup();
                if s.split.is_some() {
                    for l in labels {
                        if let Some(e) = primary(measure(t, &recipe, Some(&l))?) {
                            let _ = writeln!(out, "    target {l}: {}", describe(&e));
                        }
                    }
                }
            }
        }
        for (label, n) in &suite.targets {
            let mut flags = Vec::new();
            if n.long {
                flags.push("long");
            }
            if n.isolate {
                flags.push("isolate (runs alone)");
            }
            let note = n.note.as_deref().unwrap_or("");
            let _ = writeln!(out, "  target {label}: {} {note}", flags.join(", "));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::fixture::history_4254;
    use crate::validate::parse;

    const TEXT: &str = r#"version = 1
[suites.core]
notes = "Run before pushing."
[suites.core.targets."test:lifecycle"]
long = true
isolate = true
[[suites.core.steps]]
name = "tests"
command = ["cargo", "test", "-p", "p2pcore", "--features", "dev-tools"]
split = "cargo-test"
placement = "nixos"
"#;

    #[test]
    fn shows_structure_without_history() {
        let p = parse(TEXT, Path::new("p.toml")).unwrap();
        let s = show(&p, Path::new("p.toml"), None, &Candidates::new()).unwrap();
        assert!(s.contains("suite core") && s.contains("note: Run before pushing."));
        assert!(s.contains("step tests: cargo test -p p2pcore") && s.contains("placement nixos"));
        assert!(s.contains("target test:lifecycle: long, isolate"));
        assert!(!s.contains("known:"));
    }

    #[test]
    fn shows_durations_with_history() {
        let p = parse(TEXT, Path::new("p.toml")).unwrap();
        let h = history_4254();
        let m = vec!["nixos".to_string()];
        let s = show(
            &p,
            Path::new("p.toml"),
            Some(&Timing {
                history: &h,
                machines: &m,
            }),
            &Candidates::new(),
        )
        .unwrap();
        assert!(s.contains("known: no history"));
        assert!(s.contains("target test:lifecycle: median 1m56s"));
    }
}
