//! The job list's columns and which of them fit (R-205, R-581). Pure.
//!
//! One list serves Running, Waiting and Finished, so the columns are the same for all and
//! every one carries something for every row: the state cell holds what used to be the
//! Remote, Class, Limit and Cancel reason columns.
//!
//! Every column has a priority. When the window is too narrow the highest-numbered priority
//! goes first; priority 0 columns are essential and never dropped.

use crate::model::JobRow;

/// A list column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Col {
    /// Job id.
    Id,
    /// Agent: colour stripe, name and, when the name mentions one, the vendor.
    Agent,
    /// The command.
    Command,
    /// Machine chip.
    Machine,
    /// What the job is doing, in words.
    State,
}

/// One column of the list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColSpec {
    /// The column.
    pub col: Col,
    /// Width in points (the command column grows past this when there is room).
    pub width: f32,
    /// 0 is essential; larger numbers drop first.
    pub priority: u8,
}

/// Space between columns, in points.
pub const GAP: f32 = 12.0;
/// The narrowest window the layout supports, in points.
pub const MIN_WINDOW_WIDTH: f32 = 640.0;

const fn c(col: Col, width: f32, priority: u8) -> ColSpec {
    ColSpec {
        col,
        width,
        priority,
    }
}

/// All columns, in display order.
pub fn specs() -> Vec<ColSpec> {
    vec![
        c(Col::Id, 44.0, 0),
        c(Col::Agent, 116.0, 0),
        c(Col::Command, 120.0, 0),
        c(Col::Machine, 64.0, 1),
        c(Col::State, 232.0, 0),
    ]
}

/// Total width of `cols` with the gaps between them.
pub fn total_width(cols: &[ColSpec]) -> f32 {
    cols.iter().map(|s| s.width).sum::<f32>() + GAP * cols.len().saturating_sub(1) as f32
}

/// The columns that fit in `available` points, in display order. Least important columns go
/// first; essential ones stay even when they do not fit (the list then scrolls).
pub fn visible(available: f32) -> Vec<ColSpec> {
    let mut cols = specs();
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

/// A row's text in a column, as plain text (for tooltips and the glyph check).
pub fn cell(row: &JobRow, col: Col) -> String {
    match col {
        Col::Id => format!("#{}", row.id),
        Col::Agent => match row.vendor {
            Some(v) => format!("{} {v}", row.agent),
            None => row.agent.clone(),
        },
        Col::Command => row.command.clone(),
        Col::Machine => row.machine.clone(),
        Col::State => {
            let s = &row.state;
            [s.head.as_str(), s.left.as_str(), s.right.as_str()]
                .iter()
                .filter(|t| !t.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::model;

    #[test]
    fn r205_column_layout_drops_least_important_columns_first() {
        let all = specs();
        let mut width = total_width(&all);
        let mut kept = all.len();
        let mut last_dropped_priority = u8::MAX;
        // shrink the window one point at a time: the visible set only ever loses columns,
        // and each loss is of the least important column left
        let mut prev: Vec<Col> = all.iter().map(|s| s.col).collect();
        while width > 100.0 {
            width -= 1.0;
            let now = visible(width);
            let now_cols: Vec<Col> = now.iter().map(|s| s.col).collect();
            assert!(now_cols.iter().all(|t| prev.contains(t)));
            if now.len() < kept {
                let newly: Vec<_> = all
                    .iter()
                    .filter(|s| !now_cols.contains(&s.col) && prev.contains(&s.col))
                    .collect();
                assert_eq!(newly.len(), 1, "one column per step");
                let dropped = newly[0].priority;
                // nothing kept is less important than what was just dropped
                assert!(now.iter().all(|s| s.priority <= dropped));
                assert!(dropped <= last_dropped_priority);
                last_dropped_priority = dropped;
                kept = now.len();
            }
            prev = now_cols;
        }
        // a wide window shows everything
        assert_eq!(visible(5000.0).len(), all.len());
        // the first column to go is the machine chip
        let just_short = total_width(&all) - 1.0;
        assert!(!visible(just_short).iter().any(|s| s.col == Col::Machine));
    }

    #[test]
    fn r205_essential_columns_fit_the_narrowest_window() {
        let narrow = visible(MIN_WINDOW_WIDTH);
        assert!(total_width(&narrow) <= MIN_WINDOW_WIDTH);
        let essential: Vec<_> = specs().into_iter().filter(|s| s.priority == 0).collect();
        // even in a window of nothing, essential columns stay
        assert_eq!(visible(0.0), essential);
        assert!(total_width(&essential) <= MIN_WINDOW_WIDTH);
        for col in [Col::Id, Col::Agent, Col::Command, Col::State] {
            assert!(narrow.iter().any(|s| s.col == col), "{col:?}");
        }
    }

    #[test]
    fn r581_no_column_of_the_list_is_empty_for_any_row() {
        for d in [fixtures::busy(), fixtures::offline()] {
            let vm = model::build(&d, 0);
            for r in vm.running.iter().chain(&vm.queued).chain(&vm.recent) {
                for s in specs() {
                    let text = cell(r, s.col);
                    assert!(
                        !text.is_empty() && text != crate::format::UNKNOWN,
                        "#{} {:?}: {text:?}",
                        r.id,
                        s.col
                    );
                }
            }
        }
    }
}
