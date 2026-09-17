//! Pre-session settings launcher (egui / eframe).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use eframe::egui;
use lansec_protocol::ChromaPref;
use lansec_session::{SessionConfig, SessionMode};

/// Open the launcher. Returns `Some(config)` when the user clicks Start, `None` on Quit/close.
pub fn run_launcher() -> Result<Option<SessionConfig>> {
    let started = Arc::new(Mutex::new(None::<SessionConfig>));
    let started_ui = Arc::clone(&started);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 460.0])
            .with_min_inner_size([380.0, 400.0])
            .with_title("lansec"),
        ..Default::default()
    };
    eframe::run_native(
        "lansec",
        options,
        Box::new(move |_cc| Ok(Box::new(LauncherApp::new(started_ui)))),
    )
    .map_err(|e| anyhow::anyhow!("launcher: {e}"))?;
    let out = started.lock().unwrap().take();
    Ok(out)
}

struct LauncherApp {
    cfg: SessionConfig,
    error: String,
    started: Arc<Mutex<Option<SessionConfig>>>,
}

impl LauncherApp {
    fn new(started: Arc<Mutex<Option<SessionConfig>>>) -> Self {
        Self {
            cfg: SessionConfig::load(),
            error: String::new(),
            started,
        }
    }

    fn try_start(&mut self, ctx: &egui::Context) {
        self.cfg.clamp();
        if let Err(e) = self.cfg.validate_for_start() {
            self.error = e.to_string();
            return;
        }
        if let Err(e) = self.cfg.save() {
            self.error = format!("save config: {e}");
            return;
        }
        *self.started.lock().unwrap() = Some(self.cfg.clone());
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("lansec");
            ui.label("Bidirectional LAN remote desktop — either machine can Host or Client");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Mode");
                ui.selectable_value(&mut self.cfg.mode, SessionMode::Host, "Host (share this PC)");
                ui.selectable_value(
                    &mut self.cfg.mode,
                    SessionMode::Client,
                    "Client (view remote)",
                );
            });
            ui.weak(match self.cfg.mode {
                SessionMode::Host => "This PC encodes the desktop; the other side connects in.",
                SessionMode::Client => "Connect to a Host. FPS/Mbps here are hints; Host owns encode.",
            });

            ui.add_space(4.0);
            match self.cfg.mode {
                SessionMode::Host => {
                    ui.horizontal(|ui| {
                        ui.label("Bind");
                        ui.text_edit_singleline(&mut self.cfg.bind);
                    });
                }
                SessionMode::Client => {
                    ui.horizontal(|ui| {
                        ui.label("Connect");
                        ui.text_edit_singleline(&mut self.cfg.connect);
                    });
                }
            }

            ui.horizontal(|ui| {
                ui.label("PIN");
                ui.text_edit_singleline(&mut self.cfg.pin);
            });

            ui.add_space(8.0);
            ui.separator();
            ui.label("Video");

            ui.add(
                egui::Slider::new(&mut self.cfg.target_fps, 15..=120)
                    .text("Target FPS")
                    .suffix(" fps"),
            );
            ui.add(
                egui::Slider::new(&mut self.cfg.target_mbps, 5..=200)
                    .text("Target Mbps")
                    .suffix(" Mbps"),
            );
            ui.add(
                egui::Slider::new(&mut self.cfg.min_mbps, 1..=200)
                    .text("Min Mbps")
                    .suffix(" Mbps"),
            );
            ui.add(
                egui::Slider::new(&mut self.cfg.max_mbps, 5..=300)
                    .text("Max Mbps")
                    .suffix(" Mbps"),
            );

            ui.horizontal(|ui| {
                ui.label("Chroma");
                ui.selectable_value(&mut self.cfg.chroma, ChromaPref::Auto, ChromaPref::Auto.label());
                ui.selectable_value(
                    &mut self.cfg.chroma,
                    ChromaPref::Yuv420,
                    ChromaPref::Yuv420.label(),
                );
                ui.selectable_value(
                    &mut self.cfg.chroma,
                    ChromaPref::Yuv444,
                    ChromaPref::Yuv444.label(),
                );
            });
            ui.weak("Host-only setting. Auto: Mac→Win prefers 4:2:0; Win→Mac prefers 4:4:4.");

            if !self.error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), &self.error);
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Start").clicked() {
                    self.try_start(ctx);
                }
                if ui.button("Quit").clicked() {
                    *self.started.lock().unwrap() = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("config: {}", SessionConfig::config_path().display()));
                });
            });
        });
    }
}

/// Convenience for tests / tooling — validate without opening a window.
pub fn validate_config(cfg: &SessionConfig) -> Result<()> {
    cfg.validate_for_start().context("session config")
}
