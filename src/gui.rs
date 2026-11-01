//! Window UI: a list of virtual devices on the left, the selected one's sources on the right.
// Device configs persist via eframe storage (~/Library/Application Support/recordr);
// devices that were live at quit restart on launch, and the app registers itself as a login item.

use crate::{Loopback, RunArgs, input_devices, meter::Meter, processes};
use eframe::egui;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Device {
    args: RunArgs,
    live: Option<Loopback>,
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
    show_continuity: bool, // setting: list iPhone (Continuity) mics as inputs
    dark: bool,            // setting: Dark vs Bright theme
    // Live level meters keyed by AppEntry::meter; None = failed, retried on the next refresh.
    meters: HashMap<String, Option<Meter>>,
    icons: HashMap<String, Option<egui::TextureHandle>>,
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
        let running: Vec<bool> = self.devices.iter().map(|d| d.live.is_some()).collect();
        eframe::set_value(storage, RUNNING, &running);
        eframe::set_value(storage, LOGIN, &self.login_registered);
        eframe::set_value(storage, SHOW_CONTINUITY, &self.show_continuity);
        eframe::set_value(storage, DARK, &self.dark);
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
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(4.0);
                    if ui
                        .selectable_label(self.settings_open, "⚙ Settings")
                        .clicked()
                    {
                        self.settings_open = true;
                    }
                });
            });

        if self.settings_open {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Settings");
                ui.add_space(8.0);
                let changed = ui
                    .checkbox(&mut self.show_continuity, "Show Continuity sources")
                    .on_hover_text("List iPhone microphones (Continuity) as inputs.")
                    .changed();
                if changed {
                    self.refresh(&ui.ctx().clone());
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Appearance");
                    let a = ui.selectable_value(&mut self.dark, false, "☀ Bright");
                    let b = ui.selectable_value(&mut self.dark, true, "☾ Dark");
                    if a.changed() || b.changed() {
                        ui.ctx().set_theme(theme(self.dark));
                    }
                });
            });
            return;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let (mut delete, mut refresh) = (false, false);
            let many = self.devices.len() > 1;
            let (apps, inputs, error, meters) =
                (&self.apps, &self.inputs, &mut self.error, &self.meters);
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
                let fill = if live { CINNABAR } else { CORAL };
                let button =
                    egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE))
                        .fill(fill);
                if ui.add(button).clicked() {
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
                        let (with_icon, other): (Vec<_>, Vec<_>) =
                            apps.iter().partition(|a| a.icon.is_some());
                        app_grid(ui, "apps", &mut d.args.apps, &with_icon, meters);
                        egui::CollapsingHeader::new(format!("Other processes ({})", other.len()))
                            .show(ui, |ui| {
                                app_grid(ui, "other", &mut d.args.apps, &other, meters);
                            });
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
