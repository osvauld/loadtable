//! Moving a `tasks2` install to the LoadTable names on the daemon's first start (R-804).
//!
//! The state directory holds the ledger and the machine identity, the config directory the
//! machine policy, and `<cache root>/tasks2/checkouts` the warm build outputs that worktrees
//! link to. Each is renamed to its `loadtable` name in place (same parent, so one atomic
//! rename), and a symlink is left at the old path: links already pointing into the old cache
//! still resolve, and a stray daemon of the old name finds the same pid lock instead of a fresh
//! state with a fresh identity.
//!
//! Only default locations move. A directory given by flag or environment is used as it is.
//! Nothing moves while a daemon still holds the old state directory's pid lock.

use crate::config::{ConfigArgs, Homes, PidLock};
use crate::error::DaemonError;
use std::path::Path;
use tasks_core::names::{DIR_NAME, OLD_DIR_NAME};

/// Moves the default state and config directories; returns what was done, for the log.
pub fn move_default_dirs(
    args: &ConfigArgs,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, DaemonError> {
    let homes = Homes::of(env);
    let mut notes = Vec::new();
    let state_given = args.state_dir.is_some() || env("LOADTABLE_STATE_DIR").is_some();
    if let (false, Some(base)) = (state_given, &homes.state) {
        let old = base.join(OLD_DIR_NAME);
        if needs_move(&old, &base.join(DIR_NAME)) {
            // a running daemon of the old name keeps its files open: never move them under it
            drop(PidLock::acquire(&old.join("daemon.pid"))?);
        }
        notes.extend(move_dir(&old, &base.join(DIR_NAME))?);
    }
    let config_given = args.config_dir.is_some() || env("LOADTABLE_CONFIG_DIR").is_some();
    if let (false, Some(base)) = (config_given, &homes.config) {
        notes.extend(move_dir(&base.join(OLD_DIR_NAME), &base.join(DIR_NAME))?);
    }
    Ok(notes)
}

/// Moves `<cache root>/tasks2/checkouts` to `<cache root>/loadtable/checkouts`.
pub fn move_cache_outputs(cache_root: &Path) -> Result<Option<String>, DaemonError> {
    let new = cache_root.join(DIR_NAME).join("checkouts");
    if let Some(parent) = new.parent()
        && needs_move(&cache_root.join(OLD_DIR_NAME).join("checkouts"), &new)
    {
        std::fs::create_dir_all(parent).map_err(|e| DaemonError::io(parent, e))?;
    }
    move_dir(&cache_root.join(OLD_DIR_NAME).join("checkouts"), &new)
}

/// True when `old` is a real directory and `new` does not exist yet.
fn needs_move(old: &Path, new: &Path) -> bool {
    std::fs::symlink_metadata(old).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(new).is_err()
}

/// Renames `old` to `new` and links `old` to `new`, when `old` is a real directory and `new`
/// does not exist. Leaves both alone otherwise.
fn move_dir(old: &Path, new: &Path) -> Result<Option<String>, DaemonError> {
    if !needs_move(old, new) {
        let both = std::fs::symlink_metadata(old).is_ok_and(|m| m.is_dir()) && new.exists();
        return Ok(both.then(|| {
            format!(
                "{} and {} both exist; using {}, the old one is left as it is",
                old.display(),
                new.display(),
                new.display()
            )
        }));
    }
    std::fs::rename(old, new).map_err(|e| DaemonError::io(old, e))?;
    std::os::unix::fs::symlink(new, old).map_err(|e| DaemonError::io(old, e))?;
    Ok(Some(format!(
        "moved {} to {} (the old path links to it)",
        old.display(),
        new.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("loadtable-r804-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn env_home(home: &Path) -> impl Fn(&str) -> Option<String> + use<> {
        let home = home.display().to_string();
        move |k| (k == "HOME").then(|| home.clone())
    }

    #[test]
    fn r804_the_tasks2_state_and_config_move_to_loadtable_with_their_identity() {
        let home = temp("dirs");
        let state = home.join(".local/state");
        let config = home.join(".config");
        std::fs::create_dir_all(state.join("tasks2/slots")).unwrap();
        std::fs::write(state.join("tasks2/identity.json"), "key").unwrap();
        std::fs::create_dir_all(config.join("tasks2")).unwrap();
        std::fs::write(config.join("tasks2/machine-policy.toml"), "x").unwrap();
        let env = env_home(&home);
        // before the move a client still finds the old state
        let c = DaemonConfig::resolve(&ConfigArgs::default(), &env).unwrap();
        assert_eq!(c.state_dir, state.join("tasks2"));
        let notes = move_default_dirs(&ConfigArgs::default(), &env).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(
            std::fs::read_to_string(state.join("loadtable/identity.json")).unwrap(),
            "key"
        );
        assert!(config.join("loadtable/machine-policy.toml").is_file());
        // the old paths link to the new ones
        assert!(state.join("tasks2").is_symlink());
        assert_eq!(
            std::fs::read_to_string(state.join("tasks2/identity.json")).unwrap(),
            "key"
        );
        let c = DaemonConfig::resolve(&ConfigArgs::default(), &env).unwrap();
        assert_eq!(c.state_dir, state.join("loadtable"));
        assert_eq!(c.config_dir, config.join("loadtable"));
        // a second start has nothing to do
        assert!(
            move_default_dirs(&ConfigArgs::default(), &env)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn r804_nothing_moves_under_a_running_daemon_or_out_of_a_given_directory() {
        let home = temp("held");
        let old = home.join(".local/state/tasks2");
        std::fs::create_dir_all(&old).unwrap();
        let env = env_home(&home);
        let held = PidLock::acquire(&old.join("daemon.pid")).unwrap();
        let e = move_default_dirs(&ConfigArgs::default(), &env).unwrap_err();
        assert!(matches!(e, DaemonError::AlreadyRunning { .. }), "{e:?}");
        assert!(old.is_dir() && !old.is_symlink());
        drop(held);
        // a state directory given explicitly is never moved
        let args = ConfigArgs {
            state_dir: Some(old.clone()),
            ..ConfigArgs::default()
        };
        assert!(move_default_dirs(&args, &env).unwrap().is_empty());
        assert!(!old.is_symlink());
        // with both present the new one wins and the old one is left
        std::fs::create_dir_all(home.join(".local/state/loadtable")).unwrap();
        let notes = move_default_dirs(&ConfigArgs::default(), &env).unwrap();
        assert!(notes[0].contains("both exist"), "{notes:?}");
        assert!(!old.is_symlink());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn r804_warm_outputs_move_and_links_into_the_old_cache_still_resolve() {
        let root = temp("cache");
        let old = root.join("tasks2/checkouts/abc/target");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("built"), "1").unwrap();
        let worktree = root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::os::unix::fs::symlink(&old, worktree.join("target")).unwrap();
        let note = move_cache_outputs(&root).unwrap();
        assert!(note.is_some());
        assert!(root.join("loadtable/checkouts/abc/target/built").is_file());
        assert_eq!(
            std::fs::read_to_string(worktree.join("target/built")).unwrap(),
            "1"
        );
        assert_eq!(move_cache_outputs(&root).unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
