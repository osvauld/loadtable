use super::*;

fn result(probe: Probe, stdout: &str) -> ProbeResult {
    ProbeResult {
        probe,
        status: ProbeStatus::Success,
        stdout: stdout.into(),
        stderr: String::new(),
        detail: "synthetic exit 0".into(),
    }
}

fn ready_report() -> ProbeReport {
    ProbeReport {
        results: vec![
            result(Probe::Architecture, "arm64\n"),
            result(Probe::MacosVersion, "14.5\n"),
            result(Probe::RustcVersion, "rustc 1.85.0 (synthetic)"),
            result(Probe::CargoVersion, "cargo 1.85.0 (synthetic)"),
            result(
                Probe::RustupShow,
                "installed toolchains\n--------------------\nstable-aarch64-apple-darwin (default)\n\nactive toolchain\n----------------\nstable-aarch64-apple-darwin (default)",
            ),
            result(
                Probe::RustTargets,
                "aarch64-apple-darwin\naarch64-apple-ios\n",
            ),
            result(
                Probe::DeveloperDirectory,
                "/Applications/Xcode.app/Contents/Developer\n",
            ),
            result(Probe::XcodeVersion, "Xcode 16.0\nBuild version 16A242d\n"),
            result(
                Probe::XcodeSdks,
                "macOS SDKs:\n macOS 15.0 -sdk macosx15.0\niOS SDKs:\n iOS 18.0 -sdk iphoneos18.0\niOS Simulator SDKs:\n Simulator -sdk iphonesimulator18.0\n",
            ),
            result(
                Probe::SimulatorRuntimes,
                r#"{"runtimes":[{"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-0","isAvailable":true}]}"#,
            ),
        ],
    }
}

fn capability(plan: &SetupPlan, name: &str) -> CapabilityState {
    plan.steps
        .iter()
        .find(|s| s.capability == name)
        .unwrap()
        .state
}

#[test]
fn synthetic_full_xcode_is_distinct_from_clt() {
    let requirements = Requirements {
        ios_sdk: true,
        simulator_runtime: true,
        ..Requirements::default()
    };
    let mut report = ready_report();
    let plan = assess(&requirements, &report).unwrap();
    assert!(
        plan.steps
            .iter()
            .all(|s| s.state == CapabilityState::Present)
    );
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::DeveloperDirectory)
        .unwrap()
        .stdout = "/Library/Developer/CommandLineTools\n".into();
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::XcodeVersion)
        .unwrap()
        .status = ProbeStatus::Failed;
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "Selected developer tools"),
        CapabilityState::Present
    );
    assert_eq!(
        capability(&plan, "Full Xcode (not CLT only)"),
        CapabilityState::Missing
    );
}

#[test]
fn failing_timed_out_or_unparsed_evidence_is_unknown() {
    let mut report = ready_report();
    for r in &mut report.results {
        if r.probe == Probe::RustcVersion {
            r.status = ProbeStatus::TimedOut;
        }
        if r.probe == Probe::XcodeSdks {
            r.status = ProbeStatus::Failed;
        }
        if r.probe == Probe::RustupShow {
            r.status = ProbeStatus::OutputLimit;
        }
        if r.probe == Probe::MacosVersion {
            r.stdout = "invalid.14".into();
        }
    }
    let plan = assess(&Requirements::default(), &report).unwrap();
    for name in [
        "rustc --version",
        "macOS SDK",
        "macOS minimum version",
        "Rust toolchain selection",
    ] {
        assert_eq!(capability(&plan, name), CapabilityState::Unknown);
    }
}

#[test]
fn missing_tools_are_not_conflated_with_probe_failure() {
    let mut report = ready_report();
    let rustc = report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::RustcVersion)
        .unwrap();
    rustc.status = ProbeStatus::Missing;
    let plan = assess(&Requirements::default(), &report).unwrap();
    assert_eq!(
        capability(&plan, "rustc --version"),
        CapabilityState::Missing
    );
}

#[test]
fn version_components_are_numeric_and_missing_components_zero_filled() {
    let mut requirements = Requirements {
        minimum_macos: "14.10".into(),
        ..Requirements::default()
    };
    let mut report = ready_report();
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "macOS minimum version"),
        CapabilityState::Unsupported
    );
    requirements.minimum_macos = "14".into();
    let os = report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::MacosVersion)
        .unwrap();
    os.stdout = "14.0.0".into();
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "macOS minimum version"),
        CapabilityState::Present
    );
    for v in ["", "14.beta", "14.0.0.1", "-1", "4294967296", " 14"] {
        requirements.minimum_macos = v.into();
        assert!(requirements.validate().is_err(), "{v}");
    }
}

#[test]
fn intel_or_rosetta_does_not_prove_native_arm64() {
    let mut report = ready_report();
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::Architecture)
        .unwrap()
        .stdout = "x86_64\n".into();
    let plan = assess(&Requirements::default(), &report).unwrap();
    assert_eq!(
        capability(&plan, "Apple Silicon native host"),
        CapabilityState::Unsupported
    );
}

#[test]
fn targets_are_exact_and_manual_fixes_only_for_known_missing_target() {
    let requirements = Requirements {
        rust_targets: vec!["aarch64-apple-ios-sim".into()],
        ..Requirements::default()
    };
    let report = ready_report();
    let plan = assess(&requirements, &report).unwrap();
    let target = plan
        .steps
        .iter()
        .find(|s| s.capability == "Rust target aarch64-apple-ios-sim")
        .unwrap();
    assert_eq!(target.state, CapabilityState::Missing);
    assert_eq!(
        target.manual_commands[0].display(),
        "'rustup' 'target' 'add' 'aarch64-apple-ios-sim'"
    );
    let plan = assess(&requirements, &ProbeReport::default()).unwrap();
    let target = plan
        .steps
        .iter()
        .find(|s| s.capability == "Rust target aarch64-apple-ios-sim")
        .unwrap();
    assert_eq!(target.state, CapabilityState::Unknown);
    assert!(target.manual_commands.is_empty());
}

#[test]
fn runtime_is_optional_and_requires_available_ios_not_any_runtime() {
    let mut requirements = Requirements::default();
    let mut report = ready_report();
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "Available iOS simulator runtime"),
        CapabilityState::NotRequested
    );
    requirements.simulator_runtime = true;
    let r = report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::SimulatorRuntimes)
        .unwrap();
    r.stdout = r#"{"runtimes":[{"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-11-0","isAvailable":true},{"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-0","isAvailable":false}]}"#.into();
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "Available iOS simulator runtime"),
        CapabilityState::Missing
    );
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::SimulatorRuntimes)
        .unwrap()
        .stdout = "{}".into();
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "Available iOS simulator runtime"),
        CapabilityState::Unknown
    );
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::SimulatorRuntimes)
        .unwrap()
        .status = ProbeStatus::Skipped;
    let plan = assess(&requirements, &report).unwrap();
    assert_eq!(
        capability(&plan, "Available iOS simulator runtime"),
        CapabilityState::Unknown
    );
}

#[test]
fn unknown_schema_keys_versions_duplicates_and_commands_are_rejected() {
    let requirements = serde_json::to_value(Requirements::default()).unwrap();
    for (key, value) in [
        ("schema_version", serde_json::json!(2)),
        ("probe", serde_json::json!("sh -c malicious")),
        ("command", serde_json::json!(["rm", "-rf", "/"])),
        (
            "rust_targets",
            serde_json::json!(["aarch64-apple-darwin;touch /tmp/no"]),
        ),
        (
            "rust_targets",
            serde_json::json!(["aarch64-apple-darwin", "aarch64-apple-darwin"]),
        ),
        ("minimum_macos", serde_json::json!(null)),
    ] {
        let mut invalid = requirements.clone();
        invalid[key] = value;
        assert!(
            Requirements::from_json(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{key}"
        );
    }
    let json = serde_json::to_string(&requirements).unwrap();
    let duplicate = json.replacen('{', "{\"schema_version\":1,", 1);
    assert!(Requirements::from_json(duplicate.as_bytes()).is_err());
    let plan = assess(&Requirements::default(), &ready_report()).unwrap();
    let mut json = serde_json::to_value(plan).unwrap();
    json["steps"][0]["execute"] = serde_json::json!(true);
    assert!(PlanDocument::from_json(&serde_json::to_vec(&json).unwrap()).is_err());
    json["steps"][0].as_object_mut().unwrap().remove("execute");
    json["steps"][0]["state"] = serde_json::json!("ready_to_execute");
    assert!(PlanDocument::from_json(&serde_json::to_vec(&json).unwrap()).is_err());
}

#[test]
fn document_digest_binds_exact_received_bytes_not_a_reserialized_approximation() {
    let plan = assess(&Requirements::default(), &ready_report()).unwrap();
    let compact = serde_json::to_vec(&plan).unwrap();
    let pretty = serde_json::to_vec_pretty(&plan).unwrap();
    let a = PlanDocument::from_json(&compact).unwrap();
    let b = PlanDocument::from_json(&pretty).unwrap();
    assert_eq!(a.plan(), b.plan());
    assert_ne!(a.sha256(), b.sha256());
    assert_eq!(a.exact_bytes(), compact);
    assert_eq!(a.sha256(), format!("{:x}", Sha256::digest(&compact)));
    assert_eq!(
        a.sha256(),
        PlanDocument::from_reader(&compact[..]).unwrap().sha256()
    );
    let mut changed = plan.clone();
    changed.steps[0].instructions.push_str(" Changed.");
    assert_ne!(
        plan.document().unwrap().sha256(),
        changed.document().unwrap().sha256()
    );
}

#[test]
fn untrusted_manual_commands_can_be_loaded_and_rendered_but_never_execute() {
    let sentinel = std::env::temp_dir().join(format!("setup-no-execute-{}", std::process::id()));
    assert!(!sentinel.exists());
    let mut plan = assess(&Requirements::default(), &ProbeReport::default()).unwrap();
    plan.steps[0].manual_commands.push(ManualCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("touch '{}'", sentinel.display())],
    });
    let document = plan.document().unwrap();
    let loaded = PlanDocument::from_reader(document.exact_bytes()).unwrap();
    let rendered = loaded.plan().render_text().unwrap();
    assert!(rendered.contains("UNTRUSTED DISPLAY DATA"));
    assert!(rendered.contains("touch"));
    assert!(!sentinel.exists());
}

#[test]
fn shell_display_quotes_metacharacters_and_preserves_empty_arguments() {
    let command = ManualCommand {
        program: "not executed".into(),
        args: vec!["$(touch /tmp/no)".into(), "a'b".into(), String::new()],
    };
    assert_eq!(
        command.display(),
        "'not executed' '$(touch /tmp/no)' 'a'\\''b' ''"
    );
}

#[test]
fn lengths_controls_and_ambiguous_evidence_are_bounded() {
    assert!(Requirements::from_json(&vec![b' '; MAX_DOCUMENT_BYTES + 1]).is_err());
    assert!(PlanDocument::from_reader(&vec![b' '; MAX_DOCUMENT_BYTES + 1][..]).is_err());
    let mut requirements = Requirements {
        label: "x".repeat(161),
        ..Requirements::default()
    };
    assert!(requirements.validate().is_err());
    requirements.label = "spoof\u{202e}fdp".into();
    assert!(requirements.validate().is_err());
    let mut plan = assess(&Requirements::default(), &ready_report()).unwrap();
    plan.steps[0].evidence = "x".repeat(4097);
    assert!(plan.validate().is_err());
    plan.steps[0].evidence = "hello\u{1b}[2J".into();
    assert!(plan.validate().is_err());
    let mut report = ready_report();
    report.results.push(result(Probe::Architecture, "arm64"));
    report
        .results
        .iter_mut()
        .find(|r| r.probe == Probe::CargoVersion)
        .unwrap()
        .stderr = "\u{1b}[2J\u{202e}".repeat(4000);
    let plan = assess(&Requirements::default(), &report).unwrap();
    assert_eq!(
        capability(&plan, "Apple Silicon native host"),
        CapabilityState::Unknown
    );
    assert!(plan.steps.iter().all(|s| s.evidence.len() <= 4096));
    let rendered = plan.render_text().unwrap();
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.contains('\u{202e}'));
    assert!(rendered.contains("[truncated]"));
}

#[test]
fn xcode_sdk_and_runtime_success_must_have_recognizable_contents() {
    let mut report = ready_report();
    for r in &mut report.results {
        if matches!(
            r.probe,
            Probe::XcodeVersion
                | Probe::XcodeSdks
                | Probe::RustcVersion
                | Probe::CargoVersion
                | Probe::RustupShow
        ) {
            r.stdout = match r.probe {
                Probe::RustcVersion => "rustc not-a-version",
                Probe::CargoVersion => "cargo not-a-version",
                Probe::XcodeVersion => "Xcode not-a-version\nBuild version synthetic",
                _ => "unrecognized successful output",
            }
            .into();
        }
    }
    let plan = assess(&Requirements::default(), &report).unwrap();
    for name in [
        "Full Xcode (not CLT only)",
        "macOS SDK",
        "rustc --version",
        "cargo --version",
        "Rust toolchain selection",
    ] {
        assert_eq!(capability(&plan, name), CapabilityState::Unknown, "{name}");
    }
}

#[test]
fn rustup_without_active_toolchain_is_missing_not_ready() {
    let mut report = ready_report();
    report.results.iter_mut().find(|r| r.probe == Probe::RustupShow).unwrap().stdout =
        "installed toolchains\n--------------------\n\nactive toolchain\n----------------\nno active toolchain".into();
    let plan = assess(&Requirements::default(), &report).unwrap();
    assert_eq!(
        capability(&plan, "Rust toolchain selection"),
        CapabilityState::Missing
    );
}

#[test]
fn requirements_invariants_and_plan_step_limits_are_checked() {
    let mut requirements = Requirements {
        full_xcode: false,
        ios_sdk: true,
        ..Requirements::default()
    };
    assert!(requirements.validate().is_err());
    requirements.ios_sdk = false;
    requirements.simulator_runtime = true;
    assert!(requirements.validate().is_err());
    let mut plan = assess(&Requirements::default(), &ProbeReport::default()).unwrap();
    plan.steps.push(plan.steps[0].clone());
    assert!(plan.validate().is_err());
    plan.steps = vec![plan.steps[0].clone(); MAX_STEPS + 1];
    assert!(plan.validate().is_err());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn non_macos_local_preflight_runs_no_tools() {
    let report = run_local_preflight(ProbeOptions {
        include_simulator_runtime: true,
    });
    assert_eq!(report.results.len(), Probe::ALL.len());
    assert!(
        report
            .results
            .iter()
            .all(|r| r.status == ProbeStatus::Unsupported)
    );
    let plan = assess(&Requirements::default(), &report).unwrap();
    assert_eq!(
        capability(&plan, "Full Xcode (not CLT only)"),
        CapabilityState::Unsupported
    );
}
