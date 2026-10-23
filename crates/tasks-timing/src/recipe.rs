//! Recipe keys: the normalised identity of a command, shared by every run of the same work.

use crate::{recipe_cargo, recipe_gradle, shell};
use std::collections::BTreeSet;
use std::fmt;
use tasks_core::Kind;

/// Normalised command. Equal keys mean "the same work", whatever the checkout, `-j` or flag order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecipeKey {
    /// Tool basename: `cargo`, `gradle` (for `gradle` and `gradlew`), or `argv[0]`'s basename.
    pub tool: String,
    /// Subcommand (`test`, `check`, ...); empty for gradle and other tools.
    pub subcommand: String,
    /// Sorted `-p` packages.
    pub packages: Vec<String>,
    /// Sorted features.
    pub features: Vec<String>,
    /// Cargo profile (`dev` when unset).
    pub profile: String,
    /// Target triple, or empty.
    pub target: String,
    /// Sorted selection: `lib`, `bins`, `doc`, `test:X`, `filter:X`; gradle task names.
    pub selection: Vec<String>,
    /// Sorted flags that change the build (`all-features`, `workspace`, ...).
    pub flags: Vec<String>,
    /// Arguments kept verbatim (tools other than cargo/gradle).
    pub rest: Vec<String>,
}

const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash"];
/// Commands in a chain that only prepare the environment and say nothing about the work.
const SETUP: &[&str] = &["cd", "ulimit", "export", "set", "unset", "umask"];
const PROBES: &[&str] = &["true", "false", "echo", "sleep", "exit", ":", "printf"];

pub(crate) fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

pub(crate) fn sorted(set: BTreeSet<String>) -> Vec<String> {
    set.into_iter().collect()
}

impl RecipeKey {
    pub(crate) fn new(tool: &str) -> RecipeKey {
        RecipeKey {
            tool: tool.to_string(),
            subcommand: String::new(),
            packages: Vec::new(),
            features: Vec::new(),
            profile: String::new(),
            target: String::new(),
            selection: Vec::new(),
            flags: Vec::new(),
            rest: Vec::new(),
        }
    }

    /// Keys for a command: one, or one per command of a `sh -c "a && b"` chain.
    pub fn from_argv(argv: &[String], env: &[(String, String)]) -> Vec<RecipeKey> {
        // Leading `VAR=x`, `env`, `time` and `exec` words are not the command.
        let start = argv
            .iter()
            .position(|a| !(is_assignment(a) || matches!(a.as_str(), "env" | "time" | "exec")))
            .unwrap_or(argv.len());
        let argv = argv.get(start..).unwrap_or(&[]);
        let Some(first) = argv.first() else {
            return Vec::new();
        };
        let tool = basename(first);
        if SHELLS.contains(&tool)
            && let Some(i) = argv.iter().skip(1).position(|a| is_c_flag(a))
            && let Some(script) = argv.get(i + 2)
            && let Some(cmds) = shell::split_script(script)
        {
            let keys: Vec<RecipeKey> = cmds
                .iter()
                .filter(|c| !c.first().is_some_and(|w| SETUP.contains(&basename(w))))
                .flat_map(|c| RecipeKey::from_argv(c, env))
                .collect();
            if !keys.is_empty() {
                return keys;
            }
        }
        let args = argv.get(1..).unwrap_or(&[]);
        vec![match tool {
            "cargo" => recipe_cargo::parse(args, env),
            "gradle" | "gradlew" => recipe_gradle::parse(args),
            _ => {
                let mut k = RecipeKey::new(tool);
                k.rest = args.to_vec();
                k
            }
        }]
    }

    /// The string a command's history is stored under: its recipe, or for a `sh -c "a && b"`
    /// chain every command's recipe joined by ` && `, so `build.sh` and `build.sh && test.sh`
    /// do not share durations. `None` for an empty command.
    pub fn history_key(argv: &[String], env: &[(String, String)]) -> Option<String> {
        let keys = RecipeKey::from_argv(argv, env);
        (!keys.is_empty()).then(|| {
            keys.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" && ")
        })
    }

    /// The job kind this recipe implies. Probes (`true`, `echo`, `sleep N`, `exit N`) are `Probe`.
    pub fn kind(&self) -> Kind {
        let sel = self.selection.join(" ").to_ascii_lowercase();
        match (self.tool.as_str(), self.subcommand.as_str()) {
            (t, _) if PROBES.contains(&t) => Kind::Probe,
            ("cargo", "test" | "nextest run" | "nextest" | "bench") => {
                if self.flags.iter().any(|f| f == "no-run") {
                    Kind::Compile
                } else {
                    Kind::Test
                }
            }
            ("cargo", "run") => Kind::Run,
            ("cargo", _) => Kind::Compile,
            ("gradle", _) if sel.contains("connected") || sel.contains("install") => Kind::Device,
            ("gradle", _) if sel.contains("test") => Kind::Test,
            ("gradle", _) if sel.contains("assemble") || sel.contains("bundle") => Kind::Package,
            ("gradle", _) => Kind::Compile,
            _ => Kind::Run,
        }
    }
}

fn is_assignment(a: &str) -> bool {
    a.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn is_c_flag(a: &str) -> bool {
    a.len() > 1
        && a.starts_with('-')
        && !a.starts_with("--")
        && a.ends_with('c')
        && a.chars().skip(1).all(|c| c.is_ascii_alphabetic())
}

impl fmt::Display for RecipeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.tool)?;
        if !self.subcommand.is_empty() {
            write!(f, " {}", self.subcommand)?;
        }
        let parts: [(&str, String); 6] = [
            ("pkg", self.packages.join(",")),
            ("feat", self.features.join(",")),
            ("profile", self.profile.clone()),
            ("target", self.target.clone()),
            ("sel", self.selection.join(",")),
            ("flags", self.flags.join(",")),
        ];
        for (name, v) in parts.iter().filter(|(_, v)| !v.is_empty()) {
            write!(f, " {name}={v}")?;
        }
        for a in &self.rest {
            if a.contains(char::is_whitespace) || a.is_empty() {
                write!(f, " {a:?}")?
            } else {
                write!(f, " {a}")?
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
