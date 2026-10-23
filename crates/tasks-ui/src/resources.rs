//! The side panel's Resources block: what a job uses now and over the last minutes, as the
//! text and the numbers the window draws. Pure: the window only paints it (R-640..R-645).

use crate::format::{self, UNKNOWN};
use crate::model::{JobRow, Section};
use std::time::Duration;
use tasks_core::Attribution;
use tasks_core::metrics::{JobMetricsView, MetricSample, ProcGroup, rates};

/// How often a running job's metrics are asked for.
pub const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long after it ends a job's samples are still there to ask for (the daemon keeps them
/// this long).
pub const KEEP_SECS: u64 = 300;
/// Points a graph draws at most; a longer history is averaged down to it.
pub const GRAPH_POINTS: usize = 120;
/// Process rows shown: the daemon sends twelve and a row for the rest.
pub const MAX_PROC_ROWS: usize = 13;

/// Whether the panel wants a job's metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wants {
    /// The job runs: ask every [`POLL_EVERY`].
    Live,
    /// The job ended less than [`KEEP_SECS`] ago: ask once, and once more if the last answer
    /// came while it still ran.
    Final,
    /// Nothing to ask for: it has not started, or its samples are gone.
    No,
}

/// What the panel wants for `row`.
pub fn wants(row: &JobRow) -> Wants {
    match row.section {
        Section::Running => Wants::Live,
        Section::Recent
            if row.job.started_at_ms.is_some()
                && row.finished_ago_secs.is_some_and(|s| s <= KEEP_SECS) =>
        {
            Wants::Final
        }
        _ => Wants::No,
    }
}

/// True when a request should go out now. `since_last` is the time since the last request
/// (`None` when none went out for this job), `last_was_live` whether the job still ran then.
pub fn due(wants: Wants, since_last: Option<Duration>, last_was_live: bool) -> bool {
    match wants {
        Wants::No => false,
        Wants::Live => since_last.is_none_or(|s| s >= POLL_EVERY),
        Wants::Final => since_last.is_none() || last_was_live,
    }
}

/// A graph: its points as shares of the scale, and the words beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    /// `CPU`, `Memory`.
    pub label: &'static str,
    /// The current value, in words: `3.2 of 10 cores`, `1.4 GiB (usual peak 2.1 GiB)`.
    pub text: String,
    /// What the top of the graph stands for: `top 6 cores`.
    pub scale: String,
    /// Oldest first, each 0..=1 of the scale.
    pub points: Vec<f32>,
    /// A dashed line at this share of the scale (the usual peak).
    pub marker: Option<f32>,
    /// The tooltip.
    pub tip: String,
}

/// One row of the process table.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    /// Program base name, or the `others` label.
    pub name: String,
    /// `×6`; empty for the others row.
    pub count: String,
    /// `4.6 cores`, or why not.
    pub cpu: String,
    /// The CPU bar, 0..=1 of the busiest row.
    pub cpu_frac: f32,
    /// `2.4 GiB`.
    pub mem: String,
    /// True for the row that sums the rest.
    pub others: bool,
}

/// The processes of the job grouped by program.
#[derive(Debug, Clone, PartialEq)]
pub struct Procs {
    /// `11 processes`.
    pub note: String,
    /// Busiest first.
    pub rows: Vec<ProcRow>,
}

/// The live figures.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    /// CPU in cores.
    pub cpu: Graph,
    /// Memory.
    pub mem: Graph,
    /// Label and value: disk read and write rates, I/O stall.
    pub disk: Vec<(&'static str, String)>,
    /// The time the graphs cover: `last 4 min`.
    pub span: String,
    /// The processes; none when the daemon sent none.
    pub procs: Option<Procs>,
    /// A refresh that failed after figures had arrived: they are shown and this says why they
    /// are not newer.
    pub stale: Option<String>,
}

/// What the block shows under its heading.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// One sentence instead of figures.
    Message(String),
    /// The figures.
    Live(Box<Live>),
}

/// The block.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// Right of the heading: `exact (cgroup) · measured on mac · every 2 s`.
    pub note: String,
    /// Its content.
    pub body: Body,
}

/// What the window knows about the job's metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct Input<'a> {
    /// The newest good answer.
    pub view: Option<&'a JobMetricsView>,
    /// Why the newest request failed.
    pub error: Option<&'a str>,
    /// True while the first request is out.
    pub loading: bool,
    /// Logical cores of the machine the job runs on, when known.
    pub cores: Option<f64>,
}

fn quality_word(q: Option<Attribution>) -> Option<&'static str> {
    q.map(|q| match q {
        Attribution::Cgroup => "exact (cgroup)",
        Attribution::ProcessTree => "approximate (process tree)",
    })
}

fn note(view: Option<&JobMetricsView>, w: Wants) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = view {
        if let Some(q) = quality_word(v.quality) {
            parts.push(q.to_string());
        }
        parts.push(match &v.source {
            Some(s) => format!("measured {s}"),
            None => "measured here".to_string(),
        });
    }
    match w {
        Wants::Live => parts.push("every 2 s".to_string()),
        Wants::Final => parts.push("final readings".to_string()),
        Wants::No => {}
    }
    parts.join(" · ")
}

/// Averages `v` down to at most `max` points, oldest first.
pub fn downsample(v: &[f32], max: usize) -> Vec<f32> {
    if max == 0 || v.len() <= max {
        return v.to_vec();
    }
    (0..max)
        .map(|i| {
            let a = i * v.len() / max;
            let b = ((i + 1) * v.len() / max).max(a + 1).min(v.len());
            let part = v.get(a..b).unwrap_or_default();
            part.iter().sum::<f32>() / part.len().max(1) as f32
        })
        .collect()
}

fn rate_text(bps: Option<f64>) -> String {
    bps.map_or_else(
        || "not measured".to_string(),
        |v| format!("{}/s", format::bytes(v.max(0.0) as u64)),
    )
}

fn so_far(b: Option<u64>) -> String {
    b.map(|b| format!(" · {} so far", format::bytes(b)))
        .unwrap_or_default()
}

fn cpu_series(samples: &[MetricSample]) -> Vec<f32> {
    samples
        .windows(2)
        .filter_map(|w| match w {
            [a, b] => rates(a, b).cores.map(|c| c as f32),
            _ => None,
        })
        .collect()
}

fn cpu_graph(samples: &[MetricSample], cores: Option<f64>) -> Graph {
    let series = cpu_series(samples);
    let now = series.last().copied();
    let peak = series.iter().copied().fold(0.0_f32, f32::max);
    // scale to the data, but never past the machine: the top of the graph is "all cores" at most
    let mut top = (peak * 1.25).max(1.0);
    if let Some(c) = cores.filter(|c| *c >= 1.0) {
        top = top.min(c as f32);
    }
    let text = match (now, cores) {
        (Some(n), Some(c)) => format!("{n:.1} of {c:.0} cores"),
        (Some(n), None) => format!("{n:.1} cores"),
        (None, _) => "measuring".to_string(),
    };
    let tip = match cores {
        Some(c) => format!(
            "CPU time the job's processes use, in cores, of the {c:.0} the machine has.\nPeak {peak:.1} cores."
        ),
        None => format!("CPU time the job's processes use, in cores.\nPeak {peak:.1} cores."),
    };
    Graph {
        label: "CPU",
        text,
        scale: format!("top {top:.0} cores"),
        points: downsample(&series, GRAPH_POINTS)
            .into_iter()
            .map(|v| (v / top).clamp(0.0, 1.0))
            .collect(),
        marker: None,
        tip,
    }
}

fn mem_graph(samples: &[MetricSample], usual: Option<u64>) -> Graph {
    let series: Vec<f32> = samples
        .iter()
        .filter_map(|s| s.mem_bytes)
        .map(|b| b as f32)
        .collect();
    let now = samples.iter().rev().find_map(|s| s.mem_bytes);
    let peak = series.iter().copied().fold(0.0_f32, f32::max);
    let top = (peak.max(usual.map_or(0.0, |u| u as f32)) * 1.15).max(1.0);
    let text = match (now, usual) {
        (Some(n), Some(u)) => format!("{} (usual peak {})", format::bytes(n), format::bytes(u)),
        (Some(n), None) => format!("{} (no history for this command yet)", format::bytes(n)),
        (None, _) => "not measured".to_string(),
    };
    let tip = match usual {
        Some(u) => format!(
            "Memory in use now. The dashed line is the median peak of this command's earlier runs on this machine: {}.",
            format::bytes(u)
        ),
        None => "Memory in use now. There is no earlier run of this command to compare with."
            .to_string(),
    };
    Graph {
        label: "Memory",
        text,
        scale: format!("top {}", format::bytes(top as u64)),
        points: downsample(&series, GRAPH_POINTS)
            .into_iter()
            .map(|v| (v / top).clamp(0.0, 1.0))
            .collect(),
        marker: usual
            .filter(|_| now.is_some())
            .map(|u| (u as f32 / top).clamp(0.0, 1.0)),
        tip,
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn proc_rows(groups: &[ProcGroup]) -> Procs {
    let shown: Vec<&ProcGroup> = groups.iter().take(MAX_PROC_ROWS).collect();
    let busiest = shown
        .iter()
        .filter_map(|g| g.cpu_cores)
        .fold(0.0_f64, f64::max);
    let total: u32 = shown.iter().map(|g| g.count).sum();
    let rows = shown
        .iter()
        .map(|g| ProcRow {
            name: g.name.clone(),
            count: if g.others {
                String::new()
            } else {
                format!("×{}", g.count)
            },
            cpu: g
                .cpu_cores
                .map_or_else(|| "measuring".to_string(), |c| format!("{c:.1} cores")),
            cpu_frac: match g.cpu_cores {
                Some(c) if busiest > 0.0 => (c / busiest).clamp(0.0, 1.0) as f32,
                _ => 0.0,
            },
            mem: format::bytes(g.rss_bytes),
            others: g.others,
        })
        .collect();
    Procs {
        note: plural(total, "process", "processes"),
        rows,
    }
}

fn span_text(samples: &[MetricSample]) -> String {
    match (samples.first(), samples.last()) {
        (Some(a), Some(b)) if b.t_ms > a.t_ms => {
            format!("last {}", format::coarse((b.t_ms - a.t_ms) as f64 / 1000.0))
        }
        _ => UNKNOWN.to_string(),
    }
}

/// The block for one job.
pub fn panel(row: &JobRow, input: Input<'_>) -> Panel {
    let w = wants(row);
    let message = |m: &str| Panel {
        note: String::new(),
        body: Body::Message(m.to_string()),
    };
    if w == Wants::No {
        return message(match row.section {
            Section::Queued => "Shows here once the job runs.",
            Section::Recent if row.job.started_at_ms.is_none() => "The job never started.",
            _ => "Live figures are kept for 5 minutes after a job ends.",
        });
    }
    let Some(v) = input.view else {
        return match input.error {
            Some(e) => message(&format!("metrics unavailable: {e}")),
            None => message(if input.loading {
                "reading metrics…"
            } else {
                "no samples yet"
            }),
        };
    };
    let head = note(Some(v), w);
    if v.samples.is_empty() && v.procs.is_empty() {
        let why = v
            .note
            .clone()
            .unwrap_or_else(|| "no samples yet".to_string());
        return Panel {
            note: head,
            body: Body::Message(why),
        };
    }
    let last_two = match v.samples.as_slice() {
        [.., a, b] => Some(rates(a, b)),
        _ => None,
    }
    .unwrap_or_default();
    let last = v.samples.last();
    let disk = vec![
        (
            "Disk read",
            format!(
                "{}{}",
                rate_text(last_two.read_bps),
                so_far(last.and_then(|s| s.io_read_bytes))
            ),
        ),
        (
            "Disk write",
            format!(
                "{}{}",
                rate_text(last_two.write_bps),
                so_far(last.and_then(|s| s.io_write_bytes))
            ),
        ),
    ]
    .into_iter()
    .chain(
        last_two
            .io_stall_pct
            .map(|p| ("I/O wait", format!("{p:.0}% of the time"))),
    )
    .collect();
    Panel {
        note: head,
        body: Body::Live(Box::new(Live {
            cpu: cpu_graph(&v.samples, input.cores),
            mem: mem_graph(&v.samples, v.usual_peak_mem_bytes),
            disk,
            span: span_text(&v.samples),
            procs: (!v.procs.is_empty()).then(|| proc_rows(&v.procs)),
            stale: input
                .error
                .map(|e| format!("could not refresh: {e}; these figures are from earlier")),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::model;

    fn row(id: i64) -> JobRow {
        let vm = model::build(&fixtures::busy(), 0);
        vm.running
            .iter()
            .chain(&vm.queued)
            .chain(&vm.recent)
            .find(|r| r.id == id)
            .cloned()
            .expect("row")
    }

    fn live_of(p: Panel) -> Live {
        match p.body {
            Body::Live(l) => *l,
            other => panic!("not live: {other:?}"),
        }
    }

    // R-640
    #[test]
    fn r640_cpu_reads_in_cores_of_the_machine_and_memory_beside_its_usual_peak() {
        let v = fixtures::metrics_running();
        let p = panel(
            &row(101),
            Input {
                view: Some(&v),
                cores: Some(10.0),
                ..Default::default()
            },
        );
        assert_eq!(p.note, "exact (cgroup) · measured here · every 2 s");
        let l = live_of(p);
        assert_eq!(l.cpu.text, "3.2 of 10 cores");
        assert_eq!(l.mem.text, "1.4 GiB (usual peak 2.1 GiB)");
        assert!(l.mem.marker.is_some_and(|m| m > 0.0 && m <= 1.0));
        assert!(l.cpu.points.iter().all(|p| (0.0..=1.0).contains(p)));
        assert_eq!(l.cpu.points.len(), v.samples.len() - 1);
        assert!(l.span.starts_with("last "), "{}", l.span);
        assert!(l.cpu.scale.starts_with("top "), "{}", l.cpu.scale);
    }

    // R-640
    #[test]
    fn r640_the_graph_scale_never_passes_the_machines_cores() {
        let v = fixtures::metrics_running();
        let l = live_of(panel(
            &row(101),
            Input {
                view: Some(&v),
                cores: Some(2.0),
                ..Default::default()
            },
        ));
        assert_eq!(l.cpu.scale, "top 2 cores");
        assert!(l.cpu.points.iter().all(|p| *p <= 1.0));
        // no machine size: the scale follows the data and the text drops the "of"
        let l = live_of(panel(
            &row(101),
            Input {
                view: Some(&v),
                ..Default::default()
            },
        ));
        assert_eq!(l.cpu.text, "3.2 cores");
    }

    // R-640
    #[test]
    fn r640_without_history_memory_says_so_and_has_no_dashed_line() {
        let mut v = fixtures::metrics_running();
        v.usual_peak_mem_bytes = None;
        let l = live_of(panel(
            &row(101),
            Input {
                view: Some(&v),
                ..Default::default()
            },
        ));
        assert_eq!(l.mem.text, "1.4 GiB (no history for this command yet)");
        assert_eq!(l.mem.marker, None);
    }

    // R-640
    #[test]
    fn r640_disk_is_a_rate_and_a_total_with_the_wait_when_known() {
        let v = fixtures::metrics_running();
        let l = live_of(panel(
            &row(101),
            Input {
                view: Some(&v),
                ..Default::default()
            },
        ));
        let read = &l.disk[0];
        assert_eq!(read.0, "Disk read");
        assert!(
            read.1.contains("/s") && read.1.contains("so far"),
            "{}",
            read.1
        );
        assert!(
            l.disk
                .iter()
                .any(|(k, v)| *k == "I/O wait" && v.ends_with("of the time"))
        );
    }

    // R-640
    #[test]
    fn r640_processes_group_by_program_with_cores_and_memory_and_a_row_for_the_rest() {
        let v = fixtures::metrics_running();
        let l = live_of(panel(
            &row(101),
            Input {
                view: Some(&v),
                ..Default::default()
            },
        ));
        let procs = l.procs.expect("procs");
        assert_eq!(procs.rows[0].name, "rustc");
        assert_eq!(procs.rows[0].count, "×6");
        assert_eq!(procs.rows[0].cpu, "2.4 cores");
        assert_eq!(procs.rows[0].cpu_frac, 1.0);
        assert_eq!(procs.rows[0].mem, "1.1 GiB");
        let last = procs.rows.last().expect("row");
        assert!(last.others && last.count.is_empty(), "{last:?}");
        assert!(procs.rows.len() <= MAX_PROC_ROWS);
        assert_eq!(procs.note, "14 processes");
        // a group not yet measured says so instead of a zero
        let mut v2 = v;
        v2.procs[1].cpu_cores = None;
        let p2 = live_of(panel(
            &row(101),
            Input {
                view: Some(&v2),
                ..Default::default()
            },
        ))
        .procs
        .expect("procs");
        assert_eq!(p2.rows[1].cpu, "measuring");
        assert_eq!(p2.rows[1].cpu_frac, 0.0);
    }

    // R-640
    #[test]
    fn r640_a_forwarded_job_says_where_it_was_measured() {
        let mut v = fixtures::metrics_running();
        v.source = Some("on mac, remote job #9".into());
        v.quality = Some(Attribution::ProcessTree);
        let p = panel(
            &row(103),
            Input {
                view: Some(&v),
                ..Default::default()
            },
        );
        assert_eq!(
            p.note,
            "approximate (process tree) · measured on mac, remote job #9 · every 2 s"
        );
    }

    // R-640
    #[test]
    fn r640_empty_and_failed_states_are_said_in_words() {
        let r = row(101);
        let none = |input: Input| match panel(&r, input).body {
            Body::Message(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(none(Input::default()), "no samples yet");
        assert_eq!(
            none(Input {
                loading: true,
                ..Default::default()
            }),
            "reading metrics…"
        );
        assert_eq!(
            none(Input {
                error: Some("nixos unreachable"),
                ..Default::default()
            }),
            "metrics unavailable: nixos unreachable"
        );
        let empty = JobMetricsView {
            quality: None,
            samples: vec![],
            procs: vec![],
            usual_peak_mem_bytes: None,
            note: Some("peer nixos does not serve job metrics; upgrade it".into()),
            source: None,
        };
        assert_eq!(
            none(Input {
                view: Some(&empty),
                ..Default::default()
            }),
            "peer nixos does not serve job metrics; upgrade it"
        );
        let silent = JobMetricsView {
            note: None,
            ..empty
        };
        assert_eq!(
            none(Input {
                view: Some(&silent),
                ..Default::default()
            }),
            "no samples yet"
        );
        // figures that were there stay, with the reason they are old
        let v = fixtures::metrics_running();
        let l = live_of(panel(
            &r,
            Input {
                view: Some(&v),
                error: Some("timed out"),
                ..Default::default()
            },
        ));
        assert_eq!(
            l.stale.as_deref(),
            Some("could not refresh: timed out; these figures are from earlier")
        );
    }

    // R-645
    #[test]
    fn r645_only_a_running_or_just_finished_job_is_asked_for() {
        assert_eq!(wants(&row(101)), Wants::Live);
        assert_eq!(wants(&row(110)), Wants::No, "waiting");
        assert_eq!(wants(&row(113)), Wants::No, "held back by the peer");
        // finished 60 s ago, started: final readings
        assert_eq!(wants(&row(92)), Wants::Final);
        // older than five minutes
        assert_eq!(wants(&row(93)), Wants::No);
        assert_eq!(wants(&row(85)), Wants::No);
        let w = panel(&row(110), Input::default());
        assert_eq!(
            w.body,
            Body::Message("Shows here once the job runs.".into())
        );
        let old = panel(&row(93), Input::default());
        assert_eq!(
            old.body,
            Body::Message("Live figures are kept for 5 minutes after a job ends.".into())
        );
    }

    // R-645
    #[test]
    fn r645_a_running_job_is_polled_every_two_seconds_and_a_finished_one_once_more() {
        let s = Some;
        assert!(due(Wants::Live, None, false));
        assert!(!due(Wants::Live, s(Duration::from_millis(1999)), true));
        assert!(due(Wants::Live, s(Duration::from_secs(2)), true));
        assert!(!due(Wants::No, None, false));
        assert!(!due(Wants::No, s(Duration::from_secs(60)), true));
        // finished: the first look, then one more when the last came while it ran, then none
        assert!(due(Wants::Final, None, false));
        assert!(due(Wants::Final, s(Duration::from_secs(1)), true));
        assert!(!due(Wants::Final, s(Duration::from_secs(60)), false));
    }

    #[test]
    fn a_long_history_is_averaged_down_to_the_graph_width() {
        let v: Vec<f32> = (0..900).map(|i| i as f32).collect();
        let d = downsample(&v, 120);
        assert_eq!(d.len(), 120);
        assert!(d.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(downsample(&[1.0, 2.0], 120), vec![1.0, 2.0]);
        assert!(downsample(&[], 10).is_empty());
        assert_eq!(downsample(&[1.0], 0), vec![1.0]);
    }
}
