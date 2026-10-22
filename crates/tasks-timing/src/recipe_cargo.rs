//! Normalisation of `cargo` command lines.

use crate::recipe::{RecipeKey, sorted};
use std::collections::BTreeSet;

type Args<'a> = std::slice::Iter<'a, String>;

/// Value of option `names` in `a` (`--x v`, `--x=v`, `-x v`, `-xv`), consuming the next arg if needed.
fn opt(a: &str, it: &mut Args<'_>, names: &[&str]) -> Option<String> {
    for n in names {
        if a == *n {
            return Some(it.next().cloned().unwrap_or_default());
        }
        if let Some(v) = a.strip_prefix(&format!("{n}=")) {
            return Some(v.to_string());
        }
        if !n.starts_with("--") && a.len() > n.len() && a.starts_with(n) {
            return Some(a.get(n.len()..).unwrap_or("").to_string());
        }
    }
    None
}

fn split_list(v: &str, into: &mut BTreeSet<String>) {
    into.extend(
        v.split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    );
}

/// Flags that do not change what is built or run.
fn ignored_flag(a: &str) -> bool {
    matches!(
        a,
        "--locked"
            | "--frozen"
            | "--offline"
            | "--quiet"
            | "--verbose"
            | "--keep-going"
            | "-q"
            | "-v"
    ) || a.starts_with("--timings")
        || (a.starts_with('-')
            && !a.starts_with("--")
            && a.len() > 1
            && a.get(1..)
                .is_some_and(|r| r.chars().all(|c| c == 'q' || c == 'v')))
}

pub(crate) fn parse(args: &[String], env: &[(String, String)]) -> RecipeKey {
    let mut k = RecipeKey::new("cargo");
    let (mut pk, mut ft, mut sel, mut fl) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut profile = String::from("dev");
    let mut target = String::new();
    let mut it = args.iter();
    let mut test_like = false;
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--" {
            let tail: Vec<&String> = it.by_ref().collect();
            if test_like {
                let mut t = tail.into_iter();
                while let Some(x) = t.next() {
                    match x.as_str() {
                        "--ignored" | "--include-ignored" | "--exact" => {
                            sel.insert(x.trim_start_matches('-').to_string());
                        }
                        "--skip" => {
                            sel.insert(format!("skip:{}", t.next().cloned().unwrap_or_default()));
                        }
                        s if !s.starts_with('-') => {
                            sel.insert(format!("filter:{s}"));
                        }
                        _ => {}
                    }
                }
            } else {
                k.rest = tail.into_iter().cloned().collect();
            }
            break;
        }
        if let Some(v) = opt(a, &mut it, &["-p", "--package"]) {
            pk.insert(v);
        } else if let Some(v) = opt(a, &mut it, &["--features", "-F"]) {
            split_list(&v, &mut ft);
        } else if let Some(v) = opt(a, &mut it, &["--profile"]) {
            profile = v;
        } else if let Some(v) = opt(a, &mut it, &["--target"]) {
            target = v;
        } else if let Some(v) = opt(a, &mut it, &["--exclude"]) {
            fl.insert(format!("exclude:{v}"));
        } else if let Some(v) = opt(a, &mut it, &["--test", "--bin", "--example", "--bench"]) {
            sel.insert(format!(
                "{}:{v}",
                a.split('=').next().unwrap_or("").trim_start_matches('-')
            ));
        } else if opt(
            a,
            &mut it,
            &[
                "-j",
                "--jobs",
                "--message-format",
                "--color",
                "--manifest-path",
                "--target-dir",
            ],
        )
        .is_some()
            || ignored_flag(a)
        {
        } else if a == "--release" || a == "-r" {
            profile = "release".into();
        } else if matches!(
            a,
            "--lib" | "--bins" | "--doc" | "--tests" | "--benches" | "--examples" | "--all-targets"
        ) {
            sel.insert(a.trim_start_matches('-').to_string());
        } else if a == "--all" || a == "--workspace" {
            fl.insert("workspace".into());
        } else if let Some(v) = opt(a, &mut it, &["-Z"]) {
            fl.insert(format!("Z{v}"));
        } else if a.starts_with('-') {
            fl.insert(a.trim_start_matches('-').to_string());
        } else if let Some(tc) = a.strip_prefix('+') {
            fl.insert(format!("+{tc}"));
        } else if k.subcommand.is_empty() {
            k.subcommand = a.to_string();
            test_like = matches!(a, "test" | "bench" | "nextest");
        } else if k.subcommand == "nextest" && !k.subcommand.contains(' ') {
            k.subcommand.push(' ');
            k.subcommand.push_str(a);
        } else {
            sel.insert(format!("filter:{a}"));
        }
    }
    if target.is_empty() {
        target = env
            .iter()
            .find(|(n, _)| n == "CARGO_BUILD_TARGET")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
    }
    k.packages = sorted(pk);
    k.features = sorted(ft);
    k.selection = sorted(sel);
    k.flags = sorted(fl);
    k.profile = profile;
    k.target = target;
    k
}
