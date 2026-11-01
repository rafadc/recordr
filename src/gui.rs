//! Window UI: a list of virtual devices on the left, the selected one's sources on the right.
// Device configs persist via eframe storage (~/Library/Application Support/recordr); they start stopped.

use crate::{Loopback, RunArgs, input_devices, processes};
use eframe::egui;
use std::collections::BTreeSet;

struct Device {
    args: RunArgs,
    live: Option<Loopback>,
}

#[derive(Default)]
struct App {
    devices: Vec<Device>,
    selected: usize,
    apps: Vec<(String, bool)>,     // (bundle id, playing now)
    inputs: Vec<(String, String)>, // (uid, name)
    error: Option<String>,
}

const KEY: &str = "devices";

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
        app.devices = saved
            .into_iter()
            .map(|args| Device { args, live: None })
            .collect();
        if app.devices.is_empty() {
            app.add_device();
        }
        app.refresh();
        Ok(Box::new(app))
    });
    eframe::run_native("recordr", opts, creator).map_err(|e| e.to_string())
}

impl App {
    fn refresh(&mut self) {
        let mut seen = BTreeSet::new();
        let mut apps: Vec<(String, bool)> = vec![];
        for p in processes()
            .into_iter()
            .filter(|p| !p.bundle.is_empty() && !p.bundle.starts_with("dev.recordr"))
        {
            if seen.insert(p.bundle.clone()) {
                apps.push((p.bundle, p.playing));
            } else if p.playing {
                apps.iter_mut().find(|a| a.0 == p.bundle).unwrap().1 = true;
            }
        }
        // Playing first, then third-party apps before Apple system daemons.
        apps.sort_by_key(|(b, playing)| (!playing, b.starts_with("com.apple."), b.clone()));
        self.apps = apps;
        self.inputs = input_devices()
            .into_iter()
            .filter(|(uid, _)| !uid.starts_with("recordr."))
            .collect();
    }

    fn add_device(&mut self) {
        let name = format!("Loopback Audio {}", self.devices.len() + 1);
        self.devices.push(Device {
            args: RunArgs {
                name,
                ..Default::default()
            },
            live: None,
        });
        self.selected = self.devices.len() - 1;
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let args: Vec<&RunArgs> = self.devices.iter().map(|d| &d.args).collect();
        eframe::set_value(storage, KEY, &args);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("devices")
            .resizable(false)
            .exact_size(180.0)
            .show(ui, |ui| {
                ui.heading("Devices");
                for (i, d) in self.devices.iter().enumerate() {
                    let dot = if d.live.is_some() { "● " } else { "○ " };
                    if ui
                        .selectable_label(i == self.selected, format!("{dot}{}", d.args.name))
                        .clicked()
                    {
                        self.selected = i;
                    }
                }
                ui.separator();
                if ui.button("+ New device").clicked() {
                    self.add_device();
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            let (mut delete, mut refresh) = (false, false);
            let many = self.devices.len() > 1;
            let (apps, inputs, error) = (&self.apps, &self.inputs, &mut self.error);
            let Some(d) = self.devices.get_mut(self.selected) else {
                return;
            };
            let live = d.live.is_some();

            ui.horizontal(|ui| {
                ui.add_enabled(
                    !live,
                    egui::TextEdit::singleline(&mut d.args.name).desired_width(220.0),
                );
                let label = if live { "■ Stop" } else { "▶ Start" };
                if ui.button(label).clicked() {
                    *error = None;
                    if live {
                        d.live = None; // Drop removes the device
                    } else if d.args.apps.is_empty() && !d.args.system && d.args.devices.is_empty()
                    {
                        *error = Some("Pick at least one source.".into());
                    } else {
                        match Loopback::create(&d.args) {
                            Ok(lb) => d.live = Some(lb),
                            Err(e) => *error = Some(e),
                        }
                    }
                }
                if !live && many && ui.button("Delete").clicked() {
                    delete = true;
                }
            });
            if let Some(e) = error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            if live {
                ui.label("Live — select it as an input in any app. Stop to edit.");
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
                        for (bundle, playing) in apps {
                            let label = format!("{}{bundle}", if *playing { "♪ " } else { "" });
                            toggle(ui, &mut d.args.apps, bundle, &label);
                        }
                    });
                    ui.add_space(8.0);
                    ui.strong("Input devices (added as extra channels)");
                    for (uid, name) in inputs {
                        toggle(ui, &mut d.args.devices, uid, name);
                    }
                });
            });
            if delete {
                self.devices.remove(self.selected);
                self.selected = self.selected.saturating_sub(1);
            }
            if refresh {
                self.refresh();
            }
        });
    }
}

/// Checkbox bound to membership of `item` in `list`.
fn toggle(ui: &mut egui::Ui, list: &mut Vec<String>, item: &str, label: &str) {
    let mut on = list.iter().any(|x| x == item);
    if ui.checkbox(&mut on, label).changed() {
        if on {
            list.push(item.to_string());
        } else {
            list.retain(|x| x != item);
        }
    }
}
