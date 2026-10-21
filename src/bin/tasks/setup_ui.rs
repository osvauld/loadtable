//! Integration hook: `#[cfg(feature = "ui")] mod setup_ui;` in tasks/main.rs,
//! then call `setup_ui::run(requirements, optional_received_plan)` from an explicit
//! setup UI entry point. No daemon, peer or installed runtime is contacted.

use eframe::egui;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use tinline_task_manager::setup::{
    self, PlanDocument, ProbeOptions, ProbeReport, Requirements, SetupError,
};

/// Can also be embedded in an existing tab via `show`. Construction and rendering
/// are pure with respect to external processes; only the labelled local button
/// runs the closed preflight probe set, off the UI thread.
pub struct SetupScreen {
    requirements: Requirements,
    local: PlanDocument,
    received: Option<PlanDocument>,
    pending: Option<Receiver<ProbeReport>>,
    include_simulator_runtime: bool,
    error: Option<String>,
    checked: bool,
}

impl SetupScreen {
    pub fn new(
        requirements: Requirements,
        received: Option<PlanDocument>,
    ) -> Result<Self, SetupError> {
        let local = setup::assess(&requirements, &ProbeReport::default())?.document()?;
        Ok(Self {
            requirements,
            local,
            received,
            pending: None,
            include_simulator_runtime: false,
            error: None,
            checked: false,
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll();
        ui.heading("Apple Silicon worker setup");
        ui.label("Read-only onboarding evidence. No remote automatic setup implemented.");
        ui.label("No install, root operation, licence acceptance, simulator boot or service restart is performed.");
        ui.label("Loading and viewing requirements/plans never runs probes or supplied commands.");
        ui.separator();
        ui.label(format!(
            "Local assessment requirements: {}",
            self.requirements.label
        ));
        ui.label(format!(
            "macOS >= {}; targets: {}; full Xcode: {}; iOS SDK: {}; simulator runtime: {}",
            self.requirements.minimum_macos,
            self.requirements.rust_targets.join(", "),
            self.requirements.full_xcode,
            self.requirements.ios_sdk,
            self.requirements.simulator_runtime,
        ));
        ui.checkbox(
            &mut self.include_simulator_runtime,
            "Opt in to local simctl runtime listing (may start CoreSimulator services)",
        );
        ui.label("Fixed probes only: OS architecture/version; ~/.cargo/bin rustc/cargo/rustup; system-selected Xcode directory/version/SDKs. 5s and 32 KiB per output stream per tool. No remote probe definitions.");
        ui.label("Probes trust your installed tools; they ignore project toolchain and DEVELOPER_DIR/SDKROOT overrides. Evidence is not a build/link test or permission to run jobs.");
        let pending = self.pending.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !pending,
                    egui::Button::new("Run known LOCAL read-only preflight"),
                )
                .clicked()
            {
                self.start();
            }
            if pending {
                ui.spinner();
                ui.label("Checking local tools (up to about 51 seconds)…");
            } else if !self.checked {
                ui.label("Not probed yet");
            } else {
                ui.label("Local scan complete; evidence can become stale");
            }
        });
        if pending {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(210, 80, 60), error);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Local evidence and manual fixes");
            draw_document(ui, &self.local, "local");
            if let Some(document) = &self.received {
                ui.separator();
                ui.heading("Received plan — UNTRUSTED claims, not local evidence");
                ui.label("This document is shown verbatim as display data. Its claims have not been verified. Do not execute commands without independent review.");
                draw_document(ui, document, "received");
            }
        });
    }

    fn start(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let options = ProbeOptions {
            include_simulator_runtime: self.include_simulator_runtime,
        };
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("setup-local-preflight".into())
            .spawn(move || {
                let _ = tx.send(setup::run_local_preflight(options));
            }) {
            Ok(_) => {
                self.pending = Some(rx);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Cannot start local scan: {error}")),
        }
    }

    fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(report) => {
                self.pending = None;
                match setup::assess(&self.requirements, &report).and_then(|plan| plan.document()) {
                    Ok(document) => {
                        self.local = document;
                        self.checked = true;
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                self.error = Some("Local probe thread stopped without a report".into());
            }
            Err(TryRecvError::Empty) => {}
        }
    }
}

fn draw_document(ui: &mut egui::Ui, document: &PlanDocument, scope: &str) {
    ui.label(format!("Document SHA-256: {}", document.sha256()));
    ui.small("Digest binds exact JSON bytes only. It is not authentication, freshness, safety or an enforced approval.");
    ui.horizontal(|ui| {
        if ui.button("Copy exact plan JSON").clicked() {
            // PlanDocument can only contain successfully parsed UTF-8 JSON.
            ui.ctx()
                .copy_text(String::from_utf8_lossy(document.exact_bytes()).into_owned());
        }
        if ui.button("Copy digest").clicked() {
            ui.ctx().copy_text(document.sha256().to_owned());
        }
    });
    for (index, step) in document.plan().steps.iter().enumerate() {
        egui::CollapsingHeader::new(format!("{:?} — {}", step.state, step.capability))
            .id_salt((scope, index))
            .default_open(step.state != setup::CapabilityState::Present)
            .show(ui, |ui| {
                ui.label("Evidence (may be untrusted tool output):");
                ui.monospace(&step.evidence);
                ui.label(&step.instructions);
                for command in &step.manual_commands {
                    let display = command.display();
                    ui.monospace(&display);
                    if ui
                        .button("Copy exact manual command (does NOT execute)")
                        .clicked()
                    {
                        ui.ctx().copy_text(display);
                    }
                }
            });
    }
}

impl eframe::App for SetupScreen {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| self.show(ui));
    }
}

/// Opens a standalone setup window. Merely opening it does not probe the host.
pub fn run(requirements: Requirements, received: Option<PlanDocument>) -> Result<(), String> {
    let screen = SetupScreen::new(requirements, received).map_err(|e| e.to_string())?;
    eframe::run_native(
        "Task manager — local macOS setup",
        eframe::NativeOptions::default(),
        Box::new(move |_| Ok(Box::new(screen))),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructing_and_drawing_untrusted_plan_never_starts_preflight() {
        let requirements = Requirements::default();
        let mut plan = setup::assess(&requirements, &ProbeReport::default()).unwrap();
        plan.steps[0].manual_commands.push(setup::ManualCommand {
            program: "untrusted-program-never-executed".into(),
            args: vec!["$(no-shell-evaluation)".into()],
        });
        let document = plan.document().unwrap();
        let digest = document.sha256().to_owned();
        let mut screen = SetupScreen::new(requirements, Some(document)).unwrap();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| screen.show(ui));
        });
        assert!(screen.pending.is_none());
        assert!(!screen.checked);
        assert_eq!(screen.received.as_ref().unwrap().sha256(), digest);
        assert!(screen.error.is_none());
    }
}
