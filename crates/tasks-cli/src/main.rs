//! `tasks2` command line entry point.

use anyhow::{Context, Result};
use std::process::ExitCode;
use tasks_core::plan::Plan;

const USAGE: &str = "usage: tasks2 plan check FILE\n  plan check FILE   parse and validate a plan, printing the step order";

fn check_plan(path: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let plan = Plan::parse(&text)?;
    plan.validate()?;
    Ok(plan
        .order()
        .iter()
        .filter_map(|i| plan.steps.get(*i))
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}

fn run(args: &[String]) -> Result<Option<String>> {
    match args {
        [a, b, file] if a == "plan" && b == "check" => check_plan(file).map(Some),
        _ => Ok(None),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(Some(out)) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_check_orders_steps() {
        let p = std::env::temp_dir().join(format!("tasks2-plan-{}.json", std::process::id()));
        std::fs::write(
            &p,
            r#"{"steps":[{"name":"b","argv":["x"],"after":["a"]},{"name":"a","argv":["x"]}]}"#,
        )
        .unwrap();
        let out = run(&["plan".into(), "check".into(), p.display().to_string()]).unwrap();
        assert_eq!(out.as_deref(), Some("a\nb"));
        std::fs::write(&p, r#"{"steps":[]}"#).unwrap();
        assert!(run(&["plan".into(), "check".into(), p.display().to_string()]).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_args_print_usage() {
        assert_eq!(run(&[]).unwrap(), None);
    }
}
