//! Headless window tests: the dashboard is driven through egui frames without a
//! display, from the synthetic fixture snapshots. Nothing here contacts the
//! daemon, adb or the worker (fixture mode, or drawing functions only).
use super::*;
use crate::ui_source::FixtureSource;
use tinline_task_manager::Job;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/snapshot");

fn demo(scenario: &str) -> Dashboard {
    let dir = format!("{FIXTURES}/{scenario}");
    let feeds = FixtureSource::open_dir(Path::new(&dir))
        .unwrap()
        .into_iter()
        .map(|s| Feed::new(Box::new(s) as Box<dyn SnapshotSource>))
        .collect();
    Dashboard::new(PathBuf::from("/unused-ui-test.sock"), feeds, Some(dir))
}

/// A live-mode window with no sources; tests call drawing functions only, so
/// no request is ever sent (asserted through `next_seq`).
fn live(jobs: Vec<Job>) -> Dashboard {
    let mut d = Dashboard::new(PathBuf::from("/unused-ui-test.sock"), Vec::new(), None);
    d.response = Some(Response {
        version: tinline_task_manager::PROTOCOL_VERSION,
        execution_enabled: false,
        submitted_id: None,
        jobs,
        estimates: Vec::new(),
        error: None,
        log: None,
    });
    d
}

fn input(width: f32, height: f32) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width, height),
        )),
        ..Default::default()
    }
}

/// Runs a few frames (layout settles) and returns the last output.
fn frames(d: &mut Dashboard, context: &egui::Context, width: f32, height: f32) -> egui::FullOutput {
    let mut output = None;
    for _ in 0..3 {
        output = Some(context.run(input(width, height), |c| d.frame(c)));
    }
    output.unwrap()
}

type Text = (String, f32, usize, egui::Rect);

fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<Text> {
    fn walk(shape: &egui::Shape, clip: egui::Rect, out: &mut Vec<Text>) {
        match shape {
            egui::Shape::Text(text) => out.push((
                text.galley.text().to_string(),
                text.pos.x + text.galley.size().x,
                text.galley.rows.len(),
                clip,
            )),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, clip, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for shape in shapes {
        walk(&shape.shape, shape.clip_rect, &mut out);
    }
    out
}

fn joined(output: &egui::FullOutput) -> String {
    texts(&output.shapes)
        .into_iter()
        .map(|t| t.0)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn busy_fixture_dashboard_shows_both_machines_jobs_and_agents() {
    for width in [760.0, 1180.0, 1600.0] {
        let context = egui::Context::default();
        let mut d = demo("ui-busy");
        let output = frames(&mut d, &context, width, 1000.0);
        let text = joined(&output);
        for expected in [
            "6 agents active · 3 running · 3 queued · 13.2 cores busy across 2 machines",
            "kurukkan (laptop)",
            "nixos worker",
            "CPU 97%",
            "RAM 6.1/7.6 GB",
            "memory stalls",
            "build #91 view-step-up",
            "agents here:",
            "RUNNING",
            "QUEUED",
            "RECENT",
            "Agent",
            "#91",
            "⬅ #306",
            "view-step-up",
            "cargo test --release",
            "~4 min left (6 similar runs)",
            "starts in ~4 min",
            "✔ passed",
        ] {
            assert!(
                text.contains(expected),
                "missing {expected:?} at width {width}"
            );
        }
        if width >= 1180.0 {
            for expected in [
                "212 crates (usually ~310)",
                " WARM ",
                " COLD ",
                "compile slot held by #91",
            ] {
                assert!(
                    text.contains(expected),
                    "missing {expected:?} at width {width}"
                );
            }
        }
        assert_eq!(d.next_seq, 0, "a fixture demo must not send requests");
        assert!(
            d.devices.pending.is_none(),
            "a fixture demo must not run adb"
        );
    }
}

#[test]
fn essential_columns_and_buttons_fit_without_clipping() {
    for width in [760.0, 1060.0, 1180.0, 1600.0] {
        let context = egui::Context::default();
        let mut d = demo("ui-busy");
        let output = frames(&mut d, &context, width, 1200.0);
        let found = texts(&output.shapes);
        for label in [
            "Job", "Agent", "On", "Command", "ETA", "Starts", "Cancel", "Log",
        ] {
            let matches: Vec<_> = found.iter().filter(|t| t.0 == label).collect();
            assert!(!matches.is_empty(), "missing {label} at width {width}");
            for (_, right, rows, clip) in matches {
                assert!(
                    *right <= width && *right <= clip.right() + 0.5,
                    "{label} clipped at width {width}: right={right}, clip={clip:?}"
                );
                assert_eq!(*rows, 1, "{label} wrapped at width {width}");
            }
        }
    }
}

#[test]
fn column_layout_drops_least_important_columns_first() {
    let all = layout_columns(&RUNNING_COLUMNS, 1800.0);
    assert!(all.iter().all(Option::is_some));
    let narrow = layout_columns(&RUNNING_COLUMNS, 744.0);
    let shown: Vec<_> = RUNNING_COLUMNS
        .iter()
        .zip(&narrow)
        .filter(|(_, w)| w.is_some())
        .map(|(c, _)| c.title)
        .collect();
    assert_eq!(shown, vec!["Job", "Agent", "On", "Command", "ETA", ""]);
    let total: f32 = narrow.iter().flatten().sum::<f32>() + CELL_GAP * (shown.len() - 1) as f32;
    assert!(total <= 744.0 + 0.01, "{total}");
    // The flexible column takes the rest, never less than its minimum here.
    assert!(narrow[3].unwrap() >= FLEX_MIN);
    for columns in [&QUEUED_COLUMNS[..], &RECENT_COLUMNS[..]] {
        let widths = layout_columns(columns, 744.0);
        for (c, w) in columns.iter().zip(&widths) {
            assert!(c.drop > 0 || w.is_some(), "{} dropped", c.title);
        }
    }
}

#[test]
fn offline_worker_card_is_marked_and_its_jobs_are_stale() {
    let context = egui::Context::default();
    let mut d = demo("ui-offline");
    let text = joined(&frames(&mut d, &context, 1400.0, 900.0));
    assert!(text.contains("offline · last seen 5 min ago"), "{text}");
    assert!(text.contains("#91"));
    let idle = joined(&frames(
        &mut demo("ui-idle"),
        &egui::Context::default(),
        1400.0,
        900.0,
    ));
    assert!(idle.contains("Nothing running."));
    assert!(idle.contains("no agents active"));
}

#[test]
fn expanded_row_shows_full_owner_argv_and_raw_json_toggle() {
    let context = egui::Context::default();
    let mut d = demo("ui-busy");
    frames(&mut d, &context, 1400.0, 1000.0);
    let key = JobKey {
        machine: "nixos".into(),
        id: 91,
    };
    d.toggle(&key);
    let text = joined(&frames(&mut d, &context, 1400.0, 1400.0));
    assert!(text.contains("view-step-up@kurukkan"));
    assert!(text.contains("cargo test --release -j 8"));
    assert!(text.contains("raw snapshot JSON"));
    assert!(text.contains("median of 6 warm runs"));
    d.toggle(&key);
    assert!(d.open_rows.is_empty());
}

#[test]
fn live_mode_keeps_approval_and_actions_for_local_proposals() {
    let spec = JobSpec {
        suite: tinline_task_manager::catalog::SUITE.into(),
        configuration: tinline_task_manager::catalog::CONFIGURATION.into(),
        cost: Cost::Quick,
        owner: "layout-fixture".into(),
        source: "/synthetic/repository".into(),
        revision: "a".repeat(40),
    };
    let command = JobSpec {
        suite: tinline_task_manager::local_command::SUITE.into(),
        configuration: r#"{"argv":["cargo","test"],"resource":"compile"}"#.into(),
        ..spec.clone()
    };
    let job = |id, spec: &JobSpec, phase| Job {
        id,
        spec: spec.clone(),
        phase,
        submitted_at: unix_now(),
        execution: None,
    };
    let d = live(vec![
        job(1, &spec, Phase::Queued),
        job(2, &command, Phase::Queued),
        job(3, &spec, Phase::Approved),
    ]);
    let context = egui::Context::default();
    let mut clicks = Vec::new();
    let output = context.run(input(1180.0, 700.0), |c| {
        egui::CentralPanel::default().show(c, |ui| d.approvals(ui, &mut clicks));
    });
    let text = joined(&output);
    for expected in [
        "NEEDS APPROVAL",
        "Approve",
        "approve in a terminal",
        "Start local worker",
        "#1",
        "#2",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert!(clicks.is_empty());
    assert_eq!(d.next_seq, 0);
}

#[test]
fn new_job_tab_keeps_the_submission_form() {
    let mut d = live(Vec::new());
    d.tab = Tab::NewJob;
    let context = egui::Context::default();
    let output = context.run(input(900.0, 700.0), |c| {
        egui::CentralPanel::default().show(c, |ui| d.form(ui, c));
    });
    let text = joined(&output);
    for expected in [
        "New job",
        "Suite",
        "Submit",
        "Start local worker (approved jobs only)",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert_eq!(d.next_seq, 0);
}

#[test]
fn symbols_used_by_the_window_exist_in_the_default_fonts() {
    let context = egui::Context::default();
    let _ = context.run(egui::RawInput::default(), |_| {});
    let font = egui::FontId::proportional(14.0);
    let missing: Vec<_> = "⬅✔✖⏹⚠–·…➡"
        .chars()
        .filter(|c| !context.fonts_mut(|f| f.has_glyph(&font, *c)))
        .map(|c| format!("{c:?} U+{:04X}", c as u32))
        .collect();
    assert!(missing.is_empty(), "default fonts lack {missing:?}");
}

#[test]
fn devices_panel_renders_every_state_without_starting_adb() {
    let phone = |serial: &str, state: &str, busy: bool| Device {
        serial: serial.into(),
        state: state.into(),
        model: Some("synthetic phone".into()),
        kind: "phone",
        abis: vec!["arm64-v8a".into()],
        lock: device::lock_serial(serial).unwrap(),
        busy,
        holder: busy.then(|| "other-agent:job-1".into()),
    };
    let scans: [DeviceScan; 4] = [
        Ok(vec![]),
        Err("adb unavailable: synthetic".into()),
        Ok(vec![
            phone("SYNTH0001", "device", false),
            phone("192.168.0.2:5555", "device", true),
            phone("SYNTH0002", "unauthorized", false),
        ]),
        Ok(vec![Device {
            kind: "emulator",
            abis: vec!["x86_64".into()],
            ..phone("emulator-5554", "device", false)
        }]),
    ];
    for scan in scans {
        let context = egui::Context::default();
        let mut dashboard = live(Vec::new());
        dashboard.devices.scan = Some(scan);
        dashboard.devices.source = "/synthetic/app".into();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| dashboard.devices(ui));
        });
        // Rendering never scans or launches anything by itself.
        assert!(dashboard.devices.pending.is_none());
        assert!(dashboard.status.is_none());
    }
}

#[test]
fn self_suite_default_source_matches_only_the_embedded_layout() {
    let base = std::env::temp_dir().join(format!("tasks-ui-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    // Legacy layout: CHECKOUT/tools/task-manager is the crate; CHECKOUT is the source.
    let legacy = base.join("legacy-checkout");
    std::fs::create_dir_all(legacy.join("tools/task-manager")).unwrap();
    assert_eq!(
        self_suite_source(&legacy.join("tools/task-manager")),
        Some(std::fs::canonicalize(&legacy).unwrap())
    );
    // Standalone layout: the crate is the repository root; `../..` is not a source.
    let standalone = base.join("home/user/task-manager");
    std::fs::create_dir_all(&standalone).unwrap();
    assert_eq!(self_suite_source(&standalone), None);
    // A sibling named like the adapter directory does not make it the legacy layout.
    std::fs::create_dir_all(base.join("home/tools/task-manager")).unwrap();
    assert_eq!(self_suite_source(&base.join("home/user")), None);
    assert_eq!(self_suite_source(&base.join("missing")), None);
    std::fs::remove_dir_all(&base).unwrap();
}

/// Writes PPM screenshots of the fixture dashboards when
/// `TASKS_UI_SCREENSHOT_DIR` is set (skipped otherwise). Headless: egui output
/// is rasterised in software by `ui_raster`, no window is opened.
#[test]
fn screenshots_when_requested() {
    let Some(dir) = std::env::var_os("TASKS_UI_SCREENSHOT_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    type Shot<'a> = (&'a str, &'a str, [usize; 2], Option<(&'a str, i64)>);
    let shots: [Shot; 5] = [
        ("busy-wide", "ui-busy", [1400, 980], None),
        ("busy-narrow", "ui-busy", [760, 1150], None),
        ("busy-details", "ui-busy", [1400, 1300], Some(("nixos", 91))),
        ("offline", "ui-offline", [1400, 860], None),
        ("idle", "ui-idle", [1400, 760], None),
    ];
    for (name, scenario, size, open) in shots {
        let context = egui::Context::default();
        let mut d = demo(scenario);
        d.recent.expanded = name == "busy-wide";
        if let Some((machine, id)) = open {
            d.toggle(&JobKey {
                machine: machine.into(),
                id,
            });
        }
        let mut raster = raster::Raster::default();
        let mut last = None;
        for _ in 0..4 {
            let output = context.run(input(size[0] as f32, size[1] as f32), |c| d.frame(c));
            raster.apply(&output.textures_delta);
            last = Some(output);
        }
        let output = last.unwrap();
        let primitives = context.tessellate(output.shapes, output.pixels_per_point);
        let background = context.style().visuals.panel_fill;
        let rgb = raster.draw(&primitives, output.pixels_per_point, size, background);
        std::fs::write(dir.join(format!("{name}.ppm")), raster::ppm(&rgb, size)).unwrap();
    }
}

/// A source whose snapshot changes on every poll (the raw JSON of every job grows by one
/// byte), like a busy live machine; it never contacts anything.
struct Churn {
    template: serde_json::Value,
    n: usize,
}

impl SnapshotSource for Churn {
    fn label(&self) -> &str {
        "laptop"
    }
    fn local(&self) -> bool {
        true
    }
    fn poll(&mut self) -> Vec<Update> {
        self.n += 1;
        let mut value = self.template.clone();
        for job in value["jobs"].as_array_mut().unwrap() {
            job["synthetic_pad"] = serde_json::Value::String("x".repeat(self.n));
        }
        vec![Update::Snapshot {
            snapshot: Box::new(crate::ui_model::Snapshot::parse(value).unwrap()),
            age_s: 0.0,
        }]
    }
}

/// UI memory after N refreshes: with every row expanded and every job's JSON changing each
/// frame, egui's per-id memory stays flat (it grew by one collapsing/scroll state per open
/// row per refresh while the raw-JSON header's id included the JSON length).
#[test]
fn widget_memory_stays_flat_over_many_refreshes_with_open_rows() {
    let path = format!("{FIXTURES}/ui-busy/laptop.json");
    let template: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let feeds = vec![Feed::new(
        Box::new(Churn { template, n: 0 }) as Box<dyn SnapshotSource>
    )];
    // Fixture (demo) mode: no request is ever sent.
    let mut d = Dashboard::new(
        PathBuf::from("/unused-ui-test.sock"),
        feeds,
        Some("synthetic churn".into()),
    );
    d.recent.expanded = true;
    d.recent.show_cancelled = true;
    let context = egui::Context::default();
    frames(&mut d, &context, 1400.0, 1800.0);
    let keys: Vec<JobKey> = d
        .view
        .running
        .iter()
        .map(|c| c.key.clone())
        .chain(d.view.queued.iter().map(|r| r.key.clone()))
        .chain(d.view.recent.iter().map(|r| r.key.clone()))
        .collect();
    assert!(keys.len() >= 8, "fixture rows: {}", keys.len());
    d.open_rows.extend(keys);
    for _ in 0..20 {
        let _ = context.run(input(1400.0, 1800.0), |c| d.frame(c));
    }
    let settled = context.memory(|m| m.data.len());
    let refreshes = 300;
    for _ in 0..refreshes {
        let _ = context.run(input(1400.0, 1800.0), |c| d.frame(c));
    }
    let after = context.memory(|m| m.data.len());
    assert!(
        after <= settled + 4,
        "egui memory grew from {settled} to {after} entries over {refreshes} refreshes"
    );
    assert_eq!(d.next_seq, 0, "fixture mode sent a request");
}

/// Measurement, not a check (`cargo test --bin tasks -- --ignored --exact
/// ui::tests::parsed_snapshot_memory_cost --nocapture`, alone): resident memory one parsed
/// snapshot keeps, for a synthetic document shaped like a live one (51 jobs, ~80 KB). The
/// unbounded hand-over this replaced queued one such snapshot per feed every 2 s while the
/// window was not drawn.
#[test]
#[ignore]
fn parsed_snapshot_memory_cost() {
    fn rss_kb() -> i64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    }
    let path = format!("{FIXTURES}/ui-busy/laptop.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let template = doc["jobs"].as_array().unwrap().clone();
    let jobs: Vec<serde_json::Value> = (0..51)
        .map(|i| {
            let mut job = template[i % template.len()].clone();
            job["id"] = serde_json::json!(1000 + i);
            job["argv"] = serde_json::json!([
                "cargo",
                "test",
                "--locked",
                "-j",
                "2",
                format!("synthetic_filter_{i}_{}", "x".repeat(900))
            ]);
            job
        })
        .collect();
    doc["jobs"] = serde_json::Value::Array(jobs);
    let text = doc.to_string();
    let count = 200;
    let before = rss_kb();
    let kept: Vec<_> = (0..count)
        .map(|_| crate::ui_model::Snapshot::parse_str(&text).unwrap())
        .collect();
    let after = rss_kb();
    let per = (after - before) as f64 / f64::from(count);
    eprintln!(
        "parsed snapshot: document {} B, ~{per:.0} KiB resident each ({count} kept); \
         two feeds every 2 s = {:.0} MiB/h while undrained",
        text.len(),
        per * 3600.0 / 1024.0
    );
    assert_eq!(kept.len(), count as usize);
}
