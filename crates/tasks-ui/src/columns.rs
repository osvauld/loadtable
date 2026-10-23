//! Table columns and which of them fit (R-205). Pure.
//!
//! Every column has a priority. When the window is too narrow the highest-numbered priority
//! goes first; priority 0 columns are essential and never dropped.

use crate::format::UNKNOWN;
use crate::model::{JobRow, Section};

/// A table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Col {
    /// Job id.
    Id,
    /// Agent: owner up to `@`, with its colour dot.
    Agent,
    /// The command.
    Command,
    /// Machine badge.
    Machine,
    /// Phase.
    Phase,
    /// Progress bar and `done/total`.
    Progress,
    /// ETA and its basis.
    Eta,
    /// Queue position.
    Position,
    /// What it waits for.
    Wait,
    /// The class the queue orders the job by.
    Class,
    /// Start estimate.
    StartEstimate,
    /// The peer's phase of a forward.
    RemotePhase,
    /// Wall-clock limit.
    Timeout,
    /// Cancel reason.
    CancelReason,
    /// Running time, waiting time or time since finish.
    Age,
}

/// One column of a table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColSpec {
    /// The column.
    pub col: Col,
    /// Header text.
    pub title: &'static str,
    /// Width in points (the command column grows past this when there is room).
    pub width: f32,
    /// 0 is essential; larger numbers drop first.
    pub priority: u8,
}

/// Space between columns, in points.
pub const GAP: f32 = 8.0;
/// The narrowest window the layout supports, in points.
pub const MIN_WINDOW_WIDTH: f32 = 640.0;
/// Width the action buttons of the details bar need, in points (they wrap, never clip).
pub const ACTION_BAR_WIDTH: f32 = 420.0;

const fn c(col: Col, title: &'static str, width: f32, priority: u8) -> ColSpec {
    ColSpec {
        col,
        title,
        width,
        priority,
    }
}

/// All columns of a section, in display order.
pub fn specs(section: Section) -> Vec<ColSpec> {
    match section {
        Section::Running => vec![
            c(Col::Id, "Job", 52.0, 0),
            c(Col::Agent, "Agent", 96.0, 0),
            c(Col::Command, "Command", 200.0, 0),
            c(Col::Machine, "Machine", 72.0, 1),
            c(Col::Eta, "ETA", 170.0, 2),
            c(Col::Progress, "Progress", 120.0, 3),
            c(Col::RemotePhase, "Remote", 80.0, 4),
            c(Col::Age, "Running", 64.0, 5),
            c(Col::Timeout, "Limit", 80.0, 6),
        ],
        Section::Queued => vec![
            c(Col::Id, "Job", 52.0, 0),
            c(Col::Agent, "Agent", 96.0, 0),
            c(Col::Command, "Command", 200.0, 0),
            c(Col::Position, "#", 32.0, 1),
            c(Col::Wait, "Waiting for", 230.0, 2),
            c(Col::Class, "Class", 84.0, 2),
            c(Col::StartEstimate, "Starts in", 140.0, 3),
            c(Col::Machine, "Machine", 72.0, 4),
            c(Col::Age, "Queued", 64.0, 5),
            c(Col::Timeout, "Limit", 80.0, 6),
        ],
        Section::Recent => vec![
            c(Col::Id, "Job", 52.0, 0),
            c(Col::Agent, "Agent", 96.0, 0),
            c(Col::Command, "Command", 200.0, 0),
            c(Col::Phase, "Result", 130.0, 0),
            c(Col::Age, "Finished", 90.0, 2),
            c(Col::Machine, "Machine", 72.0, 3),
            c(Col::CancelReason, "Cancel reason", 150.0, 4),
            c(Col::Timeout, "Limit", 80.0, 5),
        ],
    }
}

/// Total width of `cols` with the gaps between them.
pub fn total_width(cols: &[ColSpec]) -> f32 {
    cols.iter().map(|s| s.width).sum::<f32>() + GAP * cols.len().saturating_sub(1) as f32
}

/// The columns that fit in `available` points, in display order. Least important columns go
/// first; essential ones stay even when they do not fit (the table then scrolls).
pub fn visible(section: Section, available: f32) -> Vec<ColSpec> {
    let mut cols = specs(section);
    while total_width(&cols) > available {
        // the droppable column with the largest priority number; the later one on a tie
        let worst = cols
            .iter()
            .enumerate()
            .filter(|(_, s)| s.priority > 0)
            .max_by_key(|(i, s)| (s.priority, *i))
            .map(|(i, _)| i);
        match worst {
            Some(i) => {
                cols.remove(i);
            }
            None => break,
        }
    }
    cols
}

/// A row's text in a column. Unknown values read [`UNKNOWN`], never zero (R-128).
pub fn cell(row: &JobRow, col: Col) -> String {
    let dash = || UNKNOWN.to_string();
    match col {
        Col::Id => format!("#{}", row.id),
        Col::Agent => row.agent.clone(),
        Col::Command => row.command.clone(),
        Col::Machine => row.machine.clone(),
        Col::Phase => row.phase.clone(),
        Col::Progress => row.progress_text.clone(),
        Col::Eta => {
            if row.eta_basis.is_empty() {
                row.eta.clone()
            } else {
                format!("{} ({})", row.eta, row.eta_basis)
            }
        }
        Col::Position => row.position.map_or_else(dash, |p| p.to_string()),
        Col::Wait => row.wait.clone().unwrap_or_else(dash),
        Col::Class => {
            if row.queue_class.is_empty() {
                dash()
            } else {
                row.queue_class.clone()
            }
        }
        Col::StartEstimate => row.start_estimate.clone().unwrap_or_else(dash),
        Col::RemotePhase => row.remote_phase.clone().unwrap_or_else(dash),
        Col::Timeout => row.timeout.clone().unwrap_or_else(dash),
        Col::CancelReason => row.cancel_reason.clone().unwrap_or_else(dash),
        Col::Age => row.age.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(cols: &[ColSpec]) -> Vec<&'static str> {
        cols.iter().map(|s| s.title).collect()
    }

    #[test]
    fn r205_column_layout_drops_least_important_columns_first() {
        for section in [Section::Running, Section::Queued, Section::Recent] {
            let all = specs(section);
            let mut width = total_width(&all);
            let mut kept = all.len();
            let mut last_dropped_priority = u8::MAX;
            // shrink the window one point at a time: the visible set only ever loses columns,
            // and each loss is of the least important column left
            let mut prev = titles(&all);
            while width > 100.0 {
                width -= 1.0;
                let now = visible(section, width);
                let now_titles = titles(&now);
                assert!(now_titles.iter().all(|t| prev.contains(t)), "{section:?}");
                if now.len() < kept {
                    let gone: Vec<_> = all
                        .iter()
                        .filter(|s| !now_titles.contains(&s.title))
                        .collect();
                    let newly: Vec<_> = gone.iter().filter(|s| prev.contains(&s.title)).collect();
                    assert_eq!(newly.len(), 1, "one column per step");
                    let dropped = newly[0].priority;
                    // nothing kept is less important than what was just dropped
                    assert!(
                        now.iter().all(|s| s.priority <= dropped),
                        "{section:?} {now_titles:?}"
                    );
                    assert!(dropped <= last_dropped_priority);
                    last_dropped_priority = dropped;
                    kept = now.len();
                }
                prev = now_titles;
            }
        }
        // a wide window shows everything
        assert_eq!(
            visible(Section::Queued, 5000.0).len(),
            specs(Section::Queued).len()
        );
        // the first column to go is the one with the highest priority number
        let all = specs(Section::Running);
        let just_short = total_width(&all) - 1.0;
        assert!(!titles(&visible(Section::Running, just_short)).contains(&"Limit"));
    }

    #[test]
    fn r205_essential_columns_and_buttons_fit_without_clipping() {
        for section in [Section::Running, Section::Queued, Section::Recent] {
            let narrow = visible(section, MIN_WINDOW_WIDTH);
            assert!(total_width(&narrow) <= MIN_WINDOW_WIDTH, "{section:?}");
            let essential: Vec<_> = specs(section)
                .into_iter()
                .filter(|s| s.priority == 0)
                .collect();
            // even in a window of nothing, essential columns stay
            let nothing = visible(section, 0.0);
            assert_eq!(nothing, essential);
            assert!(total_width(&essential) <= MIN_WINDOW_WIDTH);
            assert!(narrow.iter().any(|s| s.col == Col::Id));
            assert!(narrow.iter().any(|s| s.col == Col::Agent));
            assert!(narrow.iter().any(|s| s.col == Col::Command));
        }
        const { assert!(ACTION_BAR_WIDTH <= MIN_WINDOW_WIDTH) };
    }
}
