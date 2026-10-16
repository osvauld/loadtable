use super::*;

fn fixture() -> Dashboard {
    let (tx, rx) = mpsc::channel();
    let spec = JobSpec {
        suite: tinline_task_manager::catalog::SUITE.into(),
        configuration: tinline_task_manager::catalog::CONFIGURATION.into(),
        cost: Cost::Quick,
        owner: "layout-fixture".into(),
        source: "/synthetic/repository".into(),
        revision: "a".repeat(40),
    };
    Dashboard {
        socket: PathBuf::from("/unused-layout-fixture.sock"),
        tx,
        rx,
        next_seq: 0,
        applied_seq: 0,
        poll_in_flight: false,
        last_poll: None,
        last_update: None,
        response: Some(Response {
            version: tinline_task_manager::PROTOCOL_VERSION,
            execution_enabled: false,
            submitted_id: None,
            jobs: [Phase::Queued, Phase::Approved, Phase::WaitingBuild]
                .into_iter()
                .enumerate()
                .map(|(index, phase)| Job {
                    id: index as i64 + 1,
                    spec: spec.clone(),
                    phase,
                    submitted_at: unix_now(),
                    execution: None,
                })
                .collect(),
            estimates: Vec::new(),
            error: None,
            log: None,
        }),
        estimates: HashMap::new(),
        status: None,
        cancelling: Vec::new(),
        submitting: false,
        show_cancelled: false,
        form: Form {
            suite: spec.suite,
            owner: spec.owner,
            source: spec.source,
            revision: spec.revision,
            configuration: spec.configuration,
            cost: spec.cost,
        },
        log: None,
        raw_log: false,
        devices: Devices {
            scan: None,
            pending: None,
            checked: None,
            source: String::new(),
        },
    }
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
        let mut dashboard = fixture();
        dashboard.devices.scan = Some(scan);
        dashboard.devices.source = "/synthetic/app".into();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::SidePanel::left("form")
                .exact_width(250.0)
                .show(context, |ui| dashboard.devices(ui));
        });
        // Rendering never scans or launches anything by itself.
        assert!(dashboard.devices.pending.is_none());
        assert!(dashboard.status.is_none());
    }
}

#[test]
fn essential_job_columns_fit_compact_windows_without_wrapped_actions() {
    for width in [760.0, 1060.0, 1180.0] {
        let context = egui::Context::default();
        let mut dashboard = fixture();
        // Let egui's grid column measurements settle before checking geometry.
        let mut output = None;
        for _ in 0..3 {
            output = Some(context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 640.0),
                    )),
                    ..Default::default()
                },
                |context| {
                    egui::SidePanel::left("form")
                        .exact_width(250.0)
                        .resizable(false)
                        .show(context, |ui| dashboard.form(ui, context));
                    egui::CentralPanel::default().show(context, |ui| dashboard.jobs(ui, context));
                },
            ));
        }
        let output = output.unwrap();
        for label in ["Elapsed", "Est. runtime", "Approve", "Cancel", "Logs"] {
            let texts: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::epaint::Shape::Text(text) = &shape.shape {
                        (text.galley.text() == label).then_some((shape.clip_rect, text))
                    } else {
                        None
                    }
                })
                .collect();
            assert!(!texts.is_empty(), "missing {label} at width {width}");
            for (clip, text) in texts {
                let right = text.pos.x + text.galley.size().x;
                assert!(
                    right <= width && right <= clip.right(),
                    "{label} clipped at width {width}: right={right}, clip={clip:?}"
                );
                assert_eq!(
                    text.galley.rows.len(),
                    1,
                    "{label} wrapped at width {width}"
                );
            }
        }
        assert_eq!(dashboard.next_seq, 0, "layout test must not issue requests");
    }
}
