//! The extras allowlist: gitignored files that must travel with a checkout.
//!
//! Patterns come from the repo run policy (`[sync] extra = [...]`). Each is a relative path;
//! `*` and `?` match inside one path component, and a trailing `/` takes everything under that
//! directory. Nothing else travels from the ignored set, ever.

use super::refuse::{RefuseReason, Refused};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    comps: Vec<String>,
    /// Trailing `/`: everything under the matched directory.
    dir: bool,
}

/// A validated allowlist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraRules {
    patterns: Vec<Pattern>,
}

/// Most patterns in one allowlist.
pub const MAX_PATTERNS: usize = 64;
/// Deepest directory a wildcard or directory pattern is walked.
const MAX_DEPTH: usize = 12;
/// Most directory entries looked at while collecting candidates.
const MAX_VISITED: usize = 50_000;

fn has_wild(c: &str) -> bool {
    c.contains('*') || c.contains('?')
}

/// Matches one path component against a pattern with `*` and `?`.
fn glob(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pat.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                n += 1;
            }
            Some(c) if Some(c) == name.get(n) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    while pat.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pat.len()
}

impl ExtraRules {
    /// Validates `patterns`: relative, no `..`, no `.git`, bounded in number.
    pub fn new(patterns: &[String]) -> Result<ExtraRules, String> {
        if patterns.len() > MAX_PATTERNS {
            return Err(format!("more than {MAX_PATTERNS} extra patterns"));
        }
        let mut out = Vec::new();
        for raw in patterns {
            let dir = raw.ends_with('/');
            let trimmed = raw.trim_end_matches('/');
            super::paths::check_rel_path(trimmed)
                .map_err(|e| format!("extra pattern {raw:?}: {e}"))?;
            out.push(Pattern {
                comps: trimmed.split('/').map(str::to_string).collect(),
                dir,
            });
        }
        Ok(ExtraRules { patterns: out })
    }

    /// True when there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// True when `rel` (a file path) is allowlisted.
    pub fn matches(&self, rel: &str) -> bool {
        let comps: Vec<&str> = rel.split('/').collect();
        self.patterns.iter().any(|p| {
            let n = p.comps.len();
            if comps.len() < n || (!p.dir && comps.len() != n) {
                return false;
            }
            p.comps
                .iter()
                .zip(&comps)
                .all(|(pc, c)| glob(pc.as_bytes(), c.as_bytes()))
        })
    }

    /// The allowlisted regular files and symlinks that exist under `root`, sorted. Directories
    /// are walked from each pattern's literal prefix only, never the whole checkout.
    pub fn candidates(&self, root: &Path) -> Vec<String> {
        let mut found = std::collections::BTreeSet::new();
        let mut visited = 0usize;
        for p in &self.patterns {
            let prefix: Vec<&str> = p
                .comps
                .iter()
                .map(String::as_str)
                .take_while(|c| !has_wild(c))
                .collect();
            let depth = if p.dir {
                MAX_DEPTH
            } else {
                p.comps.len().saturating_sub(prefix.len())
            };
            let start = super::paths::join(root, &prefix.join("/"));
            let rel0 = prefix.join("/");
            let mut stack = vec![(start, rel0, 0usize)];
            while let Some((path, rel, d)) = stack.pop() {
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    if d > depth || rel.rsplit('/').next() == Some(".git") {
                        continue;
                    }
                    let Ok(rd) = std::fs::read_dir(&path) else {
                        continue;
                    };
                    for e in rd.flatten() {
                        visited += 1;
                        if visited > MAX_VISITED {
                            break;
                        }
                        let name = e.file_name().to_string_lossy().into_owned();
                        let r = if rel.is_empty() {
                            name
                        } else {
                            format!("{rel}/{name}")
                        };
                        stack.push((e.path(), r, d + 1));
                    }
                } else if !rel.is_empty() && self.matches(&rel) {
                    found.insert(rel);
                }
            }
        }
        found.into_iter().collect()
    }
}

/// An allowlisted file that cannot travel because it is a symlink.
pub fn symlink_refusal(path: &str) -> Refused {
    Refused {
        path: path.to_string(),
        reason: RefuseReason::ExtraSymlink,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(p: &[&str]) -> ExtraRules {
        ExtraRules::new(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn r301_patterns_match_exact_paths_wildcards_and_directories() {
        let r = rules(&["local.properties", "config/*.json", "fixtures/"]);
        assert!(r.matches("local.properties"));
        assert!(!r.matches("sub/local.properties"));
        assert!(r.matches("config/app.json"));
        assert!(!r.matches("config/deep/app.json"));
        assert!(!r.matches("config/app.toml"));
        assert!(r.matches("fixtures/a/b/c.bin"));
        assert!(!r.matches("fixture/a"));
    }

    #[test]
    fn r301_bad_patterns_are_refused_by_name() {
        for bad in ["../x", "/abs", ".git/config", "a//b", ""] {
            assert!(ExtraRules::new(&[bad.to_string()]).is_err(), "{bad:?}");
        }
        assert!(ExtraRules::new(&vec!["a".to_string(); MAX_PATTERNS + 1]).is_err());
    }

    #[test]
    fn glob_handles_stars_and_question_marks() {
        assert!(glob(b"*.rs", b"main.rs"));
        assert!(glob(b"a?c", b"abc"));
        assert!(glob(b"*", b""));
        assert!(!glob(b"a?c", b"ac"));
        assert!(glob(b"a*b*c", b"axxbyyc"));
        assert!(!glob(b"a*b*c", b"axxbyy"));
    }

    #[test]
    fn candidates_walk_only_under_the_literal_prefix() {
        let d = tempfile::tempdir().unwrap();
        let w = |rel: &str| {
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        };
        w("local.properties");
        w("config/a.json");
        w("config/b.txt");
        w("fixtures/x/y.bin");
        w("other/z.json");
        let r = rules(&[
            "local.properties",
            "config/*.json",
            "fixtures/",
            "missing.cfg",
        ]);
        assert_eq!(
            r.candidates(d.path()),
            ["config/a.json", "fixtures/x/y.bin", "local.properties"]
        );
    }
}
