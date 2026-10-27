//! Stable build outputs outside the working tree.
//!
//! `target` (and the Android output directories) become symlinks into
//! `<cache root>/loadtable/checkouts/<key>/...`, so a synced copy that retention deletes, or a
//! fresh worktree, still builds warm. The rules are the earlier release's:
//!
//! - an existing entry is never replaced, moved or deleted; a real directory is kept and
//!   reported, a symlink (placed earlier, or by hand) is kept as it is;
//! - a placement that races another keeps the winner;
//! - the plan is computed without creating anything, so admission can tell whether a job needs
//!   NEW cache output (and so waits for the disk reserve) or only reuses links.
//!
//! The key is per canonical path for a local checkout and per (origin peer, checkout id) for a
//! copy under the peer store, so the two never collide and a re-synced copy finds its output.

use super::{CHECKOUTS_SUBDIR, CacheError};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Every top-level output a placement can create, as a peer may fetch through it. The Android
/// outputs are two levels down, so these are relative paths.
pub const LINK_NAMES: &[&str] = &["target", "android/build", "android/app/build"];
/// Programs that write no Cargo or Gradle output (the earlier release's list): they get no links.
const NO_OUTPUT_PROGRAMS: &[&str] = &[
    "npm", "npx", "pnpm", "yarn", "node", "bun", "deno", "git", "ffmpeg", "adb",
];

/// What a command writes, from its program name after transparent `env` assignments. A shell,
/// script or other unknown program may run Cargo or Gradle, so it keeps every link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputUse {
    /// Nothing worth a link.
    None,
    /// Cargo's `target`.
    Cargo,
    /// Anything: `target` and the Android outputs.
    All,
}

impl OutputUse {
    /// The use of `argv`.
    pub fn of(argv: &[String]) -> OutputUse {
        if tool_index(argv, &["cargo"]).is_some() {
            OutputUse::Cargo
        } else if tool_index(argv, NO_OUTPUT_PROGRAMS).is_some() {
            OutputUse::None
        } else {
            OutputUse::All
        }
    }
}

/// Index of one of `names` as the program, directly or after transparent `env` assignments.
/// Only these are understood; shell text is never parsed.
pub fn tool_index(argv: &[String], names: &[&str]) -> Option<usize> {
    let mut index = 0;
    if Path::new(argv.first()?).file_name()?.to_str()? == "env" {
        index = 1;
        if argv.get(index)?.as_str() == "--" {
            index += 1;
        }
        while argv.get(index)?.contains('=') {
            index += 1;
        }
    }
    names
        .contains(&Path::new(argv.get(index)?).file_name()?.to_str()?)
        .then_some(index)
}

/// One output the plan covers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    link: PathBuf,
    real: PathBuf,
    /// True when `link` is already a symlink (placed earlier).
    present: bool,
    /// True when it points into the cache at a directory that is gone (the cache was cleaned):
    /// the directory is recreated, the entry itself is left alone.
    repair: bool,
}

/// What a placement would do, computed without creating anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputPlan {
    links: Vec<Planned>,
    preserved: Vec<PathBuf>,
    note: Option<String>,
    cache_root: PathBuf,
    /// Whether the plan found a checkout and a program that writes output.
    covered: bool,
}

/// What a placement did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placed {
    /// Every output link in force: (link in the tree, directory it points to).
    pub links: Vec<(PathBuf, PathBuf)>,
    /// How many of them this placement created.
    pub created: usize,
    /// Existing real directories left where they are, not cache-backed.
    pub preserved: Vec<PathBuf>,
    /// Anything worth a line in the job's output (a skipped placement, a failure).
    pub notes: Vec<String>,
}

impl OutputPlan {
    /// True when the job may write NEW output on the cache filesystem: a link is still to be
    /// created, or the plan could not tell (no checkout found, a program that may write
    /// anything). A job whose links all exist, or that keeps a real local directory, needs no
    /// reserve (R-061 of the earlier release).
    pub fn needs_new_output(&self) -> bool {
        if !self.covered {
            return true;
        }
        self.links.iter().any(|l| !l.present || l.repair)
            || (self.links.is_empty() && self.preserved.is_empty())
    }

    /// Creates the planned links. An entry that appeared meanwhile is kept as it is, and a
    /// failure to create one is a note, not a failed job: links are an optimization.
    pub fn apply(self) -> Placed {
        let mut out = Placed {
            preserved: self.preserved,
            notes: self.note.into_iter().collect(),
            ..Placed::default()
        };
        for p in self.links {
            if p.present {
                let target = read_link(&p.link, &p.real);
                if p.repair
                    && let Err(e) = std::fs::create_dir_all(&target)
                {
                    out.notes.push(format!(
                        "output link {} points at {}, which cannot be recreated: {e}",
                        p.link.display(),
                        target.display()
                    ));
                }
                out.links.push((p.link, target));
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&p.real) {
                out.notes.push(format!(
                    "output link {} skipped: cannot create {}: {e}",
                    p.link.display(),
                    p.real.display()
                ));
                continue;
            }
            match make_link(&p.real, &p.link) {
                Ok(()) => {
                    out.created += 1;
                    out.links.push((p.link, p.real));
                }
                // a concurrent placement won: its entry is kept, whatever it is
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::fs::symlink_metadata(&p.link).is_ok_and(|m| m.is_symlink()) {
                        out.links
                            .push((p.link.clone(), read_link(&p.link, &p.real)));
                    } else {
                        out.preserved.push(p.link);
                    }
                }
                Err(e) => out
                    .notes
                    .push(format!("output link {} skipped: {e}", p.link.display())),
            }
        }
        out
    }
}

fn read_link(link: &Path, fallback: &Path) -> PathBuf {
    std::fs::read_link(link).unwrap_or_else(|_| fallback.to_path_buf())
}

#[cfg(unix)]
fn make_link(real: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(real, link)
}

#[cfg(not(unix))]
fn make_link(_real: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symbolic links are not supported here",
    ))
}

/// A checkout: its root and where its outputs live, relative to `checkouts/`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkout {
    root: PathBuf,
    key: PathBuf,
}

fn sha_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The checkout holding `cwd` (already canonical): the synced copy under `store` when it is
/// one, else the nearest ancestor with a `.git` entry (a directory, or a file in a linked
/// worktree).
fn checkout(cwd: &Path, store: &Path) -> Option<Checkout> {
    if let Ok(canon) = std::fs::canonicalize(store)
        && let Ok(rel) = cwd.strip_prefix(&canon)
    {
        let names: Vec<&std::ffi::OsStr> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n),
                _ => None,
            })
            .collect();
        if let [peer, id, tree, ..] = names.as_slice()
            && *tree == "tree"
        {
            return Some(Checkout {
                root: canon.join(peer).join(id).join(tree),
                key: Path::new("peer").join(peer).join(id),
            });
        }
    }
    let root = cwd
        .ancestors()
        .find(|d| std::fs::symlink_metadata(d.join(".git")).is_ok())?
        .to_path_buf();
    let key = PathBuf::from(sha_hex(root.as_os_str().as_encoded_bytes()));
    Some(Checkout { root, key })
}

/// Plans the output links of a job running `argv` in `cwd`. `cache_root` is the cache root and
/// `store` the peer store. Nothing is created.
pub fn plan(cache_root: &Path, store: &Path, cwd: &Path, argv: &[String]) -> OutputPlan {
    let uses = OutputUse::of(argv);
    let mut plan = OutputPlan {
        cache_root: cache_root.to_path_buf(),
        ..OutputPlan::default()
    };
    if uses == OutputUse::None {
        return plan;
    }
    let canon = match std::fs::canonicalize(cwd) {
        Ok(c) => c,
        Err(e) => {
            plan.note = Some(format!(
                "output placement skipped: cannot resolve the working directory {}: {e}",
                cwd.display()
            ));
            return plan;
        }
    };
    let Some(co) = checkout(&canon, store) else {
        plan.note = Some(
            "output placement skipped: no Git checkout above the working directory".to_string(),
        );
        return plan;
    };
    plan.covered = true;
    let base = cache_root.join(CHECKOUTS_SUBDIR).join(&co.key);
    if co.root.join("Cargo.toml").is_file() {
        plan_link(&co.root, "target", &base, &mut plan);
    }
    if uses == OutputUse::All {
        for rel in ["android/build", "android/app/build"] {
            plan_link(&co.root, rel, &base, &mut plan);
        }
    }
    // an independent Cargo workspace below the checkout root gets a target of its own
    if let Some(ws) = cargo_workspace_root(&canon, &co.root).filter(|w| *w != co.root)
        && let Ok(rel) = ws.strip_prefix(&co.root)
    {
        let nested = base
            .join("nested")
            .join(sha_hex(rel.as_os_str().as_encoded_bytes()));
        plan_link(&ws, "target", &nested, &mut plan);
    }
    plan
}

/// One link: `<dir>/<rel>` pointing to `<base>/<rel>`. An existing entry is never touched.
fn plan_link(dir: &Path, rel: &str, base: &Path, plan: &mut OutputPlan) {
    let link = dir.join(rel);
    match std::fs::symlink_metadata(&link) {
        Ok(meta) => {
            if meta.is_symlink() {
                let target = std::fs::read_link(&link).ok();
                let repair = !link.exists()
                    && target
                        .as_ref()
                        .is_some_and(|t| t.starts_with(&plan.cache_root));
                plan.links.push(Planned {
                    real: base.join(rel),
                    link,
                    present: true,
                    repair,
                });
            } else {
                plan.preserved.push(link);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if link.parent().is_some_and(Path::is_dir) {
                plan.links.push(Planned {
                    real: base.join(rel),
                    link,
                    present: false,
                    repair: false,
                });
            }
        }
        Err(e) => plan.note = Some(format!("output placement of {}: {e}", link.display())),
    }
}

/// Conservative model of Cargo's workspace-root discovery for a canonical cwd, bounded to the
/// checkout root: the nearest manifest; its own `[workspace]`; an explicit `package.workspace`;
/// else the nearest ancestor `[workspace]` that does not exclude it; else the package itself.
/// An unreadable manifest yields `None` (no nested placement), leaving diagnosis to Cargo.
pub fn cargo_workspace_root(cwd: &Path, root: &Path) -> Option<PathBuf> {
    let within = |path: &Path| path.starts_with(root);
    let manifest = |dir: &Path| -> Option<toml::Table> {
        toml::from_str(&std::fs::read_to_string(dir.join("Cargo.toml")).ok()?).ok()
    };
    let package = cwd
        .ancestors()
        .take_while(|dir| within(dir))
        .find(|dir| dir.join("Cargo.toml").is_file())?;
    let own = manifest(package)?;
    if own.contains_key("workspace") {
        return Some(package.into());
    }
    if let Some(pointer) = own
        .get("package")
        .and_then(|p| p.get("workspace"))
        .and_then(|w| w.as_str())
    {
        let workspace = std::fs::canonicalize(package.join(pointer)).ok()?;
        return within(&workspace).then_some(workspace);
    }
    for dir in package.ancestors().skip(1).take_while(|dir| within(dir)) {
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        let Some(workspace) = manifest(dir)?.get("workspace").cloned() else {
            continue;
        };
        let excluded = workspace
            .get("exclude")
            .and_then(|e| e.as_array())
            .is_some_and(|list| {
                list.iter()
                    .filter_map(|e| e.as_str())
                    .any(|e| package.starts_with(dir.join(e)))
            });
        if !excluded {
            return Some(dir.into());
        }
    }
    Some(package.into())
}

/// A Gradle command in an `android/` Gradle project without the git-ignored `local.properties`:
/// writes it with `sdk.dir` from `ANDROID_HOME` or `ANDROID_SDK_ROOT` when one names an
/// existing directory, else fails naming the file. An existing file is never touched, and
/// nothing is copied from another checkout. Returns the line to log when it wrote one.
pub fn ensure_android_local_properties(
    argv: &[String],
    cwd: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, CacheError> {
    if tool_index(argv, &["gradlew", "gradle"]).is_none() {
        return Ok(None);
    }
    let Some(project) = cwd.ancestors().find(|dir| {
        dir.file_name().is_some_and(|n| n == "android")
            && (dir.join("settings.gradle").is_file() || dir.join("settings.gradle.kts").is_file())
    }) else {
        return Ok(None);
    };
    let properties = project.join("local.properties");
    if std::fs::symlink_metadata(&properties).is_ok() {
        return Ok(None);
    }
    for name in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        let Some(sdk) = env(name).map(PathBuf::from) else {
            continue;
        };
        // properties files are Latin-1 with backslash escapes; keep to plain ASCII
        let Some(text) = sdk.to_str().filter(|s| {
            sdk.is_absolute() && sdk.is_dir() && s.bytes().all(|b| (b' '..=b'~').contains(&b))
        }) else {
            continue;
        };
        let body = format!(
            "# Created by loadtable from the machine's {name}; git-ignored per-checkout file.\nsdk.dir={}\n",
            text.replace('\\', "\\\\")
        );
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&properties)
        {
            Ok(mut file) => file
                .write_all(body.as_bytes())
                .map_err(|e| CacheError::io(&properties, e))?,
            // created concurrently (by the user, say): theirs is kept
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(CacheError::io(&properties, e)),
        }
        return Ok(Some(format!(
            "created {} with sdk.dir={text} from {name}",
            properties.display()
        )));
    }
    Err(CacheError::NoSdk {
        project: project.to_path_buf(),
        properties,
    })
}

/// True for a Gradle command, whose Cargo output the machine owns.
pub fn is_gradle(argv: &[String]) -> bool {
    tool_index(argv, &["gradlew", "gradle"]).is_some()
}

#[cfg(test)]
mod tests;
