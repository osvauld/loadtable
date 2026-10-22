//! Pure placement scoring: which machines can run a job, and which is best.

use tasks_core::{Machine, MachineName, Requirements, Unmet};

/// Checks every machine against the requirements, keeping the unmet reasons.
pub fn candidates(
    req: &Requirements,
    machines: &[Machine],
) -> Vec<(MachineName, Result<(), Vec<Unmet>>)> {
    machines
        .iter()
        .map(|m| (m.name.clone(), req.satisfied_by(m)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tasks_core::{Arch, Os};

    fn machine(name: &str, os: Os) -> Machine {
        Machine {
            name: MachineName(name.into()),
            os,
            arch: Arch::X86_64,
            cores: 4,
            memory_bytes: 8 << 30,
            tools: BTreeSet::new(),
            devices: BTreeSet::new(),
        }
    }

    #[test]
    fn splits_capable_and_incapable() {
        let req = Requirements {
            os: Some(Os::Macos),
            ..Requirements::default()
        };
        let ms = [machine("lin", Os::Linux), machine("mac", Os::Macos)];
        let out = candidates(&req, &ms);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].1,
            Err(vec![Unmet::Os {
                want: Os::Macos,
                have: Os::Linux
            }])
        );
        assert_eq!(out[1], (MachineName("mac".into()), Ok(())));
    }
}
