//! `loadtable gc` (R-839): states, ownership through links, and what a deletion touches.

use super::*;
use crate::api::{Reply, Request, Response};
use crate::build_cache::BuildCache;
use crate::forwarder::Forwarder;
use crate::testkit::{FakeForwarder, Rig, TempDir};
use std::fs;
use std::sync::Arc;
use tasks_core::Kind;

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository on `dev` with one commit, canonical.
fn repo(at: &Path) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    let at = fs::canonicalize(at).unwrap();
    git_ok(&at, &["init", "-q", "-b", "dev"]);
    fs::write(at.join("Cargo.toml"), "[workspace]\n").unwrap();
    git_ok(&at, &["add", "."]);
    git_ok(&at, &["commit", "-q", "-m", "one"]);
    at
}

/// A worktree of `repo` at `name` on a new branch `name`, with one commit of its own when
/// `ahead`.
fn worktree(repo: &Path, name: &str, ahead: bool) -> PathBuf {
    let wt = repo.parent().unwrap().join(name);
    git_ok(
        repo,
        &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()],
    );
    if ahead {
        fs::write(wt.join("more.txt"), "x").unwrap();
        git_ok(&wt, &["add", "."]);
        git_ok(&wt, &["commit", "-q", "-m", "more"]);
    }
    wt
}

/// `bytes` of output in `dir/target`.
fn output(dir: &Path, bytes: usize) -> PathBuf {
    fs::create_dir_all(dir.join("target")).unwrap();
    fs::write(dir.join("target/blob"), vec![0u8; bytes]).unwrap();
    dir.to_path_buf()
}

#[test]
fn r839_a_branch_in_dev_with_no_changed_file_is_merged_and_the_base_or_a_newer_one_is_not() {
    let t = TempDir::new("gc-merged");
    let main = repo(&t.path().join("main"));
    let done = worktree(&main, "done", false);
    let wip = worktree(&main, "wip", true);
    let dirty = worktree(&main, "dirty", false);
    fs::write(dirty.join("Cargo.toml"), "[workspace]\n# edit\n").unwrap();

    assert_eq!(branch_of(&main), (Some("dev".into()), None));
    assert_eq!(branch_of(&done), (Some("done".into()), Some("dev".into())));
    assert_eq!(branch_of(&wip), (Some("wip".into()), None));
    assert_eq!(branch_of(&dirty), (Some("dirty".into()), None));
    let mut all = worktrees(&main);
    all.sort();
    let mut want = vec![main, done, wip, dirty];
    want.sort();
    assert_eq!(all, want);
}

#[test]
fn r839_output_belongs_to_the_checkout_its_links_point_from_also_in_the_older_layout() {
    let t = TempDir::new("gc-links");
    let base = fs::canonicalize(t.path()).unwrap();
    let cache = base.join("cache");
    let new = cache.join(crate::build_cache::CHECKOUTS_SUBDIR);
    let old = cache.join("p2p_chat/tasks/checkouts");
    fs::create_dir_all(&new).unwrap();
    fs::create_dir_all(&old).unwrap();
    // the current layout: found by its key alone
    let a = base.join("a");
    fs::create_dir_all(a.join(".git")).unwrap();
    let a_out = output(&new.join(key_of(&a)), 10);
    // an older layout under another key: found through the link
    let b = base.join("b");
    fs::create_dir_all(b.join(".git")).unwrap();
    let b_out = output(&old.join("ab".repeat(32)), 10);
    std::os::unix::fs::symlink(b_out.join("target"), b.join("target")).unwrap();

    let parents = output_parents(&cache);
    assert_eq!(
        parents,
        vec![new.clone(), old.clone()]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_eq!(owned_dirs(&a, &cache, &parents), vec![a_out]);
    assert_eq!(owned_dirs(&b, &cache, &parents), vec![b_out]);
    // a link out of the cache claims nothing there
    let c = base.join("c");
    fs::create_dir_all(c.join(".git")).unwrap();
    fs::create_dir_all(base.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(base.join("elsewhere"), c.join("target")).unwrap();
    assert!(owned_dirs(&c, &cache, &parents).is_empty());
}

#[test]
fn r839_a_busy_checkout_is_in_use_whatever_else_holds() {
    let day = DAY_MS;
    assert_eq!(
        state_of(true, false, true, Some(0), 99 * day, 14),
        GcState::InUse
    );
    assert_eq!(
        state_of(false, false, true, Some(0), 99 * day, 14),
        GcState::Gone
    );
    assert_eq!(
        state_of(false, true, true, Some(0), 99 * day, 14),
        GcState::Merged
    );
    assert_eq!(
        state_of(false, true, false, Some(0), 15 * day, 14),
        GcState::Idle
    );
    assert_eq!(
        state_of(false, true, false, Some(0), 13 * day, 14),
        GcState::Active
    );
    assert_eq!(
        state_of(false, true, false, None, 99 * day, 14),
        GcState::Active
    );
}

fn gc(r: &Rig, delete: Vec<GcState>) -> GcView {
    gc_named(r, delete, vec![])
}

fn gc_named(r: &Rig, delete: Vec<GcState>, checkouts: Vec<String>) -> GcView {
    match r.sch.handle(Request::Gc {
        delete,
        checkouts,
        idle_days: None,
    }) {
        Reply::Ok(Response::Gc(v)) => *v,
        other => panic!("unexpected {other:?}"),
    }
}

fn row<'a>(v: &'a GcView, checkout: &str) -> &'a GcRow {
    v.rows
        .iter()
        .find(|r| r.checkout.ends_with(checkout))
        .unwrap_or_else(|| panic!("no row {checkout} in {:?}", v.rows))
}

fn on(r: &GcRow, machine: &str) -> u64 {
    r.held
        .iter()
        .filter(|h| h.machine == machine)
        .map(|h| h.bytes)
        .sum()
}

#[test]
fn r839_gc_reports_every_machine_and_frees_only_the_chosen_states_never_a_busy_checkout() {
    let fw = Arc::new(FakeForwarder::new(vec![crate::tests::peer_view()]));
    let r = Rig::with_forwarder(fw.clone());
    let cache_root = r.cfg.cache_dir.clone();
    fs::create_dir_all(&cache_root).unwrap();
    r.sch.set_build_cache(BuildCache::discover(
        &cache_root,
        &r.cfg.state_dir.join("peer-store"),
        &[],
    ));
    let cache = fs::canonicalize(&cache_root).unwrap();
    let new = cache.join(crate::build_cache::CHECKOUTS_SUBDIR);
    let main = repo(&r.dir.path().join("work/main"));
    let done = worktree(&main, "done", false);
    let busy = worktree(&main, "busy", true);
    let mine = worktree(&main, "mine", true);
    let done_out = output(&new.join(key_of(&done)), 1000);
    let busy_out = output(&new.join(key_of(&busy)), 2000);
    let mine_out = output(&new.join(key_of(&mine)), 3000);
    let gone_out = output(&new.join("a".repeat(64)), 4000);
    let old_out = output(
        &cache.join("p2p_chat/tasks/checkouts").join("b".repeat(64)),
        5000,
    );
    // a queued job keeps `busy` in use; the repository's other worktrees come from Git
    let mut spec = r.spec("o", &["cargo", "build"], Kind::Compile);
    spec.cwd = busy.clone();
    r.submit(&spec);

    let id = |p: &Path| fw.checkout_id(p).unwrap();
    let held = |c: CheckoutId, busy: bool| HeldCheckout {
        checkout: c,
        copy_bytes: 100,
        output_bytes: 50,
        last_used_ms: Some(1),
        busy,
    };
    let stranger = CheckoutId::parse(&"c".repeat(32)).unwrap();
    fw.cache.lock().unwrap().insert(
        "nixos".into(),
        vec![
            held(id(&done), false),
            held(id(&busy), true),
            held(stranger.clone(), false),
        ],
    );

    let v = gc(&r, vec![]);
    assert!(!v.deleted);
    assert_eq!(row(&v, "/done").state, GcState::Merged);
    assert_eq!(row(&v, "/done").merged_into.as_deref(), Some("dev"));
    assert!(on(row(&v, "/done"), "testbox") >= 1000);
    assert_eq!(on(row(&v, "/done"), "nixos"), 150);
    assert_eq!(row(&v, "/busy").state, GcState::InUse);
    assert_eq!(row(&v, "/mine").state, GcState::Active);
    assert_eq!(
        row(&v, "(unknown checkout aaaaaaaaaaaa)").state,
        GcState::Gone
    );
    assert_eq!(
        row(&v, "(unknown checkout bbbbbbbbbbbb)").state,
        GcState::Old
    );
    assert_eq!(
        row(&v, "(unknown checkout cccccccccccc)").state,
        GcState::Gone
    );
    let nixos = v.machines.iter().find(|m| m.name == "nixos").unwrap();
    assert_eq!(
        (
            nixos.held_bytes,
            nixos.reclaimable_bytes,
            nixos.error.clone()
        ),
        (450, 300, None)
    );
    assert!(
        v.machines
            .iter()
            .any(|m| m.name == "testbox" && m.reclaimable_bytes >= 10_000)
    );

    // asking to free in-use rows too changes nothing for them
    let v = gc(
        &r,
        vec![GcState::Merged, GcState::Gone, GcState::Old, GcState::InUse],
    );
    assert!(v.deleted);
    assert!(!done_out.exists() && !gone_out.exists() && !old_out.exists());
    assert!(busy_out.exists() && mine_out.exists());
    assert!(
        done.join("Cargo.toml").exists(),
        "a working tree is never deleted"
    );
    assert!(row(&v, "/done").freed_bytes >= 1150);
    let dropped = fw.dropped.lock().unwrap().clone();
    assert_eq!(dropped.len(), 1);
    let (peer, ids) = &dropped[0];
    assert_eq!(peer, "nixos");
    assert!(ids.contains(&id(&done)) && ids.contains(&stranger) && !ids.contains(&id(&busy)));
}

#[test]
fn r839_a_peer_that_cannot_answer_is_named_and_the_rest_still_reports() {
    let fw = Arc::new(FakeForwarder::new(vec![crate::tests::peer_view()]));
    let r = Rig::with_forwarder(fw);
    let v = gc(&r, vec![]);
    let nixos = v.machines.iter().find(|m| m.name == "nixos").unwrap();
    assert_eq!(nixos.error.as_deref(), Some("nixos unreachable"));
    assert!(
        v.machines
            .iter()
            .any(|m| m.name == "testbox" && m.error.is_none())
    );
}

#[test]
fn r839_peer_outputs_are_listed_per_checkout_and_dropped_with_it() {
    use crate::peer_link::serve::PeerOutputs;
    use tasks_peer::{OutputKeeper, PeerId};
    let t = TempDir::new("gc-peer-outputs");
    let peer = PeerId::new("laptop-node").unwrap();
    let a = CheckoutId::parse(&"a".repeat(32)).unwrap();
    let b = CheckoutId::parse(&"b".repeat(32)).unwrap();
    let dir = t.path().join("peer");
    output(&dir.join(peer.as_str()).join(a.as_str()), 300);
    output(&dir.join(peer.as_str()).join(b.as_str()), 700);
    fs::create_dir_all(dir.join(peer.as_str()).join("not-a-checkout")).unwrap();
    let keep = PeerOutputs::at(dir.clone());
    let mut listed = keep.outputs(&peer);
    listed.sort();
    assert_eq!(listed, vec![(a.clone(), 300), (b.clone(), 700)]);
    assert_eq!(keep.drop_outputs(&peer, &a), 300);
    assert!(!dir.join(peer.as_str()).join(a.as_str()).exists());
    assert_eq!(keep.outputs(&peer), vec![(b, 700)]);
    assert!(keep.outputs(&PeerId::new("other-node").unwrap()).is_empty());
}

#[test]
fn r840_gc_frees_the_checkouts_named_whatever_their_state_but_never_one_a_job_uses() {
    let r = Rig::with_forwarder(Arc::new(FakeForwarder::new(vec![])));
    let cache_root = r.cfg.cache_dir.clone();
    fs::create_dir_all(&cache_root).unwrap();
    r.sch.set_build_cache(BuildCache::discover(
        &cache_root,
        &r.cfg.state_dir.join("peer-store"),
        &[],
    ));
    let new = fs::canonicalize(&cache_root)
        .unwrap()
        .join(crate::build_cache::CHECKOUTS_SUBDIR);
    let main = repo(&r.dir.path().join("work/main"));
    let done = worktree(&main, "done", false);
    let busy = worktree(&main, "busy", true);
    let mine = worktree(&main, "mine", true);
    let done_out = output(&new.join(key_of(&done)), 1000);
    let busy_out = output(&new.join(key_of(&busy)), 2000);
    let mine_out = output(&new.join(key_of(&mine)), 3000);
    let mut spec = r.spec("o", &["cargo", "build"], Kind::Compile);
    spec.cwd = busy.clone();
    r.submit(&spec);

    let v = gc(&r, vec![]);
    let name = |c: &str| row(&v, c).checkout.clone();
    let v = gc_named(
        &r,
        vec![],
        vec![
            mine.display().to_string(),
            name("/busy"),
            "~/nowhere".into(),
        ],
    );
    assert!(v.deleted);
    assert!(!mine_out.exists(), "an active checkout named is freed");
    assert!(done_out.exists(), "a merged one not named stays");
    assert!(busy_out.exists(), "one a job uses stays");
    assert!(row(&v, "/mine").freed_bytes >= 3000);
    assert_eq!(
        row(&v, "/busy").kept.as_deref(),
        Some("a job uses it; free it once the job ends")
    );
    assert_eq!(
        v.notes,
        vec!["~/nowhere holds no build output here; run loadtable gc to see what does".to_string()]
    );
    assert!(mine.join("Cargo.toml").exists());
}
