//! Window UI: a list of virtual devices on the left, the selected one's sources on the right.
// Device configs persist via eframe storage (~/Library/Application Support/recordr);
// devices that were live at quit restart on launch, and the app registers itself as a login item.

use crate::{Loopback, RunArgs, input_devices, processes};
use eframe::egui;

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
}

struct AppEntry {
    bundle: String,
    name: String,
    playing: bool,
    icon: Option<egui::TextureHandle>,
    symbol: bool, // monochrome SF Symbol: tint with the text color
}

const KEY: &str = "devices";
const RUNNING: &str = "running";
const LOGIN: &str = "login_item_registered";

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
                    let icon = app
                        .icon()
                        .and_then(|i| icon_image(&i, 64))
                        .map(|img| ctx.load_texture(&p.bundle, img, Default::default()));
                    (name, icon)
                }
                None => (None, None),
            };
            apps.push(AppEntry {
                name: name.unwrap_or_else(|| p.bundle.clone()),
                bundle: p.bundle,
                playing: p.playing,
                icon,
                symbol: false,
            });
        }
        // Playing first, then third-party before Apple, named before raw bundle ids.
        apps.sort_by_key(|a| {
            (
                !a.playing,
                a.bundle.starts_with("com.apple."),
                a.name == a.bundle,
                a.name.to_lowercase(),
            )
        });
        self.apps = apps;
        self.inputs = input_devices()
            .into_iter()
            .filter(|(uid, _, _)| !uid.starts_with("recordr."))
            .map(|(uid, name, transport)| AppEntry {
                icon: symbol_image(transport_symbol(transport), 64)
                    .map(|img| ctx.load_texture(&uid, img, Default::default())),
                bundle: uid,
                name,
                playing: false,
                symbol: true,
            })
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
        let running: Vec<bool> = self.devices.iter().map(|d| d.live.is_some()).collect();
        eframe::set_value(storage, RUNNING, &running);
        eframe::set_value(storage, LOGIN, &self.login_registered);
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
                        let (with_icon, other): (Vec<_>, Vec<_>) =
                            apps.iter().partition(|a| a.icon.is_some());
                        app_grid(ui, "apps", &mut d.args.apps, &with_icon);
                        egui::CollapsingHeader::new(format!("Other processes ({})", other.len()))
                            .show(ui, |ui| {
                                app_grid(ui, "other", &mut d.args.apps, &other);
                            });
                    });
                    ui.add_space(8.0);
                    ui.strong("Input devices (added as extra channels)");
                    let inputs: Vec<&AppEntry> = inputs.iter().collect();
                    app_grid(ui, "inputs", &mut d.args.devices, &inputs);
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

const TILE: egui::Vec2 = egui::vec2(96.0, 80.0);

/// Fixed-size tiles, as many columns as fit the width.
fn app_grid(ui: &mut egui::Ui, id: &str, list: &mut Vec<String>, apps: &[&AppEntry]) {
    let gap = 6.0;
    let cols = (((ui.available_width() + gap) / (TILE.x + gap)) as usize).max(1);
    egui::Grid::new(id).spacing([gap, gap]).show(ui, |ui| {
        for (i, a) in apps.iter().enumerate() {
            app_tile(ui, list, a);
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
}

/// One toggle tile: big icon on top, name below (one line, ellipsized), highlighted when on.
fn app_tile(ui: &mut egui::Ui, list: &mut Vec<String>, a: &AppEntry) {
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
        egui::pos2(rect.center().x, rect.top() + 28.0),
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
        egui::pos2(rect.center().x, rect.bottom() - 24.0),
        galley,
        text_color,
    );
    if a.name.ends_with(" (helper)") {
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 9.0),
            egui::Align2::CENTER_BOTTOM,
            "helper",
            egui::FontId::proportional(10.0),
            text_color.gamma_multiply(0.6),
        );
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
