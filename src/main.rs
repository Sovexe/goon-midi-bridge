#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod client_link;
mod engine;

use bridge::Bridge;
use eframe::egui;
use engine::{
    MidiError, MidiFilePlayer, MidiRouter, RouterMessage, VelocityMode, DEFAULT_CHANNELS,
};
use midir::{MidiInput, MidiInputConnection};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

struct Playback {
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Playback {
    fn start(
        schedule: Vec<(f64, RouterMessage)>,
        router: Arc<Mutex<MidiRouter>>,
        output: broadcast::Sender<Value>,
        status: mpsc::Sender<String>,
    ) -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let start = Instant::now();
            let mut completed = true;
            for (at, message) in schedule {
                let wait = Duration::from_secs_f64(at).saturating_sub(start.elapsed());
                match receiver.recv_timeout(wait) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        completed = false;
                        break;
                    }
                }
                for event in router.lock().unwrap().handle(message) {
                    let _ = output.send(event);
                }
            }

            let mut router = router.lock().unwrap();
            for event in router.panic() {
                let _ = output.send(event);
            }
            router.finish_file();
            if completed {
                let _ = status.send("Playback finished.".into());
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct App {
    bridge: Bridge,
    router: Arc<Mutex<MidiRouter>>,
    midi_input: Option<MidiInputConnection<()>>,
    input_names: Vec<String>,
    input_index: usize,
    playback: Option<Playback>,
    file: Option<PathBuf>,
    transpose: i16,
    channels: [bool; 16],
    minimum_velocity: u8,
    tempo_percent: u16,
    velocity_mode: VelocityMode,
    velocity_boost: f64,
    velocity_offset: i16,
    status: String,
    status_tx: mpsc::Sender<String>,
    status_rx: mpsc::Receiver<String>,
    byond_clients: Vec<client_link::Client>,
    selected_client: usize,
    link_result: Option<mpsc::Receiver<Result<usize, String>>>,
}

impl App {
    fn new(bridge: Bridge) -> Self {
        let (status_tx, status_rx) = mpsc::channel();
        let router = bridge.router.clone();
        router
            .lock()
            .unwrap()
            .configure_velocity(VelocityMode::Auto, 1.5, 0)
            .unwrap();
        let mut app = Self {
            bridge,
            router,
            midi_input: None,
            input_names: Vec::new(),
            input_index: 0,
            playback: None,
            file: None,
            transpose: 0,
            channels: DEFAULT_CHANNELS,
            minimum_velocity: 1,
            tempo_percent: 100,
            velocity_mode: VelocityMode::Auto,
            velocity_boost: 1.5,
            velocity_offset: 0,
            status: "Open instruments, select BYOND, then click Link Instruments.".into(),
            status_tx,
            status_rx,
            byond_clients: client_link::clients(),
            selected_client: 0,
            link_result: None,
        };
        app.refresh_inputs();
        app
    }

    fn refresh_inputs(&mut self) {
        match MidiInput::new("Goon MIDI Bridge") {
            Ok(input) => {
                self.input_names = input
                    .ports()
                    .iter()
                    .map(|p| {
                        input
                            .port_name(p)
                            .unwrap_or_else(|_| "Unnamed MIDI input".into())
                    })
                    .collect();
                self.input_index = self
                    .input_index
                    .min(self.input_names.len().saturating_sub(1));
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn stop(&mut self) {
        self.midi_input.take();
        self.playback.take();
        let mut router = self.router.lock().unwrap();
        self.bridge.send(router.panic());
        router.finish_file();
    }

    fn panic(&mut self) {
        self.stop();
        let _ = self.bridge.events.send(json!({ "type": "panic" }));
        self.status = "Stopped all notes.".into();
    }

    fn open_midi_input(&self) -> Result<MidiInputConnection<()>, String> {
        let input = MidiInput::new("Goon MIDI Bridge").map_err(|error| error.to_string())?;
        let port = input
            .ports()
            .get(self.input_index)
            .cloned()
            .ok_or("Select a MIDI input first")?;
        let router = self.router.clone();
        let output = self.bridge.events.clone();
        input
            .connect(
                &port,
                "Goon MIDI Bridge",
                move |_, bytes, _| {
                    for event in router.lock().unwrap().handle_bytes(bytes) {
                        let _ = output.send(event);
                    }
                },
                (),
            )
            .map_err(|error| error.to_string())
    }

    fn connect(&mut self) {
        self.stop();
        let channel_info = self.router.lock().unwrap().reset_channel_names();
        self.bridge.send(vec![channel_info]);
        match self.open_midi_input() {
            Ok(connection) => {
                self.midi_input = Some(connection);
                self.status = "Live MIDI input connected.".into();
            }
            Err(error) => self.status = error,
        }
    }

    fn prepare_playback(&self, path: &Path) -> Result<Vec<(f64, RouterMessage)>, MidiError> {
        let bytes = MidiFilePlayer::read(path)?;
        let speed = f64::from(self.tempo_percent) / 100.0;
        let schedule = MidiFilePlayer::schedule(&bytes, speed)?;
        let mut router = self.router.lock().unwrap();
        router.prepare_file(&schedule);
        self.bridge.send(vec![router.describe_file(&schedule)]);
        Ok(schedule)
    }

    fn open_file(&mut self, path: PathBuf) {
        let schedule =
            MidiFilePlayer::read(&path).and_then(|bytes| MidiFilePlayer::schedule(&bytes, 1.0));
        match schedule {
            Ok(schedule) => {
                self.stop();
                self.file = Some(path);
                let channel_info = self.router.lock().unwrap().describe_file(&schedule);
                self.bridge.send(vec![channel_info]);
                self.status = "MIDI file loaded.".into();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn play(&mut self) {
        let Some(path) = self.file.clone() else {
            self.status = "Open a MIDI file first.".into();
            return;
        };
        self.stop();
        let schedule = match self.prepare_playback(&path) {
            Ok(schedule) => schedule,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        self.playback = Some(Playback::start(
            schedule,
            self.router.clone(),
            self.bridge.events.clone(),
            self.status_tx.clone(),
        ));
        self.status = format!(
            "Playing {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    fn instruments(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("byond_client")
                .selected_text(
                    self.byond_clients
                        .get(self.selected_client)
                        .map_or("Select BYOND client", |client| client.title.as_str()),
                )
                .width((ui.available_width() - 220.0).max(180.0))
                .show_ui(ui, |ui| {
                    for (index, client) in self.byond_clients.iter().enumerate() {
                        ui.selectable_value(&mut self.selected_client, index, &client.title);
                    }
                });
            if ui.button("Refresh").clicked() {
                self.byond_clients = client_link::clients();
                self.selected_client = 0;
            }
            if ui
                .add_enabled(
                    self.link_result.is_none() && !self.byond_clients.is_empty(),
                    egui::Button::new("Link Instruments"),
                )
                .clicked()
            {
                let client = self.byond_clients[self.selected_client].clone();
                let adapter_url = self.bridge.adapter_url.clone();
                let (link_tx, link_rx) = mpsc::channel();
                self.link_result = Some(link_rx);
                self.status = "Finding open instruments…".into();
                std::thread::spawn(move || {
                    let _ = link_tx.send(client_link::link(&client, &adapter_url));
                });
            }
        });
        let clients = self
            .bridge
            .clients
            .load(std::sync::atomic::Ordering::Relaxed);
        ui.label(format!("{clients} instruments connected"));
        ui.weak("Open instruments before linking. Enable their keyboard input to play.");
        ui.add_space(12.0);
    }

    fn sources(&mut self, ui: &mut egui::Ui) {
        section(ui, "Source", |ui| {
            ui.strong("Live MIDI Input");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("inputs")
                    .selected_text(
                        self.input_names
                            .get(self.input_index)
                            .map_or("No MIDI inputs found", String::as_str),
                    )
                    .width((ui.available_width() - 220.0).max(150.0))
                    .show_ui(ui, |ui| {
                        for (i, name) in self.input_names.iter().enumerate() {
                            ui.selectable_value(&mut self.input_index, i, name);
                        }
                    });
                if ui.button("Refresh").clicked() {
                    self.refresh_inputs();
                }
                if ui
                    .add_enabled(
                        !self.input_names.is_empty(),
                        egui::Button::new(if self.midi_input.is_some() {
                            "Disconnect"
                        } else {
                            "Connect"
                        }),
                    )
                    .clicked()
                {
                    if self.midi_input.is_some() {
                        self.stop();
                        self.status = "Input disconnected.".into();
                    } else {
                        self.connect();
                    }
                }
            });
            ui.add_space(8.0);
            ui.strong("MIDI File");
            let name = self
                .file
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "No file selected".into());
            let response = ui.label(name);
            if let Some(path) = &self.file {
                response.on_hover_text(path.display().to_string());
            }
            ui.horizontal(|ui| {
                if ui.button("Open File…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("MIDI", &["mid", "midi"])
                        .pick_file()
                    {
                        self.open_file(path);
                    }
                }
                if ui
                    .add_enabled(
                        self.file.is_some() && self.playback.is_none(),
                        egui::Button::new("Play"),
                    )
                    .clicked()
                {
                    self.play();
                }
                if ui
                    .add_enabled(self.playback.is_some(), egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop();
                    self.status = "Playback stopped.".into();
                }
                ui.label("Tempo");
                ui.add_enabled(
                    self.playback.is_none(),
                    egui::DragValue::new(&mut self.tempo_percent)
                        .range(25..=200)
                        .suffix("%"),
                );
            });
        });
    }

    fn dynamics(&mut self, ui: &mut egui::Ui) {
        let old_velocity = (
            self.velocity_mode,
            self.velocity_boost,
            self.velocity_offset,
        );
        section(ui, "Velocity", |ui| {
            ui.horizontal(|ui| {
                for (mode, label) in [
                    (VelocityMode::Auto, "Auto"),
                    (VelocityMode::Manual, "Manual"),
                    (VelocityMode::Original, "Original"),
                ] {
                    ui.selectable_value(&mut self.velocity_mode, mode, label);
                }
            });
            egui::Grid::new("velocity_controls")
                .num_columns(2)
                .spacing([18.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Auto offset");
                    ui.add_enabled(
                        self.velocity_mode == VelocityMode::Auto,
                        egui::DragValue::new(&mut self.velocity_offset).range(-64..=64),
                    );
                    ui.end_row();
                    ui.label("Manual boost");
                    ui.add_enabled(
                        self.velocity_mode == VelocityMode::Manual,
                        egui::DragValue::new(&mut self.velocity_boost)
                            .range(1.0..=40.0)
                            .speed(0.05)
                            .suffix("×"),
                    );
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.velocity_mode == VelocityMode::Auto,
                        egui::Button::new("Recalibrate"),
                    )
                    .clicked()
                {
                    self.router.lock().unwrap().recalibrate_velocity();
                }
            });
            let mut router = self.router.lock().unwrap();
            if old_velocity
                != (
                    self.velocity_mode,
                    self.velocity_boost,
                    self.velocity_offset,
                )
            {
                let _ = router.configure_velocity(
                    self.velocity_mode,
                    self.velocity_boost,
                    self.velocity_offset,
                );
            }
            ui.label(egui::RichText::new(router.velocity_status()).color(MUTED));
        });
    }

    fn routing(&mut self, ui: &mut egui::Ui) {
        let playing = self.midi_input.is_some() || self.playback.is_some();
        let mut changed = false;
        section(ui, "Routing", |ui| {
            if playing {
                ui.label("Stop playback or disconnect the input to change routing.");
            }
            ui.add_enabled_ui(!playing, |ui| {
                egui::Grid::new("routing")
                    .num_columns(2)
                    .spacing([28.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Transpose");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.transpose)
                                    .range(-36..=36)
                                    .suffix(" semitones"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Minimum Velocity");
                        changed |= ui
                            .add(egui::DragValue::new(&mut self.minimum_velocity).range(1..=127))
                            .changed();
                        ui.end_row();
                    });
            });
        });
        if changed {
            self.apply_routing();
        }
    }

    fn channel_controls(&mut self, ui: &mut egui::Ui) {
        let playing = self.midi_input.is_some() || self.playback.is_some();
        let names = self.router.lock().unwrap().channel_names().clone();
        let mut changed = false;
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                egui::CollapsingHeader::new("MIDI Channels")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.add_enabled_ui(!playing, |ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("All").clicked() {
                                    self.channels.fill(true);
                                    changed = true;
                                }
                                if ui.small_button("None").clicked() {
                                    self.channels.fill(false);
                                    changed = true;
                                }
                            });
                            ui.columns(2, |columns| {
                                for (channel, selected) in self.channels.iter_mut().enumerate() {
                                    let label = format!("{} · {}", channel + 1, names[channel]);
                                    changed |=
                                        columns[channel / 8].checkbox(selected, label).changed();
                                }
                            });
                        });
                        ui.weak("Names use General MIDI. Channel 10 is usually drums.");
                    });
            });
        if changed {
            self.apply_routing();
        }
    }

    fn apply_routing(&mut self) {
        if let Err(error) = self.router.lock().unwrap().configure_routing(
            self.channels,
            self.transpose,
            self.minimum_velocity,
        ) {
            self.status = error.to_string();
        }
    }
}

const MUTED: egui::Color32 = egui::Color32::from_rgb(160, 160, 160);

fn section(ui: &mut egui::Ui, title: &str, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(title);
            ui.add_space(6.0);
            contents(ui);
        });
}

fn configure_style(ctx: &egui::Context) -> std::io::Result<()> {
    let windows_dir = std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
    let font_path = PathBuf::from(windows_dir).join("Fonts/segoeui.ttf");
    let font = std::fs::read(&font_path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("Cannot load {}: {error}", font_path.display()),
        )
    })?;
    let mut fonts = egui::FontDefinitions::empty();
    fonts.font_data.insert(
        "Segoe UI".into(),
        Arc::new(egui::FontData::from_owned(font)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.insert(family, vec!["Segoe UI".into()]);
    }
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = egui::Color32::from_rgb(32, 32, 32);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(65, 85, 105);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    for text in [egui::TextStyle::Body, egui::TextStyle::Button] {
        style
            .text_styles
            .insert(text, egui::FontId::proportional(14.0));
    }
    ctx.set_style(style);
    Ok(())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(status) = self.status_rx.try_recv() {
            self.status = status;
        }
        if let Some(receiver) = &self.link_result {
            if let Ok(result) = receiver.try_recv() {
                self.link_result = None;
                self.status = match result {
                    Ok(count) => format!("Sent links to {count} open instrument windows."),
                    Err(error) => error,
                };
            }
        }
        if self.playback.as_ref().is_some_and(Playback::is_finished) {
            self.playback.take();
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(32, 32, 32))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("Stop All Notes")).clicked() {
                        self.panic();
                    }
                    ui.label(&self.status);
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(32, 32, 32))
                    .inner_margin(14.0),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.instruments(ui);
                        self.sources(ui);
                        ui.add_space(12.0);
                        ui.columns(2, |columns| {
                            self.dynamics(&mut columns[0]);
                            self.routing(&mut columns[1]);
                        });
                        ui.add_space(12.0);
                        self.channel_controls(ui);
                        ui.add_space(12.0);
                    });
            });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.panic();
    }
}

fn main() -> eframe::Result {
    let bridge = match Bridge::start() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Cannot start bridge: {e}");
            return Ok(());
        }
    };
    eframe::run_native(
        "Goon MIDI Bridge",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_icon(
                    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                        .expect("Invalid app icon"),
                )
                .with_inner_size([760.0, 720.0])
                .with_min_inner_size([720.0, 620.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            configure_style(&cc.egui_ctx)?;
            Ok(Box::new(App::new(bridge)))
        }),
    )
}
