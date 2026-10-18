//! Per-node "missing capability" marks, from classified failed-job logs (see `classify`).
//!
//! A mark says: on NODE, job J failed because the machine lacks CAPABILITY (evidence: the
//! log line). Placement consults the marks as an advisory skip: `anywhere` does not choose a
//! machine with a mark whose capability the command needs ([`needed`]); a named peer still
//! gets the job, with a note. Marks are kept in `<state>/health/capabilities.json`.
//!
//! Who marks whom: every node classifies its own failed jobs (node = its hostname; the mark
//! travels in its snapshot's `health.capabilities`). The controller also classifies the log of
//! a failed FORWARDED job (it mirrors the peer's log) and marks the peer by its configured
//! name, so placement knows at once, without waiting for the peer's snapshot.
//!
//! A mark clears when (1) a later job of the same command kind passes on that node (it got
//! past that step; for `disk-space` any later passed job), (2) it expires (default 24 h, so a
//! fixed machine is retried), or (3) by hand: `tasks alerts clear NODE [CAPABILITY]`.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "health/capabilities.json";
/// Manual clear requests (`tasks alerts clear`), consumed by the daemon's health monitor.
pub const CLEAR_DIR: &str = "health/clear-requests";
const MAX_MARKS: usize = 200;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityMark {
    /// Hostname for a node's own mark; a peer's configured name for a controller's mark.
    pub node: String,
    /// `libclang`, `linker`, `native-lib:vpx`, ...
    pub capability: String,
    /// Pattern family (`classify::PATTERNS`).
    pub family: String,
    /// The failed job in the marking machine's ledger.
    pub job: i64,
    /// For a forwarded job: the job id on the peer, when known.
    #[serde(default)]
    pub remote_job: Option<i64>,
    /// Command kind of the failed job (`[offload] ` prefix removed).
    pub kind_key: String,
    pub line: String,
    pub marked_at: f64,
    pub expires_at: f64,
    /// `job log` or `forwarded job log`.
    pub source: String,
}

impl CapabilityMark {
    pub fn evidence(&self) -> String {
        if self.job == 0 {
            return format!("probe: {}", self.line);
        }
        match self.remote_job {
            Some(remote) => format!(
                "job #{} (remote #{remote} on {}): {}",
                self.job, self.node, self.line
            ),
            None => format!("job #{}: {}", self.job, self.line),
        }
    }
    /// `lacks libclang (bindgen)`.
    pub fn lacks(&self) -> String {
        let what = super::classify::pattern(&self.family)
            .map(|p| p.lacks)
            .unwrap_or("a capability");
        match self.capability.split_once(':') {
            Some((_, name)) => format!("lacks {what} ({name})"),
            None => format!("lacks {what}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Marks {
    #[serde(default)]
    pub marks: Vec<CapabilityMark>,
    /// Highest job id whose log was already classified (0: none yet).
    #[serde(default)]
    pub classified_through: i64,
}

/// Command kind without the offload/forward marker, so a forwarded job and the peer's own
/// job of the same command share it.
pub fn base_kind(kind_key: &str) -> String {
    kind_key
        .strip_prefix(crate::metrics::command::OFFLOAD_MARKER)
        .unwrap_or(kind_key)
        .to_string()
}

impl Marks {
    pub fn path(state: &Path) -> PathBuf {
        state.join(FILE)
    }
    pub fn load(state: &Path) -> Self {
        crate::peer::records::read_json(&Self::path(state)).unwrap_or_default()
    }
    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        crate::peer::records::write_json(&Self::path(state), self)
    }

    /// Adds or refreshes the mark of (node, capability). True when it is new.
    pub fn mark(&mut self, mark: CapabilityMark) -> bool {
        match self
            .marks
            .iter_mut()
            .find(|m| m.node == mark.node && m.capability == mark.capability)
        {
            Some(existing) => {
                if mark.job >= existing.job {
                    *existing = mark;
                }
                false
            }
            None => {
                self.marks.push(mark);
                if self.marks.len() > MAX_MARKS {
                    self.marks.remove(0);
                }
                true
            }
        }
    }

    /// A job passed on `node`: clears that node's older marks of the same command kind (and
    /// `disk-space` marks, which any passed job disproves).
    pub fn passed(&mut self, node: &str, job: i64, kind_key: &str) -> Vec<CapabilityMark> {
        let kind = base_kind(kind_key);
        self.remove(|m| {
            m.node == node && m.job < job && (m.kind_key == kind || m.family == "disk-space")
        })
    }

    pub fn expire(&mut self, now: f64) -> Vec<CapabilityMark> {
        self.remove(|m| m.expires_at <= now)
    }

    /// Manual clear: every mark of `node`, or only `capability`.
    pub fn clear(&mut self, node: &str, capability: Option<&str>) -> Vec<CapabilityMark> {
        self.remove(|m| m.node == node && capability.is_none_or(|c| c == m.capability))
    }

    fn remove(&mut self, gone: impl Fn(&CapabilityMark) -> bool) -> Vec<CapabilityMark> {
        let (removed, kept) = std::mem::take(&mut self.marks).into_iter().partition(gone);
        self.marks = kept;
        removed
    }
}

fn base(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
}

fn mentions_gradle(argv: &[String]) -> bool {
    argv.iter().any(|w| {
        let b = base(w);
        b == "gradlew" || b == "gradle" || w.contains("gradlew ") || w.contains("/gradlew")
    })
}

/// Does the nearest `Cargo.lock` at or above `cwd` (bounded walk and read) mention `name`?
/// None when no lock file is found or readable.
fn lock_mentions(cwd: &Path, name: &str) -> Option<bool> {
    let needle = format!("name = \"{name}\"");
    for dir in cwd.ancestors().take(8) {
        let lock = dir.join("Cargo.lock");
        if let Ok(meta) = std::fs::metadata(&lock)
            && meta.is_file()
            && meta.len() <= 8 * 1024 * 1024
        {
            return std::fs::read_to_string(&lock)
                .ok()
                .map(|t| t.contains(&needle));
        }
    }
    None
}

/// Whether a command (run from `cwd`, when known) needs a capability family. Coarse on
/// purpose: a Cargo command needs the C toolchain capabilities; `libclang` only when the
/// checkout's Cargo.lock has `bindgen` (or no lock file is known); Gradle needs the SDK and
/// Java; a full disk affects everything.
pub fn needed(family: &str, argv: &[String], cwd: Option<&Path>) -> bool {
    let cargo = crate::metrics::command::is_cargo(argv);
    match family {
        "disk-space" => true,
        "libclang" => {
            cargo
                && cwd
                    .and_then(|c| lock_mentions(c, "bindgen"))
                    .unwrap_or(true)
        }
        "linker" | "native-lib" | "pkg-config" => cargo,
        "android-sdk" => {
            mentions_gradle(argv)
                || argv
                    .iter()
                    .any(|w| base(w) == "cargo-ndk" || w == "ndk" || w.contains("cargo ndk"))
        }
        "java" => mentions_gradle(argv) || argv.iter().any(|w| base(w) == "java"),
        _ => false,
    }
}

/// The first mark (if any) among `marks` whose capability `argv` needs.
pub fn blocking<'a>(
    marks: impl IntoIterator<Item = &'a CapabilityMark>,
    argv: &[String],
    cwd: Option<&Path>,
) -> Option<&'a CapabilityMark> {
    marks.into_iter().find(|m| needed(&m.family, argv, cwd))
}

/// The marks about each peer, as placement sees them ([`crate::placement::marks_for`]: this
/// machine's own marks of the peer plus the ones the peer publishes in its snapshot). Reroute
/// consults it through [`crate::reroute::Capabilities`], so a waiting job is never moved to a
/// peer that lacks what it needs, the same way `anywhere` placement skips that peer.
#[derive(Clone, Debug, Default)]
pub struct PeerMarks(pub Vec<(String, Vec<CapabilityMark>)>);

impl PeerMarks {
    pub fn new(peers: &[crate::placement::Peer], local: &[CapabilityMark]) -> Self {
        Self(
            peers
                .iter()
                .map(|p| (p.name.clone(), crate::placement::marks_for(p, local)))
                .collect(),
        )
    }
    /// With this machine's marks from `state`.
    pub fn load(state: &Path, peers: &[crate::placement::Peer]) -> Self {
        Self::new(peers, &Marks::load(state).marks)
    }
}

impl crate::reroute::Capabilities for PeerMarks {
    fn lacks(&self, node: &str, argv: &[String], cwd: Option<&Path>) -> Option<String> {
        let (_, marks) = self.0.iter().find(|(name, _)| name == node)?;
        let m = blocking(marks, argv, cwd)?;
        let what = m.lacks();
        Some(format!(
            "{} ({})",
            what.strip_prefix("lacks ").unwrap_or(&what),
            m.evidence()
        ))
    }
}

/// A manual clear request file body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClearRequest {
    pub node: String,
    #[serde(default)]
    pub capability: Option<String>,
}

/// Writes a clear request for the daemon (`tasks alerts clear`). Returns its path.
pub fn request_clear(state: &Path, request: &ClearRequest) -> std::io::Result<PathBuf> {
    let path = state.join(CLEAR_DIR).join(format!(
        "{}-{}.json",
        crate::execution::now(),
        std::process::id()
    ));
    crate::peer::records::write_json(&path, request)?;
    Ok(path)
}

/// Reads and removes pending clear requests (bounded). Invalid files are removed too.
pub fn take_clear_requests(state: &Path) -> Vec<ClearRequest> {
    let dir = state.join(CLEAR_DIR);
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten().take(64) {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.') || !n.ends_with(".json"))
        {
            continue;
        }
        if let Some(r) = crate::peer::records::read_json::<ClearRequest>(&path) {
            out.push(r);
        }
        let _ = std::fs::remove_file(&path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn mark(node: &str, cap: &str, job: i64, kind: &str) -> CapabilityMark {
        CapabilityMark {
            node: node.into(),
            capability: cap.into(),
            family: cap.split(':').next().unwrap().into(),
            job,
            remote_job: None,
            kind_key: kind.into(),
            line: "Unable to find libclang".into(),
            marked_at: 100.0,
            expires_at: 1000.0,
            source: "job log".into(),
        }
    }

    #[test]
    fn peer_marks_answer_reroute_for_the_commands_that_need_the_capability() {
        use crate::reroute::Capabilities;
        let nixos = crate::placement::Peer {
            name: "nixos".into(),
            ..Default::default()
        };
        let other = crate::placement::Peer {
            name: "other".into(),
            ..Default::default()
        };
        let marks = PeerMarks::new(
            &[nixos, other],
            &[mark("nixos", "libclang", 7, "cargo build")],
        );
        let why = marks
            .lacks("nixos", &words("cargo build -p a"), None)
            .unwrap();
        assert!(
            !why.starts_with("lacks ") && why.contains("job #7: Unable to find libclang"),
            "{why}"
        );
        assert_eq!(marks.lacks("nixos", &words("sh -c true"), None), None);
        assert_eq!(marks.lacks("other", &words("cargo build"), None), None);
        assert_eq!(marks.lacks("unknown", &words("cargo build"), None), None);
    }

    #[test]
    fn needed_by_command_kind() {
        let cargo = words("env CARGO_BUILD_JOBS=6 cargo build --release");
        let gradle = words("./gradlew :app:assembleDebug");
        let sh = words("sh -c true");
        assert!(needed("libclang", &cargo, None));
        assert!(needed("linker", &cargo, None));
        assert!(!needed("libclang", &gradle, None));
        assert!(needed("android-sdk", &gradle, None));
        assert!(needed("java", &gradle, None));
        assert!(!needed("java", &cargo, None));
        assert!(needed("disk-space", &sh, None));
        assert!(!needed("libclang", &sh, None));
        assert!(!needed("unknown", &cargo, None));
    }

    #[test]
    fn libclang_follows_the_checkouts_cargo_lock() {
        let dir = std::env::temp_dir().join(format!("tasks-capability-{}", std::process::id()));
        let sub = dir.join("crates/a");
        std::fs::create_dir_all(&sub).unwrap();
        let cargo = words("cargo test -p a");
        std::fs::write(dir.join("Cargo.lock"), "[[package]]\nname = \"serde\"\n").unwrap();
        assert!(
            !needed("libclang", &cargo, Some(&sub)),
            "no bindgen in the lock"
        );
        std::fs::write(
            dir.join("Cargo.lock"),
            "[[package]]\nname = \"bindgen\"\nversion = \"0.69.5\"\n",
        )
        .unwrap();
        assert!(needed("libclang", &cargo, Some(&sub)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn marks_dedupe_clear_on_pass_expire_and_by_hand() {
        let mut m = Marks::default();
        assert!(m.mark(mark("nixos", "libclang", 456, "cargo build")));
        assert!(
            !m.mark(mark("nixos", "libclang", 460, "cargo build")),
            "refresh, not a second mark"
        );
        assert_eq!(m.marks.len(), 1);
        assert_eq!(m.marks[0].job, 460);
        // A different command passing does not prove libclang.
        assert!(m.passed("nixos", 470, "cargo test -p other").is_empty());
        // An older pass of the same kind does not clear a newer failure.
        assert!(m.passed("nixos", 459, "cargo build").is_empty());
        // The same kind passing later (also forwarded: marker stripped) clears it.
        let cleared = m.passed("nixos", 471, "[offload] cargo build");
        assert_eq!(cleared.len(), 1);
        assert!(m.marks.is_empty());
        m.mark(mark("nixos", "disk-space", 480, "cargo build"));
        assert_eq!(
            m.passed("nixos", 481, "sh -c true").len(),
            1,
            "any pass disproves a full disk"
        );
        m.mark(mark("nixos", "linker", 482, "cargo build"));
        assert!(m.expire(999.0).is_empty());
        assert_eq!(m.expire(1000.0).len(), 1);
        m.mark(mark("nixos", "linker", 483, "cargo build"));
        m.mark(mark("nixos", "java", 484, "gradlew build"));
        m.mark(mark("laptop", "java", 485, "gradlew build"));
        assert_eq!(m.clear("nixos", Some("java")).len(), 1);
        assert_eq!(m.clear("nixos", None).len(), 1);
        assert_eq!(m.marks.len(), 1);
        let gradle = words("gradlew build");
        assert_eq!(blocking(&m.marks, &gradle, None).unwrap().node, "laptop");
        assert!(blocking(&m.marks, &words("cargo build"), None).is_none());
        assert_eq!(m.marks[0].lacks(), "lacks a Java runtime/JDK");
        assert_eq!(
            mark("n", "native-lib:vpx", 1, "k").lacks(),
            "lacks a native static library (vpx)"
        );
    }
}
