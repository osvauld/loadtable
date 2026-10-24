//! Test executables listed by `cargo test --no-run --message-format=json`.

use crate::error::TimingError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What kind of cargo target a test executable belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ExeKind {
    /// Unit tests of a library.
    Lib,
    /// An integration test.
    Test,
    /// Unit tests of a binary.
    Bin,
    /// A bench target built for testing.
    Bench,
    /// An example built for testing.
    Example,
}

/// One test executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestExe {
    /// Package name.
    pub package: String,
    /// Target name (crate name for a lib).
    pub name: String,
    /// Target kind.
    pub kind: ExeKind,
    /// Path of the built executable; empty when the target came from `cargo metadata`.
    #[serde(default)]
    pub path: String,
}

impl TestExe {
    /// History label matching the output parser's, e.g. `test:lifecycle` or `lib:p2pcore`.
    pub fn label(&self) -> String {
        let k = match self.kind {
            ExeKind::Lib => "lib",
            ExeKind::Test => "test",
            ExeKind::Bin => "bin",
            ExeKind::Bench => "bench",
            ExeKind::Example => "example",
        };
        format!("{k}:{}", self.name)
    }
}

/// Package name from a `package_id` in either cargo format.
fn package_name(id: &str) -> String {
    if let Some((head, tail)) = id.split_once('#') {
        return match tail.split_once('@') {
            Some((name, _)) => name.to_string(),
            None => head.rsplit('/').next().unwrap_or(head).to_string(),
        };
    }
    id.split_whitespace().next().unwrap_or(id).to_string()
}

fn kind_of(kinds: &[Value]) -> ExeKind {
    let has = |k: &str| kinds.iter().any(|v| v.as_str() == Some(k));
    if has("test") {
        ExeKind::Test
    } else if has("bin") {
        ExeKind::Bin
    } else if has("bench") {
        ExeKind::Bench
    } else if has("example") {
        ExeKind::Example
    } else {
        ExeKind::Lib
    }
}

/// Extracts test executables from the JSON lines of `cargo test --no-run --message-format=json`.
///
/// Lines that are not JSON objects (stray text) are skipped; malformed JSON objects are errors.
pub fn parse_test_executables(json_lines: &str) -> Result<Vec<TestExe>, TimingError> {
    let mut out: Vec<TestExe> = Vec::new();
    for (i, line) in json_lines.lines().enumerate() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| TimingError::BadJson {
            line: i + 1,
            message: e.to_string(),
        })?;
        if v.get("reason").and_then(Value::as_str) != Some("compiler-artifact")
            || v.pointer("/profile/test").and_then(Value::as_bool) != Some(true)
        {
            continue;
        }
        let (Some(path), Some(name)) = (
            v.get("executable").and_then(Value::as_str),
            v.pointer("/target/name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if out.iter().any(|e| e.path == path) {
            continue;
        }
        let kinds = v
            .pointer("/target/kind")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        out.push(TestExe {
            package: package_name(v.get("package_id").and_then(Value::as_str).unwrap_or("")),
            name: name.to_string(),
            kind: kind_of(&kinds),
            path: path.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/core#p2pcore@0.1.0","target":{"kind":["lib"],"crate_types":["lib"],"name":"p2pcore"},"profile":{"test":true},"executable":"/t/deps/p2pcore-aaa"}
Compiling noise that is not json
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/core#p2pcore@0.1.0","target":{"kind":["test"],"name":"lifecycle"},"profile":{"test":true},"executable":"/t/deps/lifecycle-bbb"}
{"reason":"compiler-artifact","package_id":"p2pcore 0.1.0 (path+file:///w/crates/core)","target":{"kind":["lib"],"name":"p2pcore"},"profile":{"test":false},"executable":null}
{"reason":"build-script-executed","package_id":"x"}
{"reason":"compiler-artifact","package_id":"path+file:///w/tool#0.2.0","target":{"kind":["bin"],"name":"tool"},"profile":{"test":true},"executable":"/t/deps/tool-ccc"}
{"reason":"compiler-artifact","package_id":"path+file:///w/tool#0.2.0","target":{"kind":["bin"],"name":"tool"},"profile":{"test":true},"executable":"/t/deps/tool-ccc"}
{"reason":"build-finished","success":true}"#;

    #[test]
    fn parses_three_artifacts_and_skips_the_rest() {
        let exes = parse_test_executables(FIXTURE).unwrap();
        assert_eq!(exes.len(), 3);
        assert_eq!(
            (
                exes[0].package.as_str(),
                exes[0].kind,
                exes[0].name.as_str()
            ),
            ("p2pcore", ExeKind::Lib, "p2pcore")
        );
        assert_eq!(
            (exes[1].kind, exes[1].label()),
            (ExeKind::Test, "test:lifecycle".to_string())
        );
        assert_eq!(
            (exes[2].package.as_str(), exes[2].kind),
            ("tool", ExeKind::Bin)
        );
        assert_eq!(exes[2].path, "/t/deps/tool-ccc");
    }

    #[test]
    fn old_style_package_id() {
        assert_eq!(
            package_name("p2pcore 0.1.0 (path+file:///w/crates/core)"),
            "p2pcore"
        );
    }

    #[test]
    fn broken_json_is_an_error_with_line_number() {
        let e = parse_test_executables("{\"reason\":").unwrap_err();
        assert!(matches!(e, TimingError::BadJson { line: 1, .. }));
        assert!(parse_test_executables("").unwrap().is_empty());
    }
}
