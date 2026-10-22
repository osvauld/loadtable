//! The Cargo `-j` policy, applied by rewriting argv or env at spawn (R-092, R-093).
//!
//! `cargo.max_jobs` is the ceiling: every `-j` form up to it is accepted unchanged, a larger
//! request is lowered to it. While another compile job holds a slot a new Cargo job is lowered
//! further, to `cargo.jobs_when_busy`; a lone job keeps its request. A job that asks for
//! nothing gets `CARGO_BUILD_JOBS` set explicitly, so the machine never defaults to the core
//! count (R-091). The rewrite is reported in a note the daemon writes into the job log.

/// The numbers the rewrite works from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CargoJobs {
    /// Ceiling for any request (`cargo.max_jobs`).
    pub max: u32,
    /// Ceiling while another compile job runs (`cargo.jobs_when_busy`).
    pub when_busy: u32,
    /// What a job that asks for nothing gets before the busy ceiling applies.
    pub default: u32,
}

/// The rewritten command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    /// The argv to run.
    pub argv: Vec<String>,
    /// The environment to run with.
    pub env: Vec<(String, String)>,
    /// What was changed, for the job log; `None` when nothing was.
    pub note: Option<String>,
}

/// True when the command is `cargo` (any path, optional `+toolchain` argument).
pub fn is_cargo(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|a| a.rsplit('/').next() == Some("cargo"))
}

/// Where a `-j` request sits in argv.
#[derive(Debug, Clone, Copy)]
enum Form {
    /// `-j N` or `--jobs N`: the number is the next argument.
    Separate(usize),
    /// `-jN`.
    Short(usize),
    /// `--jobs=N`.
    Long(usize),
}

fn find_request(argv: &[String]) -> Option<(Form, u32)> {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    for (i, a) in argv.iter().take(end).enumerate().skip(1) {
        let next = argv.get(i + 1).filter(|_| i + 1 < end);
        let found = if a == "-j" || a == "--jobs" {
            next.and_then(|n| n.parse::<u32>().ok())
                .map(|n| (Form::Separate(i), n))
        } else if let Some(v) = a.strip_prefix("--jobs=") {
            v.parse::<u32>().ok().map(|n| (Form::Long(i), n))
        } else if let Some(v) = a.strip_prefix("-j") {
            v.parse::<u32>().ok().map(|n| (Form::Short(i), n))
        } else {
            None
        };
        if let Some((form, n)) = found.filter(|(_, n)| *n >= 1) {
            return Some((form, n));
        }
    }
    None
}

fn set_request(argv: &mut [String], form: Form, n: u32) {
    match form {
        Form::Separate(i) => {
            if let Some(slot) = argv.get_mut(i + 1) {
                *slot = n.to_string();
            }
        }
        Form::Short(i) => {
            if let Some(slot) = argv.get_mut(i) {
                *slot = format!("-j{n}");
            }
        }
        Form::Long(i) => {
            if let Some(slot) = argv.get_mut(i) {
                *slot = format!("--jobs={n}");
            }
        }
    }
}

/// Applies the policy. Commands that are not `cargo` are returned unchanged.
pub fn rewrite(
    argv: &[String],
    env: &[(String, String)],
    policy: &CargoJobs,
    busy: bool,
) -> Rewrite {
    let mut out = Rewrite {
        argv: argv.to_vec(),
        env: env.to_vec(),
        note: None,
    };
    if !is_cargo(argv) {
        return out;
    }
    let limit = if busy {
        policy.max.min(policy.when_busy)
    } else {
        policy.max
    };
    let why = if busy && policy.when_busy < policy.max {
        format!("machine busy: cap {}", policy.when_busy)
    } else {
        format!("cap {}", policy.max)
    };
    if let Some((form, asked)) = find_request(argv) {
        if asked > limit {
            set_request(&mut out.argv, form, limit);
            out.note = Some(format!("cargo -j {asked} lowered to {limit} ({why})"));
        }
        return out;
    }
    let env_request = env
        .iter()
        .position(|(k, _)| k == "CARGO_BUILD_JOBS")
        .and_then(|i| env.get(i).map(|(_, v)| (i, v.parse::<u32>().ok())));
    match env_request {
        Some((i, Some(asked))) if asked >= 1 => {
            if asked > limit {
                if let Some(e) = out.env.get_mut(i) {
                    e.1 = limit.to_string();
                }
                out.note = Some(format!(
                    "CARGO_BUILD_JOBS {asked} lowered to {limit} ({why})"
                ));
            }
        }
        _ => {
            let n = policy.default.min(limit).max(1);
            out.env.retain(|(k, _)| k != "CARGO_BUILD_JOBS");
            out.env
                .push(("CARGO_BUILD_JOBS".to_string(), n.to_string()));
            if busy && n < policy.default {
                out.note = Some(format!(
                    "CARGO_BUILD_JOBS set to {n} instead of {} ({why})",
                    policy.default
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pol(max: u32, busy: u32) -> CargoJobs {
        CargoJobs {
            max,
            when_busy: busy,
            default: 2,
        }
    }

    // R-092: every -j form up to the cap passes untouched; above it is lowered to the cap.
    #[test]
    fn r092_every_j_form_up_to_the_cap_is_accepted() {
        let p = pol(8, 2);
        for forms in [
            a(&["cargo", "build", "-j", "8"]),
            a(&["cargo", "build", "-j8"]),
            a(&["cargo", "build", "--jobs", "8"]),
            a(&["cargo", "build", "--jobs=8"]),
            a(&["cargo", "build", "-j", "3"]),
        ] {
            let r = rewrite(&forms, &[], &p, false);
            assert_eq!(r.argv, forms);
            assert_eq!(r.note, None);
        }
        let r = rewrite(&a(&["cargo", "build", "-j16"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "build", "-j8"]));
        assert!(r.note.as_deref().unwrap().contains("lowered to 8"));
        let r = rewrite(&a(&["cargo", "test", "--jobs", "12"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "test", "--jobs", "8"]));
        let r = rewrite(&a(&["cargo", "test", "--jobs=12"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "test", "--jobs=8"]));
    }

    // R-092: the default cap is 2 and a job that asks for nothing gets it explicitly (R-091).
    #[test]
    fn r092_default_cap_is_two_and_unspecified_jobs_get_it() {
        let p = pol(2, 1);
        let r = rewrite(&a(&["cargo", "check"]), &[], &p, false);
        assert_eq!(
            r.env,
            vec![("CARGO_BUILD_JOBS".to_string(), "2".to_string())]
        );
        assert_eq!(r.note, None);
        let r = rewrite(&a(&["cargo", "check", "-j4"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "check", "-j2"]));
    }

    // R-093
    #[test]
    fn r093_a_busy_machine_lowers_cargo_jobs_but_a_lone_job_keeps_the_cap() {
        let p = pol(8, 2);
        let lone = rewrite(&a(&["cargo", "build", "-j8"]), &[], &p, false);
        assert_eq!(lone.argv, a(&["cargo", "build", "-j8"]));
        let busy = rewrite(&a(&["cargo", "build", "-j8"]), &[], &p, true);
        assert_eq!(busy.argv, a(&["cargo", "build", "-j2"]));
        assert!(busy.note.as_deref().unwrap().contains("machine busy"));
        // a small request is not raised
        let small = rewrite(&a(&["cargo", "build", "-j1"]), &[], &p, true);
        assert_eq!(small.argv, a(&["cargo", "build", "-j1"]));
        // unspecified under load
        let p = CargoJobs {
            max: 8,
            when_busy: 1,
            default: 4,
        };
        let r = rewrite(&a(&["cargo", "build"]), &[], &p, true);
        assert_eq!(
            r.env,
            vec![("CARGO_BUILD_JOBS".to_string(), "1".to_string())]
        );
        assert!(r.note.is_some());
    }

    #[test]
    fn env_requests_and_non_cargo_commands() {
        let p = pol(4, 2);
        let env = vec![("CARGO_BUILD_JOBS".to_string(), "12".to_string())];
        let r = rewrite(&a(&["cargo", "build"]), &env, &p, false);
        assert_eq!(
            r.env,
            vec![("CARGO_BUILD_JOBS".to_string(), "4".to_string())]
        );
        let r = rewrite(&a(&["/usr/bin/cargo", "+nightly", "build"]), &[], &p, false);
        assert_eq!(r.env.len(), 1);
        let r = rewrite(&a(&["sh", "-c", "cargo build -j64"]), &[], &p, true);
        assert_eq!(r.argv, a(&["sh", "-c", "cargo build -j64"]));
        assert!(r.env.is_empty() && r.note.is_none());
        // arguments after `--` belong to the test binary
        let r = rewrite(&a(&["cargo", "test", "--", "-j99"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "test", "--", "-j99"]));
        // `-j` as the last argument or with a non-number is left alone
        let r = rewrite(&a(&["cargo", "build", "-j"]), &[], &p, false);
        assert_eq!(r.argv, a(&["cargo", "build", "-j"]));
    }
}
