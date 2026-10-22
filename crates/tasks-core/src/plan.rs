//! Plans: named steps with dependencies.

use crate::{Kind, Requirements};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Maximum number of steps in a plan.
pub const MAX_STEPS: usize = 256;

/// Errors found while parsing or validating a plan.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    /// The JSON could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),
    /// No steps.
    #[error("plan has no steps")]
    Empty,
    /// More than 256 steps.
    #[error("plan has {0} steps (max 256)")]
    TooMany(usize),
    /// Two steps share a name.
    #[error("duplicate step name {0:?}")]
    DuplicateName(String),
    /// Step name has invalid characters.
    #[error("bad step name {0:?}")]
    BadName(String),
    /// A step depends on itself.
    #[error("step {0:?} depends on itself")]
    SelfDependency(String),
    /// A dependency names no step.
    #[error("step {step:?} depends on unknown step {dep:?}")]
    UnknownDependency {
        /// The step.
        step: String,
        /// The missing dependency.
        dep: String,
    },
    /// A dependency is listed twice.
    #[error("step {step:?} lists dependency {dep:?} twice")]
    DuplicateDependency {
        /// The step.
        step: String,
        /// The repeated dependency.
        dep: String,
    },
    /// The dependencies form a cycle through this step.
    #[error("dependency cycle involving step {0:?}")]
    Cycle(String),
    /// A step has no argv.
    #[error("step {0:?} has empty argv")]
    EmptyArgv(String),
    /// A step has a NUL byte in argv.
    #[error("step {0:?} contains a NUL byte")]
    Nul(String),
}

/// One named step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Unique step name.
    pub name: String,
    /// Command and arguments.
    pub argv: Vec<String>,
    /// Names of steps that must pass first.
    #[serde(default)]
    pub after: Vec<String>,
    /// Step kind.
    #[serde(default = "default_kind")]
    pub kind: Kind,
    /// Step requirements.
    #[serde(default)]
    pub requirements: Requirements,
    /// Scheduling hints, usually from the repo run policy.
    #[serde(default, skip_serializing_if = "StepHints::is_empty")]
    pub hints: StepHints,
}

/// Scheduling hints for a step. None of them is required to run it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepHints {
    /// Preferred machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    /// Timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// The step must run alone (e.g. network tests).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub isolate: bool,
    /// The step is known to be long.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub long: bool,
    /// Tools the step needs on the machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl StepHints {
    /// True when no hint is set.
    pub fn is_empty(&self) -> bool {
        *self == StepHints::default()
    }
}

fn default_kind() -> Kind {
    Kind::Compile
}

/// A set of dependent steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The steps.
    pub steps: Vec<Step>,
}

impl Plan {
    /// Parses a plan from JSON (no validation beyond the schema).
    pub fn parse(s: &str) -> Result<Plan, PlanError> {
        serde_json::from_str(s).map_err(|e| PlanError::Parse(e.to_string()))
    }

    /// Checks names, argv and the dependency graph.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.steps.is_empty() {
            return Err(PlanError::Empty);
        }
        if self.steps.len() > MAX_STEPS {
            return Err(PlanError::TooMany(self.steps.len()));
        }
        let mut names = BTreeSet::new();
        for s in &self.steps {
            if s.name.is_empty()
                || !s
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(PlanError::BadName(s.name.clone()));
            }
            if !names.insert(s.name.as_str()) {
                return Err(PlanError::DuplicateName(s.name.clone()));
            }
        }
        for s in &self.steps {
            if s.argv.is_empty() {
                return Err(PlanError::EmptyArgv(s.name.clone()));
            }
            if s.argv.iter().any(|a| a.contains('\0')) {
                return Err(PlanError::Nul(s.name.clone()));
            }
            let mut seen = BTreeSet::new();
            for d in &s.after {
                if *d == s.name {
                    return Err(PlanError::SelfDependency(s.name.clone()));
                }
                if !names.contains(d.as_str()) {
                    return Err(PlanError::UnknownDependency {
                        step: s.name.clone(),
                        dep: d.clone(),
                    });
                }
                if !seen.insert(d.as_str()) {
                    return Err(PlanError::DuplicateDependency {
                        step: s.name.clone(),
                        dep: d.clone(),
                    });
                }
            }
        }
        self.topo().map(|_| ())
    }

    /// Topological order as indices into `steps` (dependencies first); empty if invalid.
    pub fn order(&self) -> Vec<usize> {
        self.topo().unwrap_or_default()
    }

    fn topo(&self) -> Result<Vec<usize>, PlanError> {
        let index: BTreeMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();
        let mut indeg: Vec<usize> = self
            .steps
            .iter()
            .map(|s| {
                s.after
                    .iter()
                    .filter(|d| index.contains_key(d.as_str()))
                    .count()
            })
            .collect();
        let mut out = Vec::with_capacity(self.steps.len());
        let mut done = vec![false; self.steps.len()];
        while out.len() < self.steps.len() {
            let next = indeg
                .iter()
                .enumerate()
                .find(|(i, d)| **d == 0 && !done.get(*i).copied().unwrap_or(true))
                .map(|(i, _)| i);
            let Some(i) = next else {
                let stuck = self
                    .steps
                    .iter()
                    .zip(&done)
                    .find(|(_, d)| !**d)
                    .map(|(s, _)| s.name.clone())
                    .unwrap_or_default();
                return Err(PlanError::Cycle(stuck));
            };
            if let Some(d) = done.get_mut(i) {
                *d = true;
            }
            out.push(i);
            if let Some(name) = self.steps.get(i).map(|s| s.name.as_str()) {
                for (j, s) in self.steps.iter().enumerate() {
                    if s.after.iter().any(|d| d == name)
                        && let Some(x) = indeg.get_mut(j)
                    {
                        *x = x.saturating_sub(1);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, after: &[&str]) -> Step {
        Step {
            name: name.into(),
            argv: vec!["true".into()],
            after: after.iter().map(|s| s.to_string()).collect(),
            kind: Kind::Compile,
            requirements: Requirements::default(),
            hints: StepHints::default(),
        }
    }
    fn plan(steps: Vec<Step>) -> Plan {
        Plan { steps }
    }

    #[test]
    fn parse_defaults() {
        let p = Plan::parse(r#"{"steps":[{"name":"a","argv":["x"]}]}"#).unwrap();
        assert_eq!(p.steps[0].kind, Kind::Compile);
        assert_eq!(p.steps[0].requirements, Requirements::default());
        assert!(p.steps[0].after.is_empty());
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        assert!(matches!(
            Plan::parse(r#"{"steps":[],"x":1}"#),
            Err(PlanError::Parse(_))
        ));
        assert!(matches!(
            Plan::parse(r#"{"steps":[{"name":"a","argv":["x"],"bogus":1}]}"#),
            Err(PlanError::Parse(_))
        ));
    }

    #[test]
    fn empty_and_too_many() {
        assert_eq!(plan(vec![]).validate(), Err(PlanError::Empty));
        let many = (0..257).map(|i| step(&format!("s{i}"), &[])).collect();
        assert_eq!(plan(many).validate(), Err(PlanError::TooMany(257)));
    }

    #[test]
    fn names() {
        assert_eq!(
            plan(vec![step("a", &[]), step("a", &[])]).validate(),
            Err(PlanError::DuplicateName("a".into()))
        );
        assert_eq!(
            plan(vec![step("a b", &[])]).validate(),
            Err(PlanError::BadName("a b".into()))
        );
        assert_eq!(
            plan(vec![step("", &[])]).validate(),
            Err(PlanError::BadName("".into()))
        );
    }

    #[test]
    fn dependencies() {
        assert_eq!(
            plan(vec![step("a", &["a"])]).validate(),
            Err(PlanError::SelfDependency("a".into()))
        );
        assert_eq!(
            plan(vec![step("a", &["z"])]).validate(),
            Err(PlanError::UnknownDependency {
                step: "a".into(),
                dep: "z".into()
            })
        );
        assert_eq!(
            plan(vec![step("a", &[]), step("b", &["a", "a"])]).validate(),
            Err(PlanError::DuplicateDependency {
                step: "b".into(),
                dep: "a".into()
            })
        );
    }

    #[test]
    fn cycle() {
        let p = plan(vec![step("a", &["b"]), step("b", &["a"])]);
        assert!(matches!(p.validate(), Err(PlanError::Cycle(_))));
        assert!(p.order().is_empty());
    }

    #[test]
    fn argv_rules() {
        let mut s = step("a", &[]);
        s.argv.clear();
        assert_eq!(
            plan(vec![s]).validate(),
            Err(PlanError::EmptyArgv("a".into()))
        );
        let mut s = step("a", &[]);
        s.argv = vec!["a\0".into()];
        assert_eq!(plan(vec![s]).validate(), Err(PlanError::Nul("a".into())));
    }

    #[test]
    fn order_respects_dependencies() {
        let p = plan(vec![
            step("c", &["b"]),
            step("b", &["a"]),
            step("a", &[]),
            step("d", &[]),
        ]);
        assert_eq!(p.validate(), Ok(()));
        let o = p.order();
        assert_eq!(o.len(), 4);
        let pos = |n: &str| {
            let i = p.steps.iter().position(|s| s.name == n).unwrap();
            o.iter().position(|x| *x == i).unwrap()
        };
        assert!(pos("a") < pos("b") && pos("b") < pos("c"));
    }
}
