//! Timing knowledge for `tasks`: recipe keys, tool-output parsers, history, estimates, progress
//! and test-suite splitting. Pure: no file, clock, process or database I/O. Time arrives as
//! `u64` milliseconds and storage sits behind the [`History`] trait.

pub mod cargo;
pub mod dur;
mod error;
pub mod estimate;
pub mod event;
pub mod exe;
pub mod family;
pub mod gradle;
pub mod history;
pub mod progress;
pub mod recipe;
mod recipe_cargo;
mod recipe_gradle;
pub mod record;
mod shell;
pub mod short;
pub mod split;
pub mod timeline;
pub mod warmth;

pub use cargo::CargoParser;
pub use error::TimingError;
pub use estimate::{
    Basis, Estimate, Eta, EtaBasis, Left, MachineInfo, NoEstimate, Planning, Shown, Source,
    estimate, estimate_with_cache, planning, shown, wanted_cache,
};
pub use event::{Event, TargetKind, TaskOutcome};
pub use exe::{ExeKind, TestExe, parse_test_executables};
pub use gradle::GradleParser;
pub use history::{Cache, History, MemHistory, Outcome, Sample, SampleKey};
pub use progress::{Estimates, Progress, progress};
pub use recipe::RecipeKey;
pub use split::{label_of, order_longest_first, split_cargo_test};
pub use timeline::{Phase, PhaseKind, Timeline};
