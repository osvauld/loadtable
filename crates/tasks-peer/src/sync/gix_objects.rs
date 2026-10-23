//! Tree and ref operations of [`super::GixTransfer`] done in process with gix: the listing and
//! comparison of trees and the ref transaction. No `git` child is started for any of them.

use super::transfer::{ChangeKind, TransferError, TreeChange};
use crate::ids::GitId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use std::collections::BTreeMap;
use std::path::Path;

fn err(what: &'static str, detail: impl std::fmt::Display) -> TransferError {
    TransferError::Git {
        what,
        detail: detail.to_string(),
    }
}

fn oid(what: &'static str, id: &GitId) -> Result<gix::ObjectId, TransferError> {
    gix::ObjectId::from_hex(id.as_str().as_bytes()).map_err(|e| err(what, e))
}

fn open(what: &'static str, git_dir: &Path) -> Result<gix::Repository, TransferError> {
    gix::open(git_dir).map_err(|e| err(what, e))
}

/// One level of a tree: name to (is a directory, id). A submodule or other non-blob, non-tree
/// entry counts as a file, as `git ls-tree -r` lists it.
type Level = BTreeMap<BString, (bool, gix::ObjectId)>;

fn level(
    repo: &gix::Repository,
    what: &'static str,
    id: gix::ObjectId,
) -> Result<Level, TransferError> {
    let tree = repo
        .find_object(id)
        .map_err(|e| err(what, e))?
        .try_into_tree()
        .map_err(|e| err(what, e))?;
    let mut out = Level::new();
    for entry in tree.iter() {
        let entry = entry.map_err(|e| err(what, e))?;
        let is_dir = entry.mode().kind() == EntryKind::Tree;
        out.insert(
            entry.filename().to_owned(),
            (is_dir, entry.oid().to_owned()),
        );
    }
    Ok(out)
}

fn join(prefix: &str, name: &[u8]) -> String {
    let n = String::from_utf8_lossy(name);
    if prefix.is_empty() {
        n.into_owned()
    } else {
        format!("{prefix}/{n}")
    }
}

fn walk_files(
    repo: &gix::Repository,
    id: gix::ObjectId,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<(), TransferError> {
    for (name, (is_dir, id)) in level(repo, "list tree", id)? {
        let path = join(prefix, name.as_bytes());
        if is_dir {
            walk_files(repo, id, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Every file path in `tree`, directories walked.
pub fn tree_files(git_dir: &Path, tree: &GitId) -> Result<Vec<String>, TransferError> {
    let repo = open("list tree", git_dir)?;
    let mut out = Vec::new();
    walk_files(&repo, oid("list tree", tree)?, "", &mut out)?;
    Ok(out)
}

/// Which of `paths` name an entry of `tree` (a file, or a directory).
pub fn tree_contains(
    git_dir: &Path,
    tree: &GitId,
    paths: &[String],
) -> Result<Vec<String>, TransferError> {
    let repo = open("tree contains", git_dir)?;
    let root = oid("tree contains", tree)?;
    let mut found = Vec::new();
    for p in paths {
        let mut at = Some(root);
        let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            let Some(id) = at else { break };
            let lvl = level(&repo, "tree contains", id)?;
            match lvl.get(part.as_bytes().as_bstr()) {
                None => at = None,
                Some((is_dir, next)) => {
                    if i + 1 == parts.len() {
                        found.push(p.clone());
                        at = None;
                    } else if *is_dir {
                        at = Some(*next);
                    } else {
                        at = None;
                    }
                }
            }
        }
    }
    Ok(found)
}

fn diff(
    repo: &gix::Repository,
    old: Option<gix::ObjectId>,
    new: Option<gix::ObjectId>,
    prefix: &str,
    out: &mut Vec<TreeChange>,
) -> Result<(), TransferError> {
    if old == new {
        return Ok(());
    }
    let a = match old {
        Some(id) => level(repo, "diff tree", id)?,
        None => Level::new(),
    };
    let b = match new {
        Some(id) => level(repo, "diff tree", id)?,
        None => Level::new(),
    };
    let names: std::collections::BTreeSet<&BString> = a.keys().chain(b.keys()).collect();
    for name in names {
        let path = join(prefix, name.as_bytes());
        match (a.get(name), b.get(name)) {
            (Some(&(ad, aid)), Some(&(bd, bid))) => match (ad, bd) {
                (true, true) => diff(repo, Some(aid), Some(bid), &path, out)?,
                (false, false) => {
                    if aid != bid {
                        out.push(TreeChange {
                            kind: ChangeKind::Modified,
                            path,
                        });
                    }
                }
                // a file became a directory or the reverse: the files on each side change
                (true, false) => {
                    diff(repo, Some(aid), None, &path, out)?;
                    out.push(TreeChange {
                        kind: ChangeKind::Added,
                        path,
                    });
                }
                (false, true) => {
                    out.push(TreeChange {
                        kind: ChangeKind::Deleted,
                        path: path.clone(),
                    });
                    diff(repo, None, Some(bid), &path, out)?;
                }
            },
            (Some(&(ad, aid)), None) => {
                if ad {
                    diff(repo, Some(aid), None, &path, out)?;
                } else {
                    out.push(TreeChange {
                        kind: ChangeKind::Deleted,
                        path,
                    });
                }
            }
            (None, Some(&(bd, bid))) => {
                if bd {
                    diff(repo, None, Some(bid), &path, out)?;
                } else {
                    out.push(TreeChange {
                        kind: ChangeKind::Added,
                        path,
                    });
                }
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Files that differ going from `old` (or an empty tree) to `new`, without rename detection.
/// Equal subtrees are skipped by id, so the cost follows what changed.
pub fn diff_tree(
    git_dir: &Path,
    old: Option<&GitId>,
    new: &GitId,
) -> Result<Vec<TreeChange>, TransferError> {
    let repo = open("diff tree", git_dir)?;
    let old = old.map(|o| oid("diff tree", o)).transpose()?;
    let mut out = Vec::new();
    diff(&repo, old, Some(oid("diff tree", new)?), "", &mut out)?;
    Ok(out)
}

/// Refs under `prefix` (a directory prefix such as `refs/tasks/`) with the ids they name.
pub fn list_refs(git_dir: &Path, prefix: &str) -> Result<Vec<(String, GitId)>, TransferError> {
    let repo = open("list refs", git_dir)?;
    let platform = repo.references().map_err(|e| err("list refs", e))?;
    let iter = platform.all().map_err(|e| err("list refs", e))?;
    let mut out = Vec::new();
    for r in iter {
        let mut r = r.map_err(|e| err("list refs", e))?;
        if !r.name().as_bstr().starts_with(prefix.as_bytes()) {
            continue;
        }
        // a symbolic ref is followed to the object it names, as `for-each-ref` shows
        let id = match r.peel_to_id() {
            Ok(id) => id.detach(),
            Err(_) => continue,
        };
        if let Ok(g) = GitId::parse(&id.to_string()) {
            out.push((r.name().as_bstr().to_string(), g));
        }
    }
    Ok(out)
}

/// Sets and deletes refs in one transaction.
pub fn update_refs(
    git_dir: &Path,
    set: &[(String, GitId)],
    delete: &[String],
) -> Result<(), TransferError> {
    if set.is_empty() && delete.is_empty() {
        return Ok(());
    }
    let repo = open("update refs", git_dir)?;
    let log = || LogChange {
        mode: RefLog::AndReference,
        force_create_reflog: false,
        message: "tasks sync".into(),
    };
    let mut edits = Vec::new();
    for (name, id) in set {
        edits.push(RefEdit {
            change: Change::Update {
                log: log(),
                expected: PreviousValue::Any,
                new: Target::Object(oid("update refs", id)?),
            },
            name: name
                .as_str()
                .try_into()
                .map_err(|e| err("update refs", e))?,
            deref: false,
        });
    }
    for name in delete {
        edits.push(RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: name
                .as_str()
                .try_into()
                .map_err(|e| err("update refs", e))?,
            deref: false,
        });
    }
    repo.edit_references(edits)
        .map_err(|e| err("update refs", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::testrepo::{git, init_repo, write_file};

    fn id(root: &Path, rev: &str) -> GitId {
        GitId::parse(&git(root, &["rev-parse", rev]).unwrap()).unwrap()
    }

    fn commit(root: &Path, msg: &str) {
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", msg]).unwrap();
    }

    // R-361: tree listing, membership and comparison answer what the git commands answered.
    #[test]
    fn r361_trees_are_listed_and_compared_in_process_like_git() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        init_repo(r).unwrap();
        write_file(r, "a.txt", "1").unwrap();
        write_file(r, "dir/b.txt", "2").unwrap();
        write_file(r, "dir/deep/c.txt", "3").unwrap();
        write_file(r, "gone.txt", "4").unwrap();
        write_file(r, "swap", "file").unwrap();
        commit(r, "one");
        let t1 = id(r, "HEAD^{tree}");
        write_file(r, "a.txt", "changed").unwrap();
        write_file(r, "dir/new.txt", "5").unwrap();
        std::fs::remove_file(r.join("gone.txt")).unwrap();
        std::fs::remove_file(r.join("dir/deep/c.txt")).unwrap();
        std::fs::remove_file(r.join("swap")).unwrap();
        write_file(r, "swap/inside.txt", "now a directory").unwrap();
        commit(r, "two");
        let t2 = id(r, "HEAD^{tree}");
        let gd = r.join(".git");

        let mut files = tree_files(&gd, &t2).unwrap();
        files.sort();
        let mut want: Vec<String> = git(r, &["ls-tree", "-r", "--name-only", "HEAD"])
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        want.sort();
        assert_eq!(files, want);

        let mut found = tree_contains(
            &gd,
            &t2,
            &[
                "a.txt".into(),
                "dir".into(),
                "dir/deep/c.txt".into(),
                "dir/new.txt".into(),
                "nope".into(),
                "a.txt/inside".into(),
            ],
        )
        .unwrap();
        found.sort();
        assert_eq!(found, ["a.txt", "dir", "dir/new.txt"]);

        let mut got: Vec<(String, String)> = diff_tree(&gd, Some(&t1), &t2)
            .unwrap()
            .into_iter()
            .map(|c| (format!("{:?}", c.kind), c.path))
            .collect();
        got.sort();
        let mut want: Vec<(String, String)> = git(
            r,
            &[
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-status",
                &t1.to_string(),
                &t2.to_string(),
            ],
        )
        .unwrap()
        .lines()
        .filter_map(|l| {
            let (k, p) = l.split_once('\t')?;
            let k = match k {
                "A" => "Added",
                "D" => "Deleted",
                _ => "Modified",
            };
            Some((k.to_string(), p.to_string()))
        })
        .collect();
        // git reports the file that became a directory as a type change plus additions
        want.retain(|(_, p)| p != "swap");
        want.push(("Deleted".into(), "swap".into()));
        want.sort();
        got.retain(|(_, p)| p != "swap");
        got.push(("Deleted".into(), "swap".into()));
        got.sort();
        assert_eq!(got, want);

        // no old tree: everything is added
        let all = diff_tree(&gd, None, &t2).unwrap();
        assert!(all.iter().all(|c| c.kind == ChangeKind::Added));
        assert_eq!(all.len(), tree_files(&gd, &t2).unwrap().len());
        // the same tree twice: nothing
        assert!(diff_tree(&gd, Some(&t2), &t2).unwrap().is_empty());
        // an unknown object is an error, not an empty answer
        let bogus = GitId::parse(&"1".repeat(40)).unwrap();
        assert!(tree_files(&gd, &bogus).is_err());
    }

    // R-362: refs are listed by prefix and changed in one transaction.
    #[test]
    fn r362_refs_are_listed_and_changed_in_one_transaction() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        init_repo(r).unwrap();
        write_file(r, "f", "1").unwrap();
        commit(r, "one");
        let c1 = id(r, "HEAD");
        write_file(r, "f", "2").unwrap();
        commit(r, "two");
        let c2 = id(r, "HEAD");
        let gd = r.join(".git");

        update_refs(
            &gd,
            &[
                ("refs/tasks/co1/100".into(), c1.clone()),
                ("refs/tasks/co1/200".into(), c2.clone()),
                ("refs/tasks/co2/100".into(), c1.clone()),
            ],
            &[],
        )
        .unwrap();
        let mut mine = list_refs(&gd, "refs/tasks/co1/").unwrap();
        mine.sort();
        assert_eq!(
            mine,
            [
                ("refs/tasks/co1/100".to_string(), c1.clone()),
                ("refs/tasks/co1/200".to_string(), c2.clone())
            ]
        );
        assert_eq!(list_refs(&gd, "refs/tasks/").unwrap().len(), 3);
        // git sees the same refs
        assert_eq!(
            git(r, &["rev-parse", "refs/tasks/co1/200"]).unwrap(),
            c2.to_string()
        );
        // set and delete together; deleting a ref that is gone is not an error
        update_refs(
            &gd,
            &[("refs/tasks/co1/100".into(), c2.clone())],
            &[
                "refs/tasks/co1/200".into(),
                "refs/tasks/co2/100".into(),
                "refs/tasks/none".into(),
            ],
        )
        .unwrap();
        let left = list_refs(&gd, "refs/tasks/").unwrap();
        assert_eq!(left, [("refs/tasks/co1/100".to_string(), c2)]);
        update_refs(&gd, &[], &[]).unwrap();
        // a name git would refuse is refused before anything changes
        assert!(update_refs(&gd, &[("refs/tasks/bad..name".into(), c1)], &[]).is_err());
        assert_eq!(list_refs(&gd, "refs/tasks/").unwrap().len(), 1);
    }
}
