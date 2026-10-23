//! [`GixTransfer`]: the [`PackTransfer`] used in production.
//!
//! In process with gix 0.90: `init_mirror`, `has_object`, `commit_tree`, and (in `gix_objects`)
//! `list_refs`, `update_refs` (one ref transaction), `tree_files`, `tree_contains` and
//! `diff_tree`. The remaining steps use the `git` command, because gix has a real gap for each:
//!
//! - `build_pack`: `git pack-objects --revs --thin`. gix-pack writes packs from an object
//!   list and reuses existing deltas, but has no thin-pack generation against excluded base
//!   objects, which is what makes a re-sync send only the changed bytes.
//! - `receive_pack`: `git index-pack --fix-thin`. gix-pack can complete a thin pack
//!   (`Bundle::write_to_directory` with a base lookup) but only behind its `streaming-input`
//!   feature, which `gix` enables only together with its network clients. Using it means a
//!   direct `gix-pack` dependency pinned to gix's version plus its progress types, for the same
//!   behaviour as the command.
//! - `checkout_tree`: `git read-tree --reset -u`. gix can check out a whole index, but not
//!   the two-way "update to this tree, remove what the old index listed, restore files a job
//!   modified, leave untracked build outputs" operation sync needs.
//! - `gc`: gix has no repack or prune.
//!
//! Every command runs with a clean environment (no inherited `GIT_*`, no system or global
//! config, no prompts) and a fixed `GIT_DIR`.

use super::gix_objects;
use super::transfer::{CheckoutRequest, PackRequest, PackTransfer, TransferError, TreeChange};
use crate::ids::GitId;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// gix for object lookups, the `git` command for the named gaps above.
#[derive(Debug, Clone)]
pub struct GixTransfer {
    git: PathBuf,
    /// A cgroup's `cgroup.procs`: every `git` child joins it before it execs, so the work counts
    /// in the transfer cgroup (design §9).
    cgroup_procs: Option<PathBuf>,
}

impl Default for GixTransfer {
    fn default() -> Self {
        GixTransfer::new()
    }
}

fn git_err(what: &'static str, detail: impl Into<String>) -> TransferError {
    TransferError::Git {
        what,
        detail: detail.into(),
    }
}

/// The last part of a command's stderr, for an error message.
fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let start = s.len().saturating_sub(600);
    let start = (start..=s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    s.get(start..).unwrap_or("").to_string()
}

const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// The id of the empty tree for this id's hash length.
pub fn empty_tree_id(like: &GitId) -> GitId {
    let hex = if like.as_str().len() == 64 {
        EMPTY_TREE_SHA256
    } else {
        EMPTY_TREE_SHA1
    };
    // both constants are valid ids, so the fallback is never taken
    GitId::parse(hex).unwrap_or_else(|_| like.clone())
}

impl GixTransfer {
    /// Uses `git` from `PATH`.
    pub fn new() -> GixTransfer {
        GixTransfer {
            git: "git".into(),
            cgroup_procs: None,
        }
    }

    /// Runs every `git` child inside the cgroup behind this `cgroup.procs` file.
    pub fn with_cgroup_procs(mut self, procs: Option<PathBuf>) -> GixTransfer {
        self.cgroup_procs = procs;
        self
    }

    /// Uses this git binary.
    pub fn with_git(git: impl Into<PathBuf>) -> GixTransfer {
        GixTransfer {
            git: git.into(),
            cgroup_procs: None,
        }
    }

    fn cmd(&self, git_dir: &Path) -> Command {
        let mut c = match &self.cgroup_procs {
            Some(procs) => {
                // a shell writes its pid into the cgroup, then becomes git
                let argv =
                    tasks_platform::place_in_cgroup(procs, &[self.git.display().to_string()]);
                let mut it = argv.into_iter();
                let mut c = Command::new(it.next().unwrap_or_default());
                c.args(it);
                c
            }
            None => Command::new(&self.git),
        };
        for k in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
        ] {
            c.env_remove(k);
        }
        c.env("GIT_DIR", git_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, what: &'static str, mut c: Command) -> Result<Vec<u8>, TransferError> {
        let out = c
            .output()
            .map_err(|e| git_err(what, format!("could not run {}: {e}", self.git.display())))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(git_err(what, tail(&out.stderr)))
        }
    }
}

impl PackTransfer for GixTransfer {
    fn init_mirror(&self, dir: &Path) -> Result<(), TransferError> {
        if dir.join("HEAD").is_file() {
            return Ok(());
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        gix::init_bare(dir).map_err(|e| git_err("init mirror", e.to_string()))?;
        Ok(())
    }

    fn has_object(&self, git_dir: &Path, id: &GitId) -> bool {
        let Ok(repo) = gix::open(git_dir) else {
            return false;
        };
        let Ok(oid) = gix::ObjectId::from_hex(id.as_str().as_bytes()) else {
            return false;
        };
        repo.has_object(oid)
    }

    fn commit_tree(&self, git_dir: &Path, commit: &GitId) -> Option<GitId> {
        let repo = gix::open(git_dir).ok()?;
        let oid = gix::ObjectId::from_hex(commit.as_str().as_bytes()).ok()?;
        let tree = repo
            .find_object(oid)
            .ok()?
            .try_into_commit()
            .ok()?
            .tree_id()
            .ok()?;
        GitId::parse(&tree.to_string()).ok()
    }

    fn list_refs(
        &self,
        git_dir: &Path,
        prefix: &str,
    ) -> Result<Vec<(String, GitId)>, TransferError> {
        gix_objects::list_refs(git_dir, prefix)
    }

    fn update_refs(
        &self,
        git_dir: &Path,
        set: &[(String, GitId)],
        delete: &[String],
    ) -> Result<(), TransferError> {
        gix_objects::update_refs(git_dir, set, delete)
    }

    fn build_pack(&self, req: &PackRequest<'_>, out: &mut dyn Write) -> Result<u64, TransferError> {
        let mut c = self.cmd(req.git_dir);
        c.args([
            "-c",
            "pack.threads=2",
            "pack-objects",
            "--revs",
            "--thin",
            "--stdout",
            "--delta-base-offset",
            "-q",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = c
            .spawn()
            .map_err(|e| git_err("pack-objects", format!("could not run git: {e}")))?;
        let mut input = String::new();
        for w in req.want {
            input.push_str(w.as_str());
            input.push('\n');
        }
        for h in req.have {
            input.push('^');
            input.push_str(h.as_str());
            input.push('\n');
        }
        if let Some(mut si) = child.stdin.take() {
            si.write_all(input.as_bytes())?;
        }
        let stderr = child.stderr.take();
        let err_thread = std::thread::spawn(move || {
            let mut s = Vec::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_end(&mut s);
            }
            s
        });
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| git_err("pack-objects", "no stdout"))?;
        let mut buf = vec![0u8; 128 * 1024];
        let mut total = 0u64;
        let result = loop {
            let n = match stdout.read(&mut buf) {
                Ok(n) => n,
                Err(e) => break Err(TransferError::Io(e)),
            };
            if n == 0 {
                break Ok(());
            }
            total += n as u64;
            if total > req.max_bytes {
                break Err(TransferError::PackTooLarge {
                    limit: req.max_bytes,
                });
            }
            if let Err(e) = out.write_all(buf.get(..n).unwrap_or(&[])) {
                break Err(TransferError::Sink(e.to_string()));
            }
        };
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let err = err_thread.join().unwrap_or_default();
        result?;
        if status.success() {
            Ok(total)
        } else {
            Err(git_err("pack-objects", tail(&err)))
        }
    }

    fn receive_pack(&self, git_dir: &Path, pack: &Path) -> Result<(), TransferError> {
        let mut c = self.cmd(git_dir);
        c.args(["index-pack", "--stdin", "--fix-thin"])
            .stdin(std::fs::File::open(pack)?);
        self.run("index-pack", c).map(|_| ())
    }

    fn checkout_tree(&self, req: &CheckoutRequest<'_>) -> Result<(), TransferError> {
        std::fs::create_dir_all(req.worktree)?;
        if let Some(parent) = req.index.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tree = match req.tree {
            Some(t) => t.clone(),
            None => {
                GitId::parse(EMPTY_TREE_SHA1).map_err(|e| git_err("checkout", e.to_string()))?
            }
        };
        let mut c = self.cmd(req.git_dir);
        c.env("GIT_WORK_TREE", req.worktree)
            .env("GIT_INDEX_FILE", req.index)
            .args([
                "-c",
                "core.bare=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.symlinks=true",
                "read-tree",
                "--reset",
                "-u",
            ])
            .arg(tree.as_str());
        self.run("read-tree", c).map(|_| ())
    }

    fn tree_contains(
        &self,
        git_dir: &Path,
        tree: &GitId,
        paths: &[String],
    ) -> Result<Vec<String>, TransferError> {
        gix_objects::tree_contains(git_dir, tree, paths)
    }

    fn tree_files(&self, git_dir: &Path, tree: &GitId) -> Result<Vec<String>, TransferError> {
        gix_objects::tree_files(git_dir, tree)
    }

    fn diff_tree(
        &self,
        git_dir: &Path,
        old: Option<&GitId>,
        new: &GitId,
    ) -> Result<Vec<TreeChange>, TransferError> {
        gix_objects::diff_tree(git_dir, old, new)
    }

    fn gc(&self, git_dir: &Path, prune_age: &str) -> Result<(), TransferError> {
        let mut c = self.cmd(git_dir);
        c.args(["-c", "gc.autoDetach=false", "gc", "-q"])
            .arg(format!("--prune={prune_age}"));
        self.run("gc", c).map(|_| ())
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    // R-337: transfer work counts in the transfer cgroup, so `git` joins it before it execs.
    #[test]
    fn r337_git_children_join_the_transfer_cgroup_before_they_exec() {
        let dir = tempfile::tempdir().unwrap();
        let procs = dir.path().join("cgroup.procs");
        std::fs::write(&procs, "").unwrap();
        let t = GixTransfer::new().with_cgroup_procs(Some(procs.clone()));
        let mut c = t.cmd(dir.path());
        c.arg("--version");
        let out = t.run("version", c).unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("git version"));
        let pid: i32 = std::fs::read_to_string(&procs)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid > 1);
        // without a cgroup nothing is written anywhere
        let plain = GixTransfer::new();
        let mut c = plain.cmd(dir.path());
        c.arg("--version");
        assert!(plain.run("version", c).is_ok());
    }
}
