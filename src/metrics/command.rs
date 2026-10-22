//! Display command and history grouping key (`kind_key`) from a job's argv.
//!
//! Rules (same for `command` and `kind_key`):
//! - a leading `env [--] NAME=value...` prefix and bare `NAME=value` prefixes are
//!   dropped (values such as SIM_FROM=74.9 or VPX_DIR=... vary per run);
//! - `-j N`, `-jN`, `--jobs N`, `--jobs=N`, `--locked`, `--offline` are dropped:
//!   they do not change what is built, only how;
//! - the program becomes its basename without a Cargo test binary's
//!   `-<16 hex>` hash (`target/release/deps/video-0e73e8183febedc5` becomes
//!   `video`); absolute argument paths and `./`, `../`, `target/` ones become
//!   basenames (feature lists such as `video/vp8` are kept);
//! - `sh -c TEXT` / `bash -c TEXT`: the shell text is split on whitespace and the
//!   same rules apply to each word (`NAME=value` only before a command word);
//! - a `tasks-offload` client becomes `[offload] ` + its inner command (after
//!   `--`); its own flags (worker, owner, key, fetch paths) are dropped.
//!
//! Argument order is kept: `-p video --release` and `--release -p video` are
//! different kinds.
//!
//! `coarse_key` groups kinds that differ only in what they select to run, as a
//! fallback for ETA history: cargo keeps the subcommand, the sorted package set
//! and target-class flags (not test filters, `--test NAME`, or anything after
//! `--`); gradlew keeps the task names; `sh -c TEXT` uses the first cargo or
//! gradlew command in the text. Anything else has no coarse sharing (its
//! `coarse_key` is its `kind_key`).
use std::collections::BTreeSet;
use std::path::Path;

pub const OFFLOAD_MARKER: &str = "[offload] ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normalized {
    pub command: String,
    pub kind_key: String,
    pub coarse_key: String,
}

pub fn normalize(argv: &[String]) -> Normalized {
    let command = words(argv).join(" ");
    Normalized {
        kind_key: command.clone(),
        coarse_key: coarse(argv).unwrap_or_else(|| command.clone()),
        command,
    }
}

/// Coarse key of one argv; None when the whole `kind_key` applies.
fn coarse(argv: &[String]) -> Option<String> {
    let rest = strip_env(argv);
    let first = rest.first()?;
    if is_offload_client(first) || crate::peer_link::is_forward_argv(rest) {
        let inner = rest
            .iter()
            .position(|a| a == "--")
            .map(|i| &rest[i + 1..])
            .unwrap_or(&[]);
        return Some(format!(
            "{OFFLOAD_MARKER}{}",
            coarse(inner).unwrap_or_else(|| words(inner).join(" "))
        ));
    }
    let prog = program(first);
    if matches!(prog.as_str(), "sh" | "bash" | "dash" | "zsh")
        && rest.get(1).is_some_and(|a| a == "-c")
        && let Some(text) = rest.get(2)
    {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        return first_build_coarse(&tokens);
    }
    build_coarse(&prog, &rest[1..])
}

/// Coarse key of the first cargo/gradlew command in shell text words.
fn first_build_coarse(tokens: &[&str]) -> Option<String> {
    let is_sep = |w: &str| matches!(w, "&&" | "||" | ";" | "|") || w.ends_with(';');
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if is_assignment(t) || t == "env" || t == "export" {
            i += 1;
            continue;
        }
        let prog = program(t);
        if matches!(prog.as_str(), "cargo" | "gradlew") {
            let args: Vec<String> = tokens[i + 1..]
                .iter()
                .take_while(|w| !is_sep(w))
                .map(|w| w.to_string())
                .collect();
            return build_coarse(&prog, &args);
        }
        // Skip to the next command word.
        while i < tokens.len() && !is_sep(tokens[i]) {
            i += 1;
        }
        i += 1;
    }
    None
}

fn build_coarse(prog: &str, rest: &[String]) -> Option<String> {
    match prog {
        "cargo" => cargo_coarse(rest),
        "gradlew" => gradle_coarse(rest),
        _ => None,
    }
}

/// `cargo [+toolchain] SUBCOMMAND`, the sorted package set, then the sorted
/// target-class flags. Test filters, positional words, `--test`/`--bin` names
/// and everything after `--` are left out.
fn cargo_coarse(rest: &[String]) -> Option<String> {
    // Flags whose next word is a value that does not change the class of build.
    const VALUE_FLAGS: &[&str] = &[
        "--test",
        "--bin",
        "--example",
        "--bench",
        "--features",
        "-F",
        "--target",
        "--manifest-path",
        "--target-dir",
        "--message-format",
        "--color",
    ];
    const CLASS_FLAGS: &[&str] = &[
        "--workspace",
        "--all",
        "--lib",
        "--bins",
        "--examples",
        "--tests",
        "--benches",
        "--doc",
        "--all-targets",
        "--release",
        "-r",
        "--all-features",
        "--no-default-features",
    ];
    let words = args(rest);
    let mut it = words.iter().map(String::as_str);
    let mut head = vec!["cargo".to_string()];
    let mut sub = None;
    let mut packages = BTreeSet::new();
    let mut flags = BTreeSet::new();
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "-p" || a == "--package" {
            packages.extend(it.next().map(|p| format!("-p {p}")));
        } else if let Some(p) = a.strip_prefix("--package=") {
            packages.insert(format!("-p {p}"));
        } else if let Some(p) = a.strip_prefix("-p").filter(|p| !p.is_empty()) {
            packages.insert(format!("-p {p}"));
        } else if a == "--exclude" || a == "--profile" {
            if let Some(v) = it.next() {
                flags.insert(format!("{a} {v}"));
            }
        } else if VALUE_FLAGS.contains(&a) {
            it.next();
        } else if CLASS_FLAGS.contains(&a) {
            flags.insert(a.to_string());
        } else if a.starts_with('+') && sub.is_none() {
            head.push(a.to_string());
        } else if !a.starts_with('-') && sub.is_none() {
            sub = Some(a.to_string());
        }
    }
    head.extend(sub);
    head.extend(packages);
    head.extend(flags);
    (head.len() > 1).then(|| head.join(" "))
}

/// `gradlew` and its task names in order; `-P`/`-D` and other flags dropped.
fn gradle_coarse(rest: &[String]) -> Option<String> {
    let mut tasks = vec![];
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--tests" {
            it.next();
        } else if !a.starts_with('-') {
            tasks.push(a.as_str());
        }
    }
    (!tasks.is_empty()).then(|| format!("gradlew {}", tasks.join(" ")))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    })
}

/// Drops `env`, `--` and NAME=value words at the start.
pub fn strip_env(argv: &[String]) -> &[String] {
    let mut rest = argv;
    if rest
        .first()
        .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "env"))
    {
        rest = &rest[1..];
        if rest.first().is_some_and(|a| a == "--") {
            rest = &rest[1..];
        }
    }
    while rest.first().is_some_and(|a| is_assignment(a)) {
        rest = &rest[1..];
    }
    rest
}

/// Basename for absolute paths and obvious relative ones (`./`, `../`,
/// `target/`). Other words with `/` (feature lists such as `video/vp8`) stay.
fn short(word: &str) -> String {
    let pathlike = word.starts_with('/')
        || word.starts_with("./")
        || word.starts_with("../")
        || word.starts_with("target/");
    if pathlike && !word.ends_with('/') {
        Path::new(word)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| word.to_string())
    } else {
        word.to_string()
    }
}

/// Program word: always its basename, without a Cargo `-<16 hex>` hash.
fn program(word: &str) -> String {
    let base = Path::new(word)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| word.to_string());
    match base.rsplit_once('-') {
        Some((stem, hash))
            if !stem.is_empty()
                && hash.len() == 16
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            stem.to_string()
        }
        _ => base,
    }
}

fn is_offload_client(word: &str) -> bool {
    Path::new(word)
        .file_name()
        .is_some_and(|n| n == "tasks-offload")
}

/// Normalised words of one argv (see module docs).
fn words(argv: &[String]) -> Vec<String> {
    let rest = strip_env(argv);
    let Some(first) = rest.first() else {
        return vec![];
    };
    // A forwarded job (`tasks peer-forward ... -- CMD`) reads like an offload client.
    if is_offload_client(first) || crate::peer_link::is_forward_argv(rest) {
        let inner = rest
            .iter()
            .position(|a| a == "--")
            .map(|i| &rest[i + 1..])
            .unwrap_or(&[]);
        let mut out = words(inner);
        match out.first_mut() {
            Some(head) => head.insert_str(0, OFFLOAD_MARKER),
            None => out.push(OFFLOAD_MARKER.trim_end().to_string()),
        }
        return out;
    }
    let prog = program(first);
    if matches!(prog.as_str(), "sh" | "bash" | "dash" | "zsh")
        && rest.get(1).is_some_and(|a| a == "-c")
        && let Some(text) = rest.get(2)
    {
        let tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        return [prog, "-c".into()]
            .into_iter()
            .chain(shell_words(&tokens))
            .collect();
    }
    let mut out = vec![prog];
    out.extend(args(&rest[1..]));
    out
}

/// Drops the parallelism/lockfile flags and shortens paths, up to the end.
fn args(rest: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    let mut after_separator = false;
    while i < rest.len() {
        let a = rest[i].as_str();
        if after_separator {
            out.push(short(a));
            i += 1;
            continue;
        }
        if a == "--" {
            after_separator = true;
            out.push(a.into());
        } else if a == "-j" || a == "--jobs" {
            i += 1; // also skip the value
        } else if a == "--locked"
            || a == "--offline"
            || a.starts_with("--jobs=")
            || (a.len() > 2 && a.starts_with("-j") && a[2..].bytes().all(|b| b.is_ascii_digit()))
        {
        } else {
            out.push(short(a));
        }
        i += 1;
    }
    out
}

/// Shell text words: `NAME=value` dropped before each command word (start,
/// or after `&&`, `||`, `;`, `|`), then the same flag rules as argv.
fn shell_words(tokens: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut command_start = true;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if matches!(t, "&&" | "||" | ";" | "|") {
            out.push(t.to_string());
            command_start = true;
            i += 1;
            continue;
        }
        if command_start && (is_assignment(t) || t == "env") {
            i += 1;
            continue;
        }
        if t == "-j" || t == "--jobs" {
            i += 2;
            continue;
        }
        if t == "--locked"
            || t == "--offline"
            || t.starts_with("--jobs=")
            || (t.len() > 2 && t.starts_with("-j") && t[2..].bytes().all(|b| b.is_ascii_digit()))
        {
            i += 1;
            continue;
        }
        // A trailing `;` glued to a word ends the command too.
        let ends = t.len() > 1 && t.ends_with(';');
        out.push(if command_start { program(t) } else { short(t) });
        command_start = ends;
        i += 1;
    }
    out
}

/// The argv of the first `cargo` invocation in a job's argv: directly (after
/// env assignments), inside `sh -c TEXT` (whitespace words up to `&&`/`;`),
/// or behind a tasks-offload wrapper (`None`: nothing is built locally).
pub fn cargo_invocation(argv: &[String]) -> Option<CargoInvocation> {
    let mut env = vec![];
    let mut rest = argv;
    if rest
        .first()
        .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "env"))
    {
        rest = &rest[1..];
        if rest.first().is_some_and(|a| a == "--") {
            rest = &rest[1..];
        }
    }
    while let Some(a) = rest.first().filter(|a| is_assignment(a)) {
        env.push(a.clone());
        rest = &rest[1..];
    }
    let first = rest.first()?;
    if is_offload_client(first)
        || first.ends_with("offload-portable-desktop.sh")
        || crate::peer_link::is_forward_argv(rest)
    {
        return None;
    }
    let prog = program(first);
    if prog == "cargo" {
        return Some(CargoInvocation {
            env,
            args: rest[1..].to_vec(),
        });
    }
    if matches!(prog.as_str(), "sh" | "bash" | "dash" | "zsh")
        && rest.get(1).is_some_and(|a| a == "-c")
        && let Some(text) = rest.get(2)
    {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let mut env = vec![];
        let mut i = 0;
        while i < tokens.len() {
            let t = tokens[i];
            if is_assignment(t) || t == "env" || t == "export" {
                if is_assignment(t) {
                    env.push(t.to_string());
                }
                i += 1;
                continue;
            }
            if Path::new(t).file_name().is_some_and(|n| n == "cargo") {
                let args = tokens[i + 1..]
                    .iter()
                    .take_while(|w| !matches!(**w, "&&" | "||" | ";" | "|") && !w.ends_with(';'))
                    .map(|w| w.to_string())
                    .collect();
                return Some(CargoInvocation { env, args });
            }
            // Skip to the next command word.
            while i < tokens.len() && !matches!(tokens[i], "&&" | "||" | ";" | "|") {
                if tokens[i].ends_with(';') {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CargoInvocation {
    /// `NAME=value` words before cargo (e.g. CARGO_TARGET_DIR).
    pub env: Vec<String>,
    /// Everything after `cargo`.
    pub args: Vec<String>,
}

impl CargoInvocation {
    fn value_of(&self, flag: &str) -> Option<&str> {
        let mut it = self.args.iter();
        while let Some(a) = it.next() {
            if a == "--" {
                break;
            }
            if a == flag {
                return it.next().map(String::as_str);
            }
            if let Some(v) = a.strip_prefix(flag).and_then(|v| v.strip_prefix('=')) {
                return Some(v);
            }
        }
        None
    }
    fn has(&self, flag: &str) -> bool {
        self.args
            .iter()
            .take_while(|a| *a != "--")
            .any(|a| a == flag)
    }
    /// Output directory name under the target dir: `debug`, `release` or a
    /// custom profile's name.
    pub fn profile_dir(&self) -> String {
        if let Some(profile) = self.value_of("--profile") {
            return match profile {
                "dev" | "test" => "debug".into(),
                "bench" | "release" => "release".into(),
                other => other.into(),
            };
        }
        if self.has("--release") || self.has("-r") {
            "release".into()
        } else {
            "debug".into()
        }
    }
    pub fn target_triple(&self) -> Option<&str> {
        self.value_of("--target")
    }
    pub fn target_dir(&self) -> Option<&str> {
        self.value_of("--target-dir").or_else(|| {
            self.env
                .iter()
                .find_map(|e| e.strip_prefix("CARGO_TARGET_DIR="))
        })
    }
    pub fn manifest_path(&self) -> Option<&str> {
        self.value_of("--manifest-path")
    }
}

/// True when the job builds with Cargo locally (directly or in `sh -c`).
/// Integration test targets a command names with `--test NAME` (also inside an `sh -c`
/// script or a forwarded command), for a "target 12/23" progress count. None when the
/// command names none, so a bare `cargo test` shows no total it cannot know.
pub fn test_targets_expected(argv: &[String]) -> Option<u64> {
    let mut names = std::collections::HashSet::new();
    let words: Vec<&str> = argv.iter().flat_map(|a| a.split_whitespace()).collect();
    for (i, word) in words.iter().enumerate() {
        if let Some(name) = word.strip_prefix("--test=") {
            names.insert(name);
        } else if *word == "--test"
            && let Some(name) = words.get(i + 1)
        {
            names.insert(*name);
        }
    }
    (!names.is_empty()).then_some(names.len() as u64)
}

pub fn is_cargo(argv: &[String]) -> bool {
    cargo_invocation(argv).is_some()
}

/// True for an offload client (`tasks-offload`, `offload-portable-desktop.sh`) or a job
/// forwarded to a peer (`tasks peer-forward`).
pub fn is_offload(argv: &[String]) -> bool {
    crate::peer_link::is_forward_argv(strip_env(argv))
        || strip_env(argv).first().is_some_and(|a| {
            Path::new(a).file_name().is_some_and(|n| {
                crate::local_command::OFFLOAD_CLIENTS
                    .iter()
                    .any(|client| n == *client)
            })
        })
}
