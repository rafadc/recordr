//! Window UI: a list of virtual devices on the left, the selected one's sources on the right.
// Device configs persist via eframe storage (~/Library/Application Support/recordr);
// devices that were live at quit restart on launch, and the app registers itself as a login item.

use crate::{
    Loopback, RunArgs, input_devices,
    meter::Meter,
    processes,
    record::{self, Recorder},
    tray::{self, Tray},
};
use eframe::egui;
use global_hotkey::hotkey::HotKey;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Device {
    args: RunArgs,
    rec: Option<Recorder>, // before `live`: fields drop in order, so recording stops first
    live: Option<Loopback>,
}

impl Device {
    fn start(&mut self) -> Result<(), String> {
        self.live = Some(Loopback::create(&self.args)?);
        Ok(())
    }

    /// Stop recording, or start it (starting the device first if needed).
    fn toggle_record(&mut self, dir: &str) -> Result<(), String> {
        if self.rec.take().is_some() {
            return Ok(()); // Drop finalizes the file
        }
        if self.live.is_none() {
            self.start()?;
        }
        let dev = self.live.as_ref().map_or(0, |l| l.dev_id);
        self.rec = Some(Recorder::start(
            dev,
            record::file_path(dir, &self.args.name),
        )?);
        Ok(())
    }
}

/// Global shortcuts (Carbon hot keys: no Accessibility permission needed). Presses arrive on
/// the main thread, get queued, and wake `App::logic`, which runs even while the window is hidden.
struct Hotkeys {
    manager: global_hotkey::GlobalHotKeyManager,
    registered: Vec<(String, Option<HotKey>)>, // per device: its shortcut, and the hotkey if it registered
    pressed: std::sync::mpsc::Receiver<u32>,
}

impl Hotkeys {
    fn new(ctx: &egui::Context) -> Option<Self> {
        let manager = global_hotkey::GlobalHotKeyManager::new()
            .map_err(|e| eprintln!("shortcuts: {e}"))
            .ok()?;
        let (tx, pressed) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        global_hotkey::GlobalHotKeyEvent::set_event_handler(Some(
            move |e: global_hotkey::GlobalHotKeyEvent| {
                if e.state == global_hotkey::HotKeyState::Pressed {
                    let _ = tx.send(e.id);
                    ctx.request_repaint();
                }
            },
        ));
        Some(Hotkeys {
            manager,
            registered: vec![],
            pressed,
        })
    }

    /// Re-register when any device's shortcut changed. Returns the first error.
    fn sync(&mut self, devices: &[Device]) -> Option<String> {
        let wanted: Vec<&str> = devices.iter().map(|d| d.args.shortcut.as_str()).collect();
        if wanted.iter().eq(self.registered.iter().map(|(s, _)| s)) {
            return None;
        }
        for (_, h) in self.registered.drain(..) {
            if let Some(h) = h {
                let _ = self.manager.unregister(h);
            }
        }
        let mut error = None;
        for s in wanted {
            let h = (!s.is_empty()).then(|| {
                let h: HotKey = s.parse().map_err(|e| format!("Shortcut {s}: {e}"))?;
                self.manager
                    .register(h)
                    .map_err(|e| format!("Shortcut {s}: {e}"))?;
                Ok::<_, String>(h)
            });
            let h = match h {
                Some(Ok(h)) => Some(h),
                Some(Err(e)) => {
                    error.get_or_insert(e);
                    None
                }
                None => None,
            };
            self.registered.push((s.to_string(), h));
        }
        error
    }

    /// Indices of devices whose shortcut was pressed since the last call.
    fn take_pressed(&self) -> Vec<usize> {
        self.pressed
            .try_iter()
            .filter_map(|id| {
                self.registered
                    .iter()
                    .position(|(_, h)| h.is_some_and(|h| h.id() == id))
            })
            .collect()
    }
}

#[derive(Default)]
struct App {
    devices: Vec<Device>,
    selected: usize,
    apps: Vec<AppEntry>,
    inputs: Vec<AppEntry>, // bundle = device uid
    error: Option<String>,
    login_registered: bool,
    settings_open: bool,
    perms: [Perm; 2],      // re-checked on every refresh
    show_continuity: bool, // setting: list iPhone (Continuity) mics as inputs
    dark: bool,            // setting: Dark vs Bright theme
    record_dir: String,    // setting: where Record saves WAVs; `~` allowed
    show_in: ShowIn,       // setting: Dock icon, menu bar icon, or both
    tray: Option<Tray>,    // the menu bar icon, when shown
    quitting: bool,        // Quit from the menu bar: let the close through
    hotkeys: Option<Hotkeys>,
    capturing: Option<usize>, // device whose record shortcut is being typed
    // Live level meters keyed by AppEntry::meter; None = failed, retried on the next refresh.
    meters: HashMap<String, Option<Meter>>,
    icons: HashMap<String, Option<egui::TextureHandle>>,
    installed: HashMap<String, Option<(String, egui::TextureHandle)>>, // closed apps: name + icon
    last_refresh: Option<Instant>,
}

struct AppEntry {
    bundle: String,
    name: String,
    playing: bool,
    icon: Option<egui::TextureHandle>,
    symbol: bool,               // monochrome SF Symbol: tint with the text color
    meter: Option<String>,      // key into App::meters; None = not metered
    note: Option<&'static str>, // small grey subtitle under the name
}

const KEY: &str = "devices";
const RUNNING: &str = "running";
const LOGIN: &str = "login_item_registered";
const SHOW_CONTINUITY: &str = "show_continuity";
const DARK: &str = "dark";
const RECORD_DIR: &str = "record_dir";
const SHOW_IN: &str = "show_in";

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
enum ShowIn {
    #[default]
    Both,
    Dock,
    MenuBar,
}

// Palette shared with the website (docs/index.html).
const GRAPHITE: egui::Color32 = egui::Color32::from_rgb(0x33, 0x33, 0x33);
const CORAL: egui::Color32 = egui::Color32::from_rgb(0xEE, 0x76, 0x72);
const TANGERINE: egui::Color32 = egui::Color32::from_rgb(0xF7, 0xA2, 0x6F);
const CINNABAR: egui::Color32 = egui::Color32::from_rgb(0xE9, 0x54, 0x50);

/// Bright and Dark visuals in the site's palette; `ctx.set_theme` switches between them.
fn install_theme(ctx: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let dark = theme == egui::Theme::Dark;
        let mut v = theme.default_visuals();
        let (bg, card, field) = if dark {
            (
                egui::Color32::from_rgb(0x26, 0x26, 0x26),
                GRAPHITE,
                egui::Color32::from_rgb(0x1C, 0x1C, 0x1C),
            )
        } else {
            (
                egui::Color32::from_rgb(0xFC, 0xFA, 0xF8),
                egui::Color32::from_rgb(0xF4, 0xEC, 0xE6),
                egui::Color32::WHITE,
            )
        };
        v.panel_fill = bg;
        v.window_fill = bg;
        v.faint_bg_color = card;
        v.extreme_bg_color = field;
        v.selection.bg_fill = CORAL;
        v.selection.stroke.color = egui::Color32::WHITE;
        v.hyperlink_color = CINNABAR;
        v.widgets.hovered.weak_bg_fill = TANGERINE.gamma_multiply(0.35);
        v.widgets.hovered.bg_stroke.color = CORAL;
        v.widgets.active.weak_bg_fill = TANGERINE;
        ctx.set_visuals_of(theme, v);
    }
}

pub fn run() -> Result<(), String> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 440.0]),
        ..Default::default()
    };
    let creator: eframe::AppCreator = Box::new(|cc| {
        let mut app = App::default();
        let saved: Vec<RunArgs> = cc
            .storage
            .and_then(|s| eframe::get_value(s, KEY))
            .unwrap_or_default();
        let running: Vec<bool> = cc
            .storage
            .and_then(|s| eframe::get_value(s, RUNNING))
            .unwrap_or_default();
        app.devices = saved
            .into_iter()
            .zip(running.into_iter().chain(std::iter::repeat(false)))
            .map(|(args, run)| Device {
                // ponytail: a failed restart (e.g. revoked permission) just leaves it stopped.
                live: run.then(|| Loopback::create(&args).ok()).flatten(),
                rec: None,
                args,
            })
            .collect();
        // Register once only, so turning it off in System Settings sticks.
        app.login_registered = cc
            .storage
            .and_then(|s| eframe::get_value::<bool>(s, LOGIN))
            .filter(|&done| done)
            .unwrap_or_else(register_login_item);
        app.show_continuity = cc
            .storage
            .and_then(|s| eframe::get_value(s, SHOW_CONTINUITY))
            .unwrap_or(false);
        app.dark = cc
            .storage
            .and_then(|s| eframe::get_value(s, DARK))
            .unwrap_or(cc.egui_ctx.system_theme() != Some(egui::Theme::Light));
        app.record_dir = cc
            .storage
            .and_then(|s| eframe::get_value(s, RECORD_DIR))
            .unwrap_or_else(|| "~/Music/recordr".into());
        app.show_in = cc
            .storage
            .and_then(|s| eframe::get_value(s, SHOW_IN))
            .unwrap_or_default();
        app.apply_show_in(&cc.egui_ctx);
        app.hotkeys = Hotkeys::new(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);
        cc.egui_ctx.set_theme(theme(app.dark));
        if app.devices.is_empty() {
            app.add_device();
        }
        app.refresh(&cc.egui_ctx);
        Ok(Box::new(app))
    });
    eframe::run_native("recordr", opts, creator).map_err(|e| e.to_string())
}

impl App {
    fn refresh(&mut self, ctx: &egui::Context) {
        let mut apps: Vec<AppEntry> = vec![];
        for p in processes()
            .into_iter()
            .filter(|p| !p.bundle.is_empty() && !p.bundle.starts_with("dev.recordr"))
        {
            if let Some(a) = apps.iter_mut().find(|a| a.bundle == p.bundle) {
                a.playing |= p.playing;
                continue;
            }
            let (name, icon) = match running_app(p.pid, &p.bundle) {
                Some((app, helper)) => {
                    let name = app.localizedName().map(|n| n.to_string());
                    let name = name.map(|n| if helper { format!("{n} (helper)") } else { n });
                    let icon = self
                        .icons
                        .entry(p.bundle.clone())
                        .or_insert_with(|| {
                            app.icon()
                                .and_then(|i| icon_image(&i, 64))
                                .map(|img| ctx.load_texture(&p.bundle, img, Default::default()))
                        })
                        .clone();
                    (name, icon)
                }
                None => (None, None),
            };
            let name = name.unwrap_or_else(|| p.bundle.clone());
            let name_is_helper = name.ends_with(" (helper)");
            apps.push(AppEntry {
                name,
                meter: icon.is_some().then(|| format!("app:{}", p.bundle)),
                note: name_is_helper.then_some("helper"),
                bundle: p.bundle,
                playing: p.playing,
                icon,
                symbol: false,
            });
        }
        // Stable order (it refreshes every 2 s): third-party before Apple, named before raw ids.
        apps.sort_by_key(|a| {
            (
                a.bundle.starts_with("com.apple."),
                a.name == a.bundle,
                a.name.to_lowercase(),
            )
        });
        self.apps = apps;
        self.inputs = input_devices()
            .into_iter()
            .filter(|(uid, _, t)| {
                !uid.starts_with("recordr.") && (self.show_continuity || !is_continuity(*t))
            })
            .map(|(uid, name, transport)| AppEntry {
                icon: self
                    .icons
                    .entry(uid.clone())
                    .or_insert_with(|| {
                        symbol_image(transport_symbol(transport), 64)
                            .map(|img| ctx.load_texture(&uid, img, Default::default()))
                    })
                    .clone(),
                // Opening these connects/reconfigures the remote end, so never meter them.
                meter: (!is_remote(transport)).then(|| format!("in:{uid}")),
                note: is_remote(transport).then_some("no meter"),
                bundle: uid,
                name,
                playing: false,
                symbol: true,
            })
            .collect();
        // Selected apps that aren't running: look up their name and icon on disk, once.
        for b in self.devices.iter().flat_map(|d| &d.args.apps) {
            if !self.apps.iter().any(|a| a.bundle == *b) && !self.installed.contains_key(b) {
                let found = installed_app(b)
                    .map(|(name, img)| (name, ctx.load_texture(b, img, Default::default())));
                self.installed.insert(b.clone(), found);
            }
        }
        self.perms = PERMS.map(|(_, service, _)| tcc_status(service));
        self.last_refresh = Some(Instant::now());
        // Drop meters for things that vanished, and failed ones so they get retried.
        let wanted: Vec<&String> = self
            .apps
            .iter()
            .chain(&self.inputs)
            .filter_map(|a| a.meter.as_ref())
            .collect();
        self.meters
            .retain(|k, m| m.is_some() && wanted.contains(&k));
    }

    /// Meters run while the window is visible, focused or not. They keep taps and mics open
    /// (macOS shows its recording indicators), so stop them when minimized or fully hidden.
    fn update_meters(&mut self, ctx: &egui::Context) {
        let hidden = ctx.input(|i| {
            let v = i.viewport();
            v.minimized.unwrap_or(false) || v.occluded.unwrap_or(false)
        });
        if hidden {
            self.meters.clear();
            return;
        }
        if self
            .last_refresh
            .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.refresh(ctx); // pick up apps that started or stopped playing
        }
        for a in self.apps.iter().chain(&self.inputs) {
            let Some(key) = &a.meter else { continue };
            self.meters.entry(key.clone()).or_insert_with(|| {
                let m = if a.symbol {
                    Meter::device(&a.bundle)
                } else {
                    Meter::app(&a.bundle)
                };
                m.map_err(|e| eprintln!("meter {key}: {e}")).ok()
            });
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn apply_show_in(&mut self, ctx: &egui::Context) {
        tray::set_dock(self.show_in != ShowIn::MenuBar);
        if self.show_in == ShowIn::Dock {
            self.tray = None;
        } else if self.tray.is_none() {
            self.tray = Some(Tray::new(ctx));
        }
    }

    /// Quit from the menu bar, and closing the window: with a menu bar icon it only hides the app.
    fn handle_window(&mut self, ctx: &egui::Context) {
        use egui::ViewportCommand as Cmd;
        if tray::take_quit() {
            self.quitting = true;
            ctx.send_viewport_cmd(Cmd::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.tray.is_some() && !self.quitting {
            ctx.send_viewport_cmd(Cmd::CancelClose);
            tray::hide();
        }
        let recording = self.devices.iter().any(|d| d.rec.is_some());
        if let Some(t) = &mut self.tray {
            t.set_recording(recording);
        }
    }

    fn add_device(&mut self) {
        let name = format!("Loopback Audio {}", self.devices.len() + 1);
        self.devices.push(Device {
            args: RunArgs {
                name,
                ..Default::default()
            },
            live: None,
            rec: None,
        });
        self.selected = self.devices.len() - 1;
    }
}

impl eframe::App for App {
    /// Runs every frame, and also while the window is hidden (unlike `ui`).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(h) = &mut self.hotkeys {
            if let Some(e) = h.sync(&self.devices) {
                self.error = Some(e);
            }
            for i in h.take_pressed() {
                if let Some(Err(e)) = self
                    .devices
                    .get_mut(i)
                    .map(|d| d.toggle_record(&self.record_dir))
                {
                    self.error = Some(e);
                }
            }
        }
        self.handle_window(ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let args: Vec<&RunArgs> = self.devices.iter().map(|d| &d.args).collect();
        eframe::set_value(storage, KEY, &args);
        let running: Vec<bool> = self.devices.iter().map(|d| d.live.is_some()).collect();
        eframe::set_value(storage, RUNNING, &running);
        eframe::set_value(storage, LOGIN, &self.login_registered);
        eframe::set_value(storage, SHOW_CONTINUITY, &self.show_continuity);
        eframe::set_value(storage, DARK, &self.dark);
        eframe::set_value(storage, RECORD_DIR, &self.record_dir);
        eframe::set_value(storage, SHOW_IN, &self.show_in);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_meters(&ui.ctx().clone());
        egui::Panel::left("devices")
            .resizable(false)
            .exact_size(180.0)
            .show(ui, |ui| {
                ui.heading("Devices");
                for (i, d) in self.devices.iter().enumerate() {
                    let dot = if d.live.is_some() { "● " } else { "○ " };
                    let current = i == self.selected && !self.settings_open;
                    if ui
                        .selectable_label(current, format!("{dot}{}", d.args.name))
                        .clicked()
                    {
                        self.selected = i;
                        self.settings_open = false;
                    }
                }
                ui.separator();
                if ui.button("+ New device").clicked() {
                    self.add_device();
                    self.settings_open = false;
                }
                let missing = self.perms.contains(&Perm::Denied);
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(4.0);
                    let label = if missing {
                        "⚠ Settings"
                    } else {
                        "⚙ Settings"
                    };
                    if ui.selectable_label(self.settings_open, label).clicked() {
                        self.settings_open = true;
                    }
                });
            });

        if self.settings_open {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Settings");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Appearance");
                    let a = ui.selectable_value(&mut self.dark, false, "☀ Bright");
                    let b = ui.selectable_value(&mut self.dark, true, "☾ Dark");
                    if a.changed() || b.changed() {
                        ui.ctx().set_theme(theme(self.dark));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Show in");
                    let before = self.show_in;
                    ui.selectable_value(&mut self.show_in, ShowIn::Both, "Dock and menu bar");
                    ui.selectable_value(&mut self.show_in, ShowIn::Dock, "Dock only");
                    ui.selectable_value(&mut self.show_in, ShowIn::MenuBar, "Menu bar only");
                    if self.show_in != before {
                        self.apply_show_in(&ui.ctx().clone());
                    }
                });
                ui.add_space(16.0);
                ui.strong("Recordings");
                ui.horizontal(|ui| {
                    ui.label("Folder");
                    ui.text_edit_singleline(&mut self.record_dir);
                    if ui.link("Show in Finder").clicked() {
                        let dir = record::file_path(&self.record_dir, "x");
                        let dir = dir.parent().unwrap_or(&dir);
                        let _ = std::fs::create_dir_all(dir);
                        let _ = std::process::Command::new("open").arg(dir).spawn();
                    }
                });
                ui.add_space(16.0);
                ui.strong("Command line");
                ui.horizontal(|ui| {
                    if cli_installed() {
                        ui.colored_label(TANGERINE, format!("✔ {CLI_LINK}"));
                    } else if ui.button("Install command line tool").clicked() {
                        install_cli();
                    }
                })
                .response
                .on_hover_text(format!(
                    "Links {CLI_LINK} to this app, so `recordr` works in a terminal."
                ));
                ui.add_space(16.0);
                ui.strong("Permissions");
                for ((name, _, pane), status) in PERMS.iter().zip(self.perms) {
                    ui.horizontal(|ui| {
                        ui.label(*name);
                        match status {
                            Perm::Granted => ui.colored_label(TANGERINE, "✔ Granted"),
                            Perm::Unknown => {
                                ui.weak("Not asked yet: macOS asks when it's first needed")
                            }
                            Perm::Denied => {
                                ui.colored_label(CINNABAR, "✖ Missing");
                                if ui.link("Open System Settings").clicked() {
                                    open_settings(pane);
                                }
                                ui.weak("then relaunch recordr")
                            }
                        };
                    });
                }
                ui.add_space(16.0);
                let changed = ui
                    .checkbox(&mut self.show_continuity, "Show Continuity sources")
                    .on_hover_text("List iPhone microphones (Continuity) as inputs.")
                    .changed();
                if changed {
                    self.refresh(&ui.ctx().clone());
                }
            });
            return;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let (mut delete, mut refresh) = (false, false);
            let many = self.devices.len() > 1;
            let sel = self.selected;
            let (apps, inputs, error, meters, installed, record_dir, capturing) = (
                &self.apps,
                &self.inputs,
                &mut self.error,
                &self.meters,
                &self.installed,
                &self.record_dir,
                &mut self.capturing,
            );
            let Some(d) = self.devices.get_mut(self.selected) else {
                return;
            };
            let live = d.live.is_some();

            if self.perms.contains(&Perm::Denied) {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        CINNABAR,
                        "⚠ recordr is missing permissions, so devices may be silent.",
                    );
                    if ui.link("Fix in Settings").clicked() {
                        self.settings_open = true;
                    }
                });
                ui.separator();
            }

            ui.horizontal(|ui| {
                ui.add_enabled(
                    !live,
                    egui::TextEdit::singleline(&mut d.args.name).desired_width(220.0),
                );
                let label = if live { "■ Stop" } else { "▶ Start" };
                let fill = if live { CINNABAR } else { CORAL };
                let button =
                    egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE))
                        .fill(fill);
                if ui.add(button).clicked() {
                    *error = None;
                    if live {
                        d.rec = None;
                        d.live = None; // Drop removes the device
                    } else if let Err(e) = d.start() {
                        *error = Some(e);
                    }
                }
                let recording = d.rec.is_some();
                let label = if recording {
                    "■ Stop recording"
                } else {
                    "● Record"
                };
                let button =
                    egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE))
                        .fill(if recording { CINNABAR } else { TANGERINE });
                if ui
                    .add(button)
                    .on_hover_text("Save the device's audio as a WAV in the Settings folder.")
                    .clicked()
                {
                    *error = d.toggle_record(record_dir).err();
                }
                // Closed apps are skipped by Loopback::create; say so.
                let closed: Vec<String> = d
                    .args
                    .apps
                    .iter()
                    .filter(|b| !d.args.system && !apps.iter().any(|a| a.bundle == **b))
                    .map(|b| match installed.get(b) {
                        Some(Some((name, _))) => name.clone(),
                        _ => b.clone(),
                    })
                    .collect();
                if !closed.is_empty() {
                    ui.colored_label(TANGERINE, format!("⚠ Not running: {}", closed.join(", ")))
                        .on_hover_text(
                            "These apps won't be captured until they're open and play sound.",
                        );
                }
                if !live && many && ui.button("Delete").clicked() {
                    delete = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Record shortcut");
                if *capturing == Some(sel) {
                    ui.colored_label(CORAL, "Press a shortcut… (Esc cancels)");
                    let key = ui.input(|i| {
                        i.events.iter().find_map(|e| match e {
                            egui::Event::Key {
                                key,
                                pressed: true,
                                modifiers,
                                ..
                            } => Some((*key, *modifiers)),
                            _ => None,
                        })
                    });
                    match key {
                        Some((egui::Key::Escape, _)) => *capturing = None,
                        // Require a modifier, so a plain letter isn't stolen from every app.
                        Some((key, m)) if m.mac_cmd || m.ctrl || m.alt => {
                            d.args.shortcut = shortcut_name(key, m);
                            *capturing = None;
                        }
                        _ => {}
                    }
                } else {
                    let text = if d.args.shortcut.is_empty() {
                        "Set…"
                    } else {
                        &d.args.shortcut
                    };
                    if ui
                        .button(text)
                        .on_hover_text("Toggles recording from any app.")
                        .clicked()
                    {
                        *capturing = Some(sel);
                    }
                    if !d.args.shortcut.is_empty() && ui.small_button("✕").clicked() {
                        d.args.shortcut.clear();
                    }
                }
            });
            if let Some(e) = error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            if live {
                ui.label("Live — select it as an input in any app. Stop to edit.");
            }
            if let Some(r) = &d.rec {
                ui.horizontal(|ui| {
                    ui.colored_label(CINNABAR, format!("● Recording to {}", r.path.display()));
                    if ui.link("Show in Finder").clicked() {
                        let _ = std::process::Command::new("open")
                            .arg("-R")
                            .arg(&r.path)
                            .spawn();
                    }
                });
            }
            ui.separator();

            // Sources can't change on a live tap; stop first.
            ui.add_enabled_ui(!live, |ui| {
                ui.checkbox(&mut d.args.mute, "Mute sources on speakers");
                ui.checkbox(&mut d.args.system, "All system audio");
                ui.horizontal(|ui| {
                    ui.strong("Apps");
                    if ui.small_button("↻ Refresh").clicked() {
                        refresh = true;
                    }
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_enabled_ui(!d.args.system, |ui| {
                        let (with_icon, other): (Vec<_>, Vec<_>) =
                            apps.iter().partition(|a| a.icon.is_some());
                        app_grid(ui, "apps", &mut d.args.apps, &with_icon, meters);
                        egui::CollapsingHeader::new(format!("Other processes ({})", other.len()))
                            .show(ui, |ui| {
                                app_grid(ui, "other", &mut d.args.apps, &other, meters);
                            });
                        // Selected apps that aren't running: shown so they can be deselected.
                        let closed: Vec<AppEntry> = d
                            .args
                            .apps
                            .iter()
                            .filter(|b| !apps.iter().any(|a| a.bundle == **b))
                            .map(|b| {
                                let found = installed.get(b).cloned().flatten();
                                AppEntry {
                                    bundle: b.clone(),
                                    name: found.as_ref().map_or_else(
                                        || b.rsplit('.').next().unwrap_or(b).to_string(),
                                        |(n, _)| n.clone(),
                                    ),
                                    playing: false,
                                    icon: found.map(|(_, t)| t),
                                    symbol: false,
                                    meter: None,
                                    note: Some("not running"),
                                }
                            })
                            .collect();
                        if !closed.is_empty() {
                            ui.add_space(8.0);
                            ui.strong("Closed apps");
                            let closed: Vec<&AppEntry> = closed.iter().collect();
                            app_grid(ui, "closed", &mut d.args.apps, &closed, meters);
                        }
                    });
                    ui.add_space(8.0);
                    ui.strong("Input devices (added as extra channels)");
                    let inputs: Vec<&AppEntry> = inputs.iter().collect();
                    app_grid(ui, "inputs", &mut d.args.devices, &inputs, meters);
                });
            });
            if delete {
                self.devices.remove(self.selected);
                self.selected = self.selected.saturating_sub(1);
            }
            if refresh {
                self.refresh(&ui.ctx().clone());
            }
        });
    }
}

/// Open at login via SMAppService (macOS 13+). Only works from a .app bundle; no-op for `cargo run`.
/// Returns whether it registered; false means try again next launch.
fn register_login_item() -> bool {
    if !std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/")) {
        return false;
    }
    let svc = unsafe { objc2_service_management::SMAppService::mainAppService() };
    unsafe { svc.registerAndReturnError() }
        .map_err(|e| eprintln!("login item: {e:?}"))
        .is_ok()
}

const TILE: egui::Vec2 = egui::vec2(96.0, 96.0);

/// Fixed-size tiles, as many columns as fit the width.
fn app_grid(
    ui: &mut egui::Ui,
    id: &str,
    list: &mut Vec<String>,
    apps: &[&AppEntry],
    meters: &HashMap<String, Option<Meter>>,
) {
    let gap = 6.0;
    let cols = (((ui.available_width() + gap) / (TILE.x + gap)) as usize).max(1);
    egui::Grid::new(id).spacing([gap, gap]).show(ui, |ui| {
        for (i, a) in apps.iter().enumerate() {
            let levels = a
                .meter
                .as_ref()
                .and_then(|k| meters.get(k)?.as_ref())
                .map(Meter::levels);
            app_tile(ui, list, a, levels);
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
}

/// One toggle tile: big icon on top, name below (one line, ellipsized), highlighted when on.
fn app_tile(ui: &mut egui::Ui, list: &mut Vec<String>, a: &AppEntry, levels: Option<[f32; 2]>) {
    let on = list.iter().any(|x| *x == a.bundle);
    let (rect, resp) = ui.allocate_exact_size(TILE, egui::Sense::click());
    let resp = resp.on_hover_text(format!("{}\n{}", a.name, a.bundle));
    if resp.clicked() {
        if on {
            list.retain(|x| *x != a.bundle);
        } else {
            list.push(a.bundle.clone());
        }
    }
    let visuals = ui.visuals();
    let (fill, text_color) = if on {
        (visuals.selection.bg_fill, visuals.selection.stroke.color)
    } else if resp.hovered() {
        (visuals.widgets.hovered.weak_bg_fill, visuals.text_color())
    } else {
        (visuals.faint_bg_color, visuals.text_color())
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, fill);

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 26.0),
        egui::vec2(40.0, 40.0),
    );
    match &a.icon {
        Some(t) if a.symbol => egui::Image::new(t)
            .tint(text_color)
            .paint_at(ui, icon_rect.shrink(4.0)),
        Some(t) => egui::Image::new(t).paint_at(ui, icon_rect),
        None => {
            // No icon: a monogram so the grid stays uniform.
            let letter = a.bundle.rsplit('.').next().unwrap_or("?");
            let letter: String = letter.chars().take(1).collect::<String>().to_uppercase();
            painter.circle_filled(icon_rect.center(), 18.0, visuals.widgets.inactive.bg_fill);
            painter.text(
                icon_rect.center(),
                egui::Align2::CENTER_CENTER,
                letter,
                egui::FontId::proportional(18.0),
                text_color,
            );
        }
    }
    if a.playing {
        painter.text(
            rect.right_top() + egui::vec2(-8.0, 6.0),
            egui::Align2::RIGHT_TOP,
            "♪",
            egui::FontId::proportional(13.0),
            text_color,
        );
    }

    let label = if a.icon.is_some() || a.symbol {
        a.name.trim_end_matches(" (helper)")
    } else {
        a.bundle.rsplit('.').next().unwrap_or(&a.bundle)
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.to_string(),
        egui::FontId::proportional(12.0),
        text_color,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(TILE.x - 8.0);
    job.halign = egui::Align::Center;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    painter.galley(
        egui::pos2(rect.center().x, rect.top() + 50.0),
        galley,
        text_color,
    );
    if let Some(note) = a.note {
        painter.text(
            egui::pos2(rect.center().x, rect.top() + 66.0),
            egui::Align2::CENTER_TOP,
            note,
            egui::FontId::proportional(10.0),
            text_color.gamma_multiply(0.6),
        );
    }
    if let Some(levels) = levels {
        meter_bars(&painter, rect, levels, text_color);
    }
}

/// Two thin L/R bars along the tile's bottom edge: tangerine, then coral, cinnabar near 0 dB.
fn meter_bars(painter: &egui::Painter, tile: egui::Rect, levels: [f32; 2], track: egui::Color32) {
    for (i, level) in levels.into_iter().enumerate() {
        let y = tile.bottom() - 14.0 + i as f32 * 6.0;
        let bar = egui::Rect::from_min_size(
            egui::pos2(tile.left() + 10.0, y),
            egui::vec2(TILE.x - 20.0, 4.0),
        );
        painter.rect_filled(bar, 2.0, track.gamma_multiply(0.15));
        let color = match level {
            l if l > 0.95 => CINNABAR,
            l if l > 0.8 => CORAL,
            _ => TANGERINE,
        };
        let mut lit = bar;
        lit.set_width(bar.width() * level);
        painter.rect_filled(lit, 2.0, color);
    }
}

/// The app owning this process: itself, or for helpers/extensions ("com.google.Chrome.helper")
/// the running parent app found by trimming the bundle id. Bool = it's a helper.
fn running_app(
    pid: i32,
    bundle: &str,
) -> Option<(
    objc2::rc::Retained<objc2_app_kit::NSRunningApplication>,
    bool,
)> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        && app.localizedName().is_some()
    {
        return Some((app, false));
    }
    let mut id = bundle;
    while let Some((parent, _)) = id.rsplit_once('.') {
        // ponytail: stop at "com.apple" etc; two components can't be an app id.
        if !parent.contains('.') {
            return None;
        }
        let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(parent),
        );
        if let Some(app) = apps.iter().find(|a| a.localizedName().is_some()) {
            return Some((app, true));
        }
        id = parent;
    }
    None
}

/// Name and icon of an installed app that isn't running. Helper ids fall back to the parent app.
fn installed_app(bundle: &str) -> Option<(String, egui::ColorImage)> {
    use objc2_foundation::NSString;
    let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let mut id = bundle;
    let url = loop {
        if let Some(u) = ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(id)) {
            break u;
        }
        id = id.rsplit_once('.').filter(|(p, _)| p.contains('.'))?.0;
    };
    let path = url.path()?;
    let name = url.lastPathComponent()?.to_string();
    let name = name.trim_end_matches(".app").to_string();
    Some((name, icon_image(&ws.iconForFile(&path), 64)?))
}

/// Rasterize the app's icon to px×px premultiplied RGBA.
/// iPhone microphones over Continuity (wired/wireless).
fn is_continuity(transport: u32) -> bool {
    matches!(&transport.to_be_bytes(), b"ccwd" | b"ccwl")
}

/// Inputs that live on another device: opening them for a meter would wake the iPhone
/// (Continuity) or drop AirPods into low-quality call mode (Bluetooth).
fn is_remote(transport: u32) -> bool {
    is_continuity(transport) || matches!(&transport.to_be_bytes(), b"blue" | b"blea")
}

/// SF Symbol for a Core Audio transport type.
fn transport_symbol(transport: u32) -> &'static str {
    match &transport.to_be_bytes() {
        b"bltn" => "mic.fill",
        b"blue" | b"blea" => "headphones",
        b"usb " => "cable.connector",
        b"ccwd" | b"ccwl" => "iphone",
        b"virt" => "waveform",
        b"grup" | b"agrp" => "square.stack.3d.up.fill",
        _ => "mic",
    }
}

/// SF Symbol rendered white-on-transparent (tinted at paint time).
fn symbol_image(name: &str, px: usize) -> Option<egui::ColorImage> {
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSString;
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    let mut out = icon_image(&img, px)?;
    for p in &mut out.pixels {
        *p = egui::Color32::from_white_alpha(p.a());
    }
    Some(out)
}

/// Rasterize an NSImage to px×px premultiplied RGBA, aspect-fit and centered.
fn icon_image(image: &objc2_app_kit::NSImage, px: usize) -> Option<egui::ColorImage> {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            px as isize,
            px as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (px * 4) as isize,
            32,
        )
    }?;
    let gc = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&gc));
    let side = px as f64;
    let size = image.size();
    let k = side / size.width.max(size.height).max(1.0);
    let (w, h) = (size.width * k, size.height * k);
    image.drawInRect(NSRect::new(
        NSPoint::new((side - w) / 2.0, (side - h) / 2.0),
        NSSize::new(w, h),
    ));
    gc.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, px * px * 4) };
    Some(egui::ColorImage::from_rgba_premultiplied([px, px], bytes))
}

fn theme(dark: bool) -> egui::Theme {
    if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Perm {
    #[default]
    Unknown,
    Granted,
    Denied,
}

/// (label, TCC service, System Settings pane) for each permission recordr needs.
const PERMS: [(&str, &str, &str); 2] = [
    (
        "Audio Capture",
        "kTCCServiceAudioCapture",
        "Privacy_ScreenCapture",
    ),
    ("Microphone", "kTCCServiceMicrophone", "Privacy_Microphone"),
];

/// Asks TCC whether we hold `service`, without prompting.
// ponytail: TCCAccessPreflight is private API (there's no public one for Audio Capture);
// if Apple removes it every permission just reads "Not asked yet".
fn tcc_status(service: &str) -> Perm {
    use std::ffi::{c_char, c_int, c_void};
    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    type Preflight = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;
    let f = unsafe {
        let h = dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/TCC".as_ptr(),
            1,
        );
        if h.is_null() {
            return Perm::Unknown;
        }
        dlsym(h, c"TCCAccessPreflight".as_ptr())
    };
    if f.is_null() {
        return Perm::Unknown;
    }
    let f: Preflight = unsafe { std::mem::transmute(f) };
    let name = objc2_foundation::NSString::from_str(service); // toll-free bridged to CFString
    match unsafe { f(objc2::rc::Retained::as_ptr(&name).cast(), std::ptr::null()) } {
        0 => Perm::Granted,
        1 => Perm::Denied,
        _ => Perm::Unknown,
    }
}

/// "Ctrl+Option+Shift+Cmd+R": readable, and parseable by `global_hotkey`.
fn shortcut_name(key: egui::Key, m: egui::Modifiers) -> String {
    let mods = [
        (m.ctrl, "Ctrl"),
        (m.alt, "Option"),
        (m.shift, "Shift"),
        (m.mac_cmd, "Cmd"),
    ];
    let mut parts: Vec<&str> = mods.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect();
    parts.push(key.name());
    parts.join("+")
}

const CLI_LINK: &str = "/usr/local/bin/recordr";

fn cli_installed() -> bool {
    let exe = std::env::current_exe().ok();
    std::fs::read_link(CLI_LINK).ok() == exe
}

/// Symlink the CLI via an admin prompt (/usr/local/bin is root-owned). Doesn't wait: the
/// Settings row re-checks the link every frame.
fn install_cli() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "do shell script \"mkdir -p /usr/local/bin && ln -sf \" & quoted form of \"{exe}\" & \" {CLI_LINK}\" with administrator privileges"
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn();
}

fn open_settings(pane: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    let _ = std::process::Command::new("open").arg(url).spawn();
}

#[test]
fn shortcut_names_parse() {
    let m = egui::Modifiers {
        mac_cmd: true,
        shift: true,
        ..Default::default()
    };
    for key in [
        egui::Key::R,
        egui::Key::Num1,
        egui::Key::F5,
        egui::Key::Space,
    ] {
        let name = shortcut_name(key, m);
        assert!(name.parse::<HotKey>().is_ok(), "{name}");
    }
    assert_eq!(shortcut_name(egui::Key::R, m), "Shift+Cmd+R");
}
