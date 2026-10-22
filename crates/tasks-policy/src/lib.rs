//! Repo run policy: a checked-in `.tasks/run-policy.toml` that says how to build and test a repo.
//!
//! Agents read it and run `tasks2 suite run NAME`. After a few runs, [`suggest`] turns measured
//! history into a proposed diff with evidence. tasks never writes the policy file.
//!
//! Layers: [`model`] and [`validate`] are pure; [`load`] is the thin I/O layer behind
//! [`PolicyFs`]; [`expand`] makes a [`tasks_core::Plan`]; [`suggest`], [`diff`] and [`show`] are
//! pure functions over a policy and a [`tasks_timing::History`].

pub mod diff;
pub mod error;
pub mod expand;
pub mod load;
pub mod model;
pub mod show;
pub mod submit_rules;
pub mod suggest;
pub mod timing;
pub mod validate;

pub use diff::{render_diff, render_proposal};
pub use error::{PolicyError, Problem, Violation, Violations};
pub use expand::{Executables, ExpandOptions, Expansion, expand};
pub use load::{Loaded, POLICY_PATH, PolicyFs, RealFs, find_policy, load};
pub use model::{Policy, SCHEMA_VERSION, SplitMode, StepDef, Suite, TargetNote};
pub use show::show;
pub use suggest::{Action, SuggestConfig, Suggestion, SuggestionKind, suggest};
pub use tasks_core::StepHints;
pub use timing::{Candidates, Timing, candidate_key};
pub use validate::{MAX_TIMEOUT_SECS, parse, validate};
