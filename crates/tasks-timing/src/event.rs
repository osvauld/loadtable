//! Events emitted by the output parsers.

/// Which kind of cargo test target is running.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// Unit tests of a library.
    Lib,
    /// An integration test (`tests/NAME.rs`).
    Test(String),
    /// Unit tests of a binary, bench or example.
    Bin,
    /// Documentation tests.
    Doc,
}

impl TargetKind {
    /// Stable label used as the history target key, e.g. `test:lifecycle` or `lib:p2pcore`.
    pub fn label(&self, name: &str) -> String {
        match self {
            TargetKind::Lib => format!("lib:{name}"),
            TargetKind::Test(t) => format!("test:{t}"),
            TargetKind::Bin => format!("bin:{name}"),
            TargetKind::Doc => format!("doc:{name}"),
        }
    }
}

/// Outcome of one gradle task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskOutcome {
    /// The task ran.
    Executed,
    /// Outputs were current.
    UpToDate,
    /// Outputs came from the build cache.
    FromCache,
    /// The task failed.
    Failed,
    /// Skipped, including `NO-SOURCE`.
    Skipped,
}

/// One thing noticed in tool output.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The first `Compiling` line was seen.
    CompileStarted,
    /// One crate was compiled or checked (every `Compiling`/`Checking` line, R-800).
    CrateBuilt {
        /// A dependency (registry or git), not a crate of the checkout.
        dependency: bool,
    },
    /// cargo printed `Finished`.
    CompileFinished {
        /// Profile name, e.g. `release` or `test`.
        profile: String,
    },
    /// Compilation failed (`error: could not compile`).
    CompileFailed,
    /// A test target started running.
    TargetStarted {
        /// Target or crate name.
        name: String,
        /// Target kind.
        kind: TargetKind,
    },
    /// A test target printed its `test result:` line.
    TargetFinished {
        /// Target or crate name.
        name: String,
        /// Tests passed.
        passed: u32,
        /// Tests failed.
        failed: u32,
        /// Tests ignored.
        ignored: u32,
        /// `finished in Xs` as printed by the harness.
        reported_secs: Option<f64>,
    },
    /// One test failed.
    TestFailed {
        /// Target (cargo) or task path (gradle) it ran in.
        target: String,
        /// Test name.
        test: String,
    },
    /// A gradle task line.
    GradleTask {
        /// Task path, e.g. `:app:compileDebugKotlin`.
        path: String,
        /// Its outcome.
        outcome: TaskOutcome,
    },
    /// A test thread panicked.
    Panic {
        /// Target it ran in.
        target: String,
        /// Panic message.
        message: String,
    },
    /// gradle printed `BUILD SUCCESSFUL/FAILED in ...`.
    BuildFinished {
        /// Whether the build succeeded.
        success: bool,
        /// `in 4m 3s` as printed.
        reported_secs: Option<f64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable() {
        assert_eq!(TargetKind::Lib.label("p2pcore"), "lib:p2pcore");
        assert_eq!(
            TargetKind::Test("calls".into()).label("calls"),
            "test:calls"
        );
        assert_eq!(TargetKind::Doc.label("x"), "doc:x");
        assert_eq!(TargetKind::Bin.label("b"), "bin:b");
    }
}
