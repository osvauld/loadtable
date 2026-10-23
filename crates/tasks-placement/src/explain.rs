//! Stable text renderings of a placement report (R-116: written once at the top of the job log).

use crate::decide::{
    Decision, Exclusion, MachineReport, Refusal, Report, Score, Verdict, WaitBasis,
};
use std::fmt;
use tasks_core::Unmet;

/// Whole seconds as `12s`, `2m05s` or `1h02m`.
pub fn fmt_secs(secs: f64) -> String {
    let s = if secs.is_finite() {
        secs.round().max(0.0)
    } else {
        0.0
    } as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

impl fmt::Display for Exclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exclusion::NotRequested => f.write_str("not requested"),
            Exclusion::DeviceLocalOnly => f.write_str("device jobs stay local"),
            Exclusion::Unmet(u) => {
                let parts: Vec<String> = u.iter().map(unmet_text).collect();
                write!(f, "unmet: {}", parts.join(", "))
            }
            Exclusion::AboveCargoCap { asked, cap } => write!(
                f,
                "cargo parallelism {asked} is above this machine's cap of {cap}"
            ),
            Exclusion::NotTransportable(why) => write!(f, "cannot be shipped: {why}"),
            Exclusion::Marked(m) => write!(f, "marked: {m}"),
            Exclusion::Unreachable {
                last_seen_ago_secs: Some(s),
            } => write!(f, "unreachable, last seen {} ago", fmt_secs(*s as f64)),
            Exclusion::Unreachable {
                last_seen_ago_secs: None,
            } => f.write_str("unreachable, never seen"),
            Exclusion::Stale {
                age_secs,
                limit_secs,
            } => write!(
                f,
                "stale: state {} old, limit {}",
                fmt_secs(*age_secs as f64),
                fmt_secs(*limit_secs as f64)
            ),
        }
    }
}

fn unmet_text(u: &Unmet) -> String {
    match u {
        Unmet::Os { want, have } => format!("os {want:?} wanted, has {have:?}"),
        Unmet::Arch { want, have } => format!("arch {want:?} wanted, has {have:?}"),
        Unmet::Tool(t) => format!("tool {} missing", t.0),
        Unmet::Device(d) => format!("device {} missing", d.0),
        Unmet::Memory { want, have } => format!("memory {want} bytes wanted, has {have}"),
    }
}

fn basis_text(s: &Score) -> String {
    let base = match s.wait_basis {
        WaitBasis::Free => "slot free",
        WaitBasis::Expected => "expected",
        WaitBasis::Unknown => "unknown, assumed",
    };
    if s.blocked {
        format!(
            "{base}, gate closed, pressure +{}",
            fmt_secs(s.pressure_secs)
        )
    } else if s.pressure_secs > 0.0 {
        format!("{base}, pressure +{}", fmt_secs(s.pressure_secs))
    } else {
        base.to_string()
    }
}

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "wait {} ({}) + run {}{} + sync {}",
            fmt_secs(self.wait_secs),
            basis_text(self),
            fmt_secs(self.run_secs),
            if self.run_known {
                ""
            } else {
                " (no history, assumed)"
            },
            fmt_secs(self.sync_secs)
        )?;
        if self.local_bonus_secs > 0.0 {
            write!(f, " - local bonus {}", fmt_secs(self.local_bonus_secs))?;
        }
        write!(f, " = {}", fmt_secs(self.total_secs()))
    }
}

/// The chosen machine's expected finish with its basis, or why there is none: a fallback
/// assumption (no run history, unknown queue wait) is never shown as an estimate.
pub fn estimate_text(s: &Score) -> String {
    if !s.run_known {
        return "no estimate (no run history for this command)".to_string();
    }
    if s.wait_basis == WaitBasis::Unknown {
        return "no estimate (the queue ahead is unknown)".to_string();
    }
    format!("estimate {} ({})", fmt_secs(s.total_secs()), basis_text(s))
}

fn exclusions_text(ex: &[Exclusion]) -> String {
    ex.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn machine_line(m: &MachineReport, chosen: bool) -> String {
    let side = if m.is_local { " (local)" } else { "" };
    let press = if m.pressure.is_empty() {
        String::new()
    } else {
        format!(" [{}]", m.pressure)
    };
    match &m.verdict {
        Verdict::Eligible(s) => format!(
            "  {}{side}: {s}{press}{}",
            m.name.0,
            if chosen { " <- chosen" } else { "" }
        ),
        Verdict::Excluded(ex) => {
            format!(
                "  {}{side}: excluded: {}{press}",
                m.name.0,
                exclusions_text(ex)
            )
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::UnknownPeer(n) => write!(f, "no peer named {}", n.0),
            Refusal::RequestedMachineCannot(n) => write!(f, "{} cannot run it", n.0),
            Refusal::NoCapableMachine => f.write_str("no machine can run it"),
        }
    }
}

impl Report {
    /// One line: the choice and each other machine's total or reason.
    pub fn one_line(&self) -> String {
        let chosen = self.chosen().map(|m| m.name.0.clone());
        let head = match &self.decision {
            Decision::Run(n) => format!("{} ({})", n.0, self.request.reason()),
            Decision::Wait => format!("wait ({}): no usable machine now", self.request.reason()),
            Decision::Refuse(r) => format!("refused ({}): {r}", self.request.reason()),
        };
        let rest: Vec<String> = self
            .machines
            .iter()
            .filter(|m| Some(&m.name.0) != chosen.as_ref())
            .map(|m| match &m.verdict {
                Verdict::Eligible(s) if s.run_known => {
                    format!("{} {}", m.name.0, fmt_secs(s.total_secs()))
                }
                Verdict::Eligible(_) => format!("{} no estimate", m.name.0),
                Verdict::Excluded(ex) => format!("{} excluded: {}", m.name.0, exclusions_text(ex)),
            })
            .collect();
        let mut out = head;
        if let Some(c) = self.chosen()
            && let Verdict::Eligible(s) = &c.verdict
        {
            out.push_str(&format!(": {}", estimate_text(s)));
        }
        if !rest.is_empty() {
            out.push_str("; ");
            out.push_str(&rest.join(", "));
        }
        out
    }

    /// Several lines: a header and one line per machine, sorted by name.
    pub fn multi_line(&self) -> String {
        let chosen = self.chosen().map(|m| m.name.clone());
        let head = match &self.decision {
            Decision::Run(n) => format!("placement: {} ({})", n.0, self.request.reason()),
            Decision::Wait => format!(
                "placement: wait ({}), no usable machine now",
                self.request.reason()
            ),
            Decision::Refuse(r) => {
                format!("placement: refused ({}), {r}", self.request.reason())
            }
        };
        let mut lines = vec![head];
        lines.extend(
            self.machines
                .iter()
                .map(|m| machine_line(m, Some(&m.name) == chosen.as_ref())),
        );
        lines.join("\n")
    }
}
