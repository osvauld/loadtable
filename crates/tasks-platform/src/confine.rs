//! Confinement of jobs that come from another machine (R-821).
//!
//! A paired machine may run builds here, not read the owner's files. A confined job can read and
//! write only the folders LoadTable owns (the build cache, the synced copies, temp) and the
//! toolchains, and read the system; the rest of the home folder, other mounted volumes and
//! `/root` cannot be opened at all. The OS enforces it, not the job's good manners:
//!
//! - Linux: Landlock, applied by a small helper (`loadtabled confine`) that restricts itself and
//!   then execs the job, so the restriction holds for every process the job starts.
//! - macOS: Seatbelt, by running the job under `/usr/bin/sandbox-exec` with a generated profile.
//!
//! Neither needs root. When the confinement cannot be applied the job fails; it never runs
//! unconfined.

use std::path::{Path, PathBuf};

/// The macOS sandbox runner.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// What a confined job may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confine {
    /// The argv prefix of the Linux helper (`[".../loadtabled", "confine"]`); unused on macOS.
    pub helper: Vec<String>,
    /// Folders whose contents are closed: the home folder and other people's data.
    pub closed: Vec<PathBuf>,
    /// Read and write.
    pub write: Vec<PathBuf>,
    /// Read only.
    pub read: Vec<PathBuf>,
}

/// The paths of a parsed helper command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    /// Closed folders.
    pub closed: Vec<PathBuf>,
    /// Read and write.
    pub write: Vec<PathBuf>,
    /// Read only.
    pub read: Vec<PathBuf>,
    /// The job's command.
    pub argv: Vec<String>,
}

/// The command that runs `argv` confined on this OS.
pub fn wrap(c: &Confine, argv: &[String]) -> Vec<String> {
    if cfg!(target_os = "macos") {
        wrap_macos(c, argv)
    } else {
        wrap_helper(c, argv)
    }
}

/// `helper --closed P.. --write P.. --read P.. -- argv`.
pub fn wrap_helper(c: &Confine, argv: &[String]) -> Vec<String> {
    let mut out = c.helper.clone();
    for (flag, paths) in [
        ("--closed", &c.closed),
        ("--write", &c.write),
        ("--read", &c.read),
    ] {
        for p in paths {
            out.push(flag.to_string());
            out.push(p.display().to_string());
        }
    }
    out.push("--".to_string());
    out.extend(argv.iter().cloned());
    out
}

/// Parses the helper's arguments (after `confine`).
pub fn parse_helper_args(args: &[String]) -> Result<HelperArgs, String> {
    let mut h = HelperArgs {
        closed: Vec::new(),
        write: Vec::new(),
        read: Vec::new(),
        argv: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let list = match a.as_str() {
            "--closed" => &mut h.closed,
            "--write" => &mut h.write,
            "--read" => &mut h.read,
            "--" => {
                h.argv = it.cloned().collect();
                break;
            }
            other => return Err(format!("confine: unknown argument {other:?}")),
        };
        let v = it
            .next()
            .ok_or_else(|| format!("confine: {a} needs a path"))?;
        list.push(PathBuf::from(v));
    }
    if h.argv.is_empty() {
        return Err("confine: no command after --".to_string());
    }
    if h.closed.is_empty() {
        return Err("confine: nothing to close (--closed)".to_string());
    }
    Ok(h)
}

/// `sandbox-exec -p PROFILE argv`.
pub fn wrap_macos(c: &Confine, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        SANDBOX_EXEC.to_string(),
        "-p".to_string(),
        seatbelt_profile(c),
    ];
    out.extend(argv.iter().cloned());
    out
}

fn sb_str(p: &Path) -> String {
    let s = p.display().to_string();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The Seatbelt profile: everything is allowed except opening anything inside a closed folder;
/// the owned folders inside it are opened again. A later rule wins over an earlier one.
pub fn seatbelt_profile(c: &Confine) -> String {
    let list = |ps: &[PathBuf], form: &str| -> String {
        ps.iter()
            .map(|p| format!(" ({form} {})", sb_str(p)))
            .collect()
    };
    let mut s = String::from("(version 1)\n(allow default)\n");
    s.push_str(&format!(
        "(deny file-read* file-write*{})\n",
        list(&c.closed, "subpath")
    ));
    // a path walk and getcwd stat the closed folder itself and every folder between it and an
    // opened one (`realpath` of the synced copy stats ~/.local and ~/.local/state); their
    // listings stay closed
    let mut stat = c.closed.clone();
    stat.extend(between(&c.closed, c.read.iter().chain(&c.write)));
    s.push_str(&format!(
        "(allow file-read-metadata{})\n",
        list(&stat, "literal")
    ));
    if !c.read.is_empty() {
        s.push_str(&format!("(allow file-read*{})\n", list(&c.read, "subpath")));
    }
    if !c.write.is_empty() {
        s.push_str(&format!(
            "(allow file-read* file-write*{})\n",
            list(&c.write, "subpath")
        ));
    }
    s
}

/// The folders strictly between a closed folder and an opened path inside it, each once, in
/// order: for `/h` closed and `/h/.local/state/x` opened, `/h/.local` and `/h/.local/state`.
fn between<'a>(closed: &[PathBuf], opened: impl Iterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in opened {
        let Some(root) = closed
            .iter()
            .filter(|c| p.starts_with(c))
            .max_by_key(|c| c.as_os_str().len())
        else {
            continue;
        };
        let mut chain: Vec<PathBuf> = p
            .ancestors()
            .skip(1)
            .take_while(|a| *a != root.as_path())
            .map(Path::to_path_buf)
            .collect();
        chain.reverse();
        for a in chain {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

/// The folders under `root` a Linux job may read: everything except the closed folders, found by
/// walking down only the folders that lead to one. Landlock grants by hierarchy, so "all but X"
/// is every sibling of X and of each of its ancestors.
pub fn open_siblings(root: &Path, closed: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, closed, &mut out);
    out
}

fn walk(dir: &Path, closed: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for p in names {
        if closed.iter().any(|c| c == &p) {
            continue;
        }
        if closed.iter().any(|c| c.starts_with(&p)) {
            // a real folder leading to a closed one: open its other children
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                walk(&p, closed, out);
            }
            continue;
        }
        out.push(p);
    }
}

/// System folders a job may write besides the owned ones.
pub const SYSTEM_WRITABLE: &[&str] = &["/tmp", "/var/tmp", "/dev"];

/// Applies Landlock to this process: read the system and `read`, read and write `write` and
/// the temp folders, nothing inside `closed`. Every process started afterwards inherits it.
#[cfg(target_os = "linux")]
pub fn restrict(h: &HelperArgs) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };
    let abi = ABI::V5;
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);
    let mut readable = open_siblings(Path::new("/"), &h.closed);
    readable.extend(h.read.iter().cloned());
    let mut writable: Vec<PathBuf> = SYSTEM_WRITABLE
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    writable.extend(h.write.iter().cloned());
    let err = |e: landlock::RulesetError| e.to_string();
    let status = Ruleset::default()
        .handle_access(all)
        .map_err(err)?
        .create()
        .map_err(err)?
        .add_rules(path_beneath_rules(&readable, read))
        .map_err(err)?
        .add_rules(path_beneath_rules(&writable, all))
        .map_err(err)?
        .restrict_self()
        .map_err(err)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(
            "this kernel does not enforce Landlock (Linux 5.13 or later with \
                    Landlock enabled is needed)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn c() -> Confine {
        Confine {
            helper: vec!["/bin/loadtabled".into(), "confine".into()],
            closed: vec![PathBuf::from("/home/u")],
            write: vec![PathBuf::from("/home/u/build-cache")],
            read: vec![PathBuf::from("/home/u/.rustup")],
        }
    }

    #[test]
    fn r821_the_helper_line_round_trips_and_the_command_comes_last() {
        let argv = vec!["cargo".to_string(), "--".to_string(), "x".to_string()];
        let line = wrap_helper(&c(), &argv);
        assert_eq!(&line[..2], ["/bin/loadtabled", "confine"]);
        let back = parse_helper_args(&line[2..]).unwrap();
        assert_eq!(back.closed, c().closed);
        assert_eq!(back.write, c().write);
        assert_eq!(back.read, c().read);
        assert_eq!(
            back.argv, argv,
            "a -- inside the command stays the command's"
        );
    }

    #[test]
    fn r821_the_helper_refuses_a_line_that_closes_nothing_or_runs_nothing() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(parse_helper_args(&s(&["--closed", "/h"])).is_err());
        assert!(parse_helper_args(&s(&["--", "true"])).is_err());
        assert!(parse_helper_args(&s(&["--closed"])).is_err());
        assert!(parse_helper_args(&s(&["--bogus", "x", "--", "true"])).is_err());
    }

    #[test]
    fn r821_the_seatbelt_profile_closes_home_and_reopens_only_the_owned_folders() {
        let p = seatbelt_profile(&c());
        let deny = p
            .find("(deny file-read* file-write* (subpath \"/home/u\"))")
            .unwrap();
        let ro = p
            .find("(allow file-read* (subpath \"/home/u/.rustup\"))")
            .unwrap();
        let rw = p
            .find("(allow file-read* file-write* (subpath \"/home/u/build-cache\"))")
            .unwrap();
        assert!(
            deny < ro && deny < rw,
            "the reopenings come after the denial: {p}"
        );
        assert!(p.contains("(allow file-read-metadata (literal \"/home/u\"))"));
        let w = wrap_macos(&c(), &["make".to_string()]);
        assert_eq!(w[0], SANDBOX_EXEC);
        assert_eq!(w[1], "-p");
        assert_eq!(w[3], "make");
    }

    #[test]
    fn r831_the_folders_above_an_opened_one_can_be_looked_up_but_not_listed() {
        let mut x = c();
        x.write = vec![
            PathBuf::from("/home/u/.local/state/loadtable/peer-store"),
            PathBuf::from("/home/u/build-cache"),
        ];
        x.read = vec![PathBuf::from("/home/u/Library/Caches/swiftpm")];
        let p = seatbelt_profile(&x);
        let stat = p
            .lines()
            .find(|l| l.starts_with("(allow file-read-metadata"))
            .unwrap();
        for dir in [
            "/home/u",
            "/home/u/.local",
            "/home/u/.local/state",
            "/home/u/.local/state/loadtable",
            "/home/u/Library",
            "/home/u/Library/Caches",
        ] {
            assert!(
                stat.contains(&format!("(literal \"{dir}\")")),
                "{dir} must be looked up for realpath: {stat}"
            );
        }
        assert!(
            !stat.contains("subpath"),
            "only the folders themselves, never what is under them: {stat}"
        );
        assert_eq!(
            stat.matches("(literal \"/home/u/.local\")").count(),
            1,
            "each folder once"
        );
        assert!(
            !p.contains("(allow file-read* (literal \"/home/u/.local\"))"),
            "a folder above is not listed: {p}"
        );
    }

    #[test]
    fn r831_paths_outside_the_closed_folders_add_no_lookups() {
        let x = Confine {
            helper: vec![],
            closed: vec![PathBuf::from("/home/u")],
            write: vec![
                PathBuf::from("/opt/sdk/tools"),
                PathBuf::from("/home/u/.cargo"),
            ],
            read: vec![],
        };
        let stat = seatbelt_profile(&x)
            .lines()
            .find(|l| l.starts_with("(allow file-read-metadata"))
            .unwrap()
            .to_string();
        assert_eq!(stat, "(allow file-read-metadata (literal \"/home/u\"))");
    }

    #[test]
    fn r821_profile_paths_are_quoted() {
        let mut x = c();
        x.closed = vec![PathBuf::from("/Users/a \"b\"")];
        assert!(seatbelt_profile(&x).contains("(subpath \"/Users/a \\\"b\\\"\")"));
    }

    #[test]
    fn r821_every_folder_but_the_closed_ones_is_opened() {
        let r = &std::env::temp_dir().join(format!("tasks-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(r);
        for d in [
            "usr/bin",
            "home/u/photos",
            "home/u/build-cache",
            "home/v",
            "etc",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        std::fs::write(r.join("home/u/notes.txt"), "x").unwrap();
        let open = open_siblings(r, &[r.join("home/u")]);
        assert_eq!(open, vec![r.join("etc"), r.join("home/v"), r.join("usr")]);
        let _ = std::fs::remove_dir_all(r);
    }
}
