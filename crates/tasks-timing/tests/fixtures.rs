//! Runs the parsers over trimmed real logs (job 4254 cargo, job 304 gradle) with synthetic clocks.

use tasks_timing::{CargoParser, Event, GradleParser, PhaseKind, TaskOutcome, Timeline};

const CARGO: &str = include_str!("fixtures/cargo-test-4254.log");
const GRADLE: &str = include_str!("fixtures/gradle-304.log");

fn secs_in(line: &str) -> Option<f64> {
    let rest = line.split("finished in ").nth(1)?;
    tasks_timing::dur::parse_secs(rest)
}

/// Timestamps: compile lines share the 51m03s the log reports; a test target takes what its
/// `test result:` line says; every other line takes 1ms.
fn cargo_timeline() -> Timeline {
    let lines: Vec<&str> = CARGO.lines().collect();
    let n_compiling = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("Compiling"))
        .count() as u64;
    let mut parser = CargoParser::new();
    let mut tl = Timeline::new();
    let (mut ts, mut pending_run) = (0u64, 0u64);
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("Compiling") {
            ts += 3_063_000 / n_compiling;
        } else if t.starts_with("Running") {
            pending_run = lines
                .get(i + 1..)
                .unwrap_or(&[])
                .iter()
                .find_map(|l| secs_in(l))
                .map_or(0, |s| (s * 1000.0) as u64);
        } else if t.starts_with("test result") {
            ts += pending_run;
        } else {
            ts += 1;
        }
        for ev in parser.feed(ts, line) {
            tl.record(ts, &ev);
        }
    }
    tl.finish(ts);
    tl
}

#[test]
fn cargo_job_4254_timeline() {
    let tl = cargo_timeline();
    let targets: Vec<_> = tl.phases().iter().filter(|p| p.is_target()).collect();
    assert_eq!(targets.len(), 22);
    assert!(targets.iter().all(|p| p.end_ms.is_some() && p.failed == 0));
    // The compile phase ends at `Finished`, about 51m03s after it started.
    let compile = tl.compile_ms() as f64 / 1000.0;
    assert!((3000.0..3080.0).contains(&compile), "{compile}");
    let slow = tl.slowest_targets(5);
    let names: Vec<&str> = slow.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "test:lifecycle",
            "test:group_call",
            "test:link",
            "test:idle_close",
            "test:private_rotation"
        ]
    );
    // Measured time tracks the harness-reported time.
    for p in slow {
        let measured = p.duration_ms(0) as f64 / 1000.0;
        let reported = p.reported_secs.unwrap_or(0.0);
        assert!(
            (measured - reported).abs() < 0.1,
            "{}: {measured} vs {reported}",
            p.name
        );
    }
    let blocking = targets
        .iter()
        .find(|p| p.name == "test:blocking_acceptance")
        .map(|p| (p.passed, p.ignored));
    assert_eq!(blocking, Some((17, 2)));
}

#[test]
fn gradle_job_304_timeline() {
    let mut parser = GradleParser::new();
    let mut tl = Timeline::new();
    for (i, line) in GRADLE.lines().enumerate() {
        let ts = i as u64 * 1000;
        for ev in parser.feed(ts, line) {
            tl.record(ts, &ev);
        }
    }
    tl.finish(GRADLE.lines().count() as u64 * 1000);
    let tasks: Vec<_> = tl
        .phases()
        .iter()
        .filter(|p| matches!(p.kind, PhaseKind::Task(_)))
        .collect();
    assert_eq!(tasks.len(), 45);
    assert_eq!(tasks[0].name, ":app:cargoNdkBuild");
    let outcome = |name: &str| {
        tasks
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.kind.clone())
    };
    assert_eq!(
        outcome(":app:compileDebugKotlin"),
        Some(PhaseKind::Task(TaskOutcome::UpToDate))
    );
    assert_eq!(
        outcome(":app:mergeDebugNativeDebugMetadata"),
        Some(PhaseKind::Task(TaskOutcome::Skipped))
    );
    // The cargo build inside the first task spans many log lines, so it is the longest task.
    assert!(tasks[0].duration_ms(0) > tasks[1].duration_ms(0));
    assert_eq!(tl.build, Some((true, Some(283.0))));
}

#[test]
fn fixture_is_small_and_clean() {
    for f in [CARGO, GRADLE] {
        assert!(f.len() < 100_000);
        let l = f.to_lowercase();
        assert!(!l.contains("password") && !l.contains("key=") && !l.contains("token"));
    }
    let mut p = CargoParser::new();
    assert!(
        CARGO
            .lines()
            .flat_map(|l| p.feed(0, l))
            .any(|e| matches!(e, Event::CompileFinished { .. }))
    );
}
