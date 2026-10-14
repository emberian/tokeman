//! The egui tray (feature `tray`): a menu-bar dot coloured by the best
//! account's weekly room, a window of gauges per account, launch buttons,
//! and the launch settings, edited through `Fleet::update`.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono::Utc;
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::account::Fleet;
use crate::anthropic::Client;
use crate::anthropic::probe::{PROBE_MODEL, PROBE_TIMEOUT, probe_all};
use crate::claude::settings as claude_settings;
use crate::daemon;
use crate::launch;
use crate::observe;
use crate::policy;
use crate::quota::{Level, ModelKey, Reading, Source, Window};
use crate::settings::LaunchSettings;
use crate::store::Store;
use crate::ui::text::{format_reset_compact, short_bucket_label, status_badge, truncate_chars};

const WINDOW_WIDTH: f32 = 420.0;
const GRAY: [u8; 3] = [158, 158, 158];

enum BgCmd {
    Refresh,
    /// The fleet changed on disk (settings); probe with the new one.
    Fleet(Box<Fleet>),
    Shutdown,
}

/// What the probe thread hands the UI.
#[derive(Default)]
struct Shared {
    readings: Arc<Vec<Reading>>,
    last_probe: Option<Instant>,
    /// The model Claude starts on, read with each probe.
    target_model: Option<ModelKey>,
}

/// A filled circle with an anti-aliased edge; `None` only if the size math
/// is wrong, which it is not.
fn make_icon(color: [u8; 3]) -> Option<tray_icon::Icon> {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let radius = center - 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * size + x) * 4) as usize;
            if dist <= radius {
                let alpha = if dist > radius - 1.0 {
                    ((radius - dist).clamp(0.0, 1.0) * 255.0) as u8
                } else {
                    255
                };
                rgba[idx..idx + 3].copy_from_slice(&color);
                rgba[idx + 3] = alpha;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).ok()
}

fn target_model() -> Option<ModelKey> {
    let settings = claude_settings::load().ok()?;
    claude_settings::target_model(&settings).map(ModelKey::new)
}

fn select_best<'a>(
    readings: &'a [Reading],
    fleet: &Fleet,
    target: Option<&ModelKey>,
) -> Option<&'a Reading> {
    policy::choose_landing(readings, &fleet.policy, target)
}

fn level_rgb(level: Level) -> [u8; 3] {
    match level {
        Level::Ok => [76, 175, 80],
        Level::Low => [255, 193, 7],
        Level::Critical => [244, 67, 54],
    }
}

fn level_color(level: Level) -> egui::Color32 {
    let [r, g, b] = level_rgb(level);
    egui::Color32::from_rgb(r, g, b)
}

fn weekly_remaining(reading: &Reading) -> f64 {
    reading.weekly.map_or(0.0, |w| w.remaining())
}

fn status_color(best: Option<&Reading>) -> [u8; 3] {
    best.map_or(GRAY, |reading| {
        level_rgb(Level::of(weekly_remaining(reading)))
    })
}

fn spawn_bg_thread(
    fleet: Fleet,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) -> mpsc::Sender<BgCmd> {
    let (tx, rx) = mpsc::channel::<BgCmd>();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("could not start the probe runtime: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            let mut fleet = fleet;
            let store = match Store::open() {
                Ok(store) => Some(store),
                Err(error) => {
                    eprintln!("history will not be recorded: {error:#}");
                    None
                }
            };
            loop {
                let _ = observe::scan_transcripts(&fleet.accounts);
                let now = Utc::now().timestamp();
                let mut readings = probe_all(
                    Client::shared(),
                    &fleet.accounts,
                    PROBE_MODEL,
                    PROBE_TIMEOUT,
                    now,
                )
                .await;
                observe::apply(&mut readings);
                if let Some(store) = &store {
                    for reading in &readings {
                        let _ = store.insert(reading);
                    }
                }
                {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.readings = Arc::new(readings);
                    state.last_probe = Some(Instant::now());
                    state.target_model = target_model();
                }
                ctx.request_repaint();

                let interval = Duration::from_secs(fleet.launch.probe_interval_secs.max(1));
                let mut deadline = Instant::now() + interval;
                loop {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(BgCmd::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Ok(BgCmd::Fleet(fresh)) => {
                            fleet = *fresh;
                            deadline = Instant::now()
                                + Duration::from_secs(fleet.launch.probe_interval_secs.max(1));
                        }
                        Ok(BgCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        });
    });
    tx
}

struct TrayApp {
    fleet: Fleet,
    shared: Arc<Mutex<Shared>>,
    bg_tx: mpsc::Sender<BgCmd>,
    tray_icon: Option<TrayIcon>,
    menu_show_id: Option<MenuId>,
    menu_refresh_id: Option<MenuId>,
    menu_quit_id: Option<MenuId>,
    settings_open: bool,
    settings_draft: LaunchSettings,
    first_frame: bool,
    last_icon_color: [u8; 3],
    quit_requested: bool,
    window_visible: bool,
}

impl TrayApp {
    fn new(cc: &eframe::CreationContext<'_>, fleet: Fleet) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = egui::Color32::from_rgb(30, 30, 30);
        visuals.panel_fill = egui::Color32::from_rgb(30, 30, 30);
        visuals.extreme_bg_color = egui::Color32::from_rgb(20, 20, 20);
        visuals.faint_bg_color = egui::Color32::from_white_alpha(8);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_white_alpha(10);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_white_alpha(15);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_white_alpha(25);
        visuals.widgets.active.bg_fill = egui::Color32::from_white_alpha(35);
        visuals.window_corner_radius = egui::CornerRadius::same(12);
        cc.egui_ctx.set_visuals(visuals);

        let shared = Arc::new(Mutex::new(Shared::default()));
        let bg_tx = spawn_bg_thread(fleet.clone(), shared.clone(), cc.egui_ctx.clone());
        let settings_draft = fleet.launch.clone();
        let mut app = Self {
            fleet,
            shared,
            bg_tx,
            tray_icon: None,
            menu_show_id: None,
            menu_refresh_id: None,
            menu_quit_id: None,
            settings_open: false,
            settings_draft,
            first_frame: true,
            last_icon_color: GRAY,
            quit_requested: false,
            window_visible: true,
        };
        app.create_tray_icon();
        app
    }

    fn create_tray_icon(&mut self) {
        let menu = Menu::new();
        let show = MenuItem::new("Show / Hide", true, None);
        let refresh = MenuItem::new("Refresh Now", true, None);
        let quit = MenuItem::new("Quit tokeman", true, None);
        let _ = menu.append(&show);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&refresh);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);
        self.menu_show_id = Some(show.id().clone());
        self.menu_refresh_id = Some(refresh.id().clone());
        self.menu_quit_id = Some(quit.id().clone());

        let Some(icon) = make_icon(GRAY) else {
            eprintln!("Failed to build the tray icon image");
            return;
        };
        // Left click toggles the window; the menu is on right click.
        let built = TrayIconBuilder::new()
            .with_tooltip("tokeman")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build();
        match built {
            Ok(tray_icon) => self.tray_icon = Some(tray_icon),
            Err(error) => eprintln!("Failed to create tray icon: {error}"),
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.window_visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.window_visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    /// Hide if visible; otherwise move to `position` (when given) and show.
    fn toggle_window(&mut self, ctx: &egui::Context, position: Option<egui::Pos2>) {
        if self.window_visible {
            self.hide_window(ctx);
        } else {
            if let Some(position) = position {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
            }
            self.show_window(ctx);
        }
    }

    fn update_tray_icon(&mut self, best: Option<&Reading>) {
        let color = status_color(best);
        if color == self.last_icon_color {
            return;
        }
        if let Some(tray_icon) = &self.tray_icon {
            let _ = tray_icon.set_icon(make_icon(color));
        }
        self.last_icon_color = color;
    }

    /// Make the account the default, then open Claude in a terminal.
    fn launch_account(&self, name: &str) {
        if let Err(error) = daemon::activate(&self.fleet, name) {
            eprintln!("Failed to activate account: {error:#}");
            return;
        }
        let (binary, args) = self.fleet.launch.command([]);
        if let Err(error) =
            launch::in_terminal(&binary, &args, self.fleet.launch.terminal.as_deref())
        {
            eprintln!("Failed to launch terminal: {error}");
        }
    }

    /// Write a change to tokens.toml as it is now (this window's copy of the
    /// credentials may be hours stale) and hand the result to the prober.
    fn save_launch_settings(&mut self, settings: LaunchSettings) {
        match Fleet::update(|fleet| {
            fleet.launch = settings;
            Ok(())
        }) {
            Ok((fresh, ())) => self.fleet = fresh,
            Err(error) => eprintln!("Failed to save settings: {error:#}"),
        }
        let _ = self.bg_tx.send(BgCmd::Fleet(Box::new(self.fleet.clone())));
    }

    fn draw_gauge(ui: &mut egui::Ui, label: &str, window: Window) {
        let remaining = window.remaining().clamp(0.0, 1.0);
        let color = level_color(Level::of(remaining));
        let remaining = remaining as f32;
        let pct = (remaining * 100.0).round() as u32;
        let bar_bg = egui::Color32::from_white_alpha(30);

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("{label:>3}"))
                    .monospace()
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
            let bar_height = 14.0;
            let bar_width = (ui.available_width() - 55.0).max(80.0);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(bar_width, bar_height), egui::Sense::hover());
            let painter = ui.painter();
            let rounding = egui::CornerRadius::same(4);
            painter.rect_filled(rect, rounding, bar_bg);
            let filled_width = rect.width() * remaining;
            if filled_width > 0.5 {
                let filled_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(filled_width, rect.height()));
                painter.rect_filled(filled_rect, rounding, color);
            }
            ui.label(
                egui::RichText::new(format!("{pct:>3}%"))
                    .monospace()
                    .size(11.0)
                    .color(egui::Color32::from_gray(200)),
            );
        });

        ui.horizontal(|ui| {
            ui.add_space(35.0);
            ui.label(
                egui::RichText::new(format!("resets {}", format_reset_compact(window.resets_at)))
                    .size(10.0)
                    .color(egui::Color32::from_gray(130)),
            );
        });
    }

    fn draw_account_card(&self, ui: &mut egui::Ui, reading: &Reading) {
        let frame = egui::Frame::NONE
            .fill(egui::Color32::from_black_alpha(140))
            .stroke(egui::Stroke::new(
                1.0_f32,
                egui::Color32::from_white_alpha(30),
            ))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(10));

        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&reading.account).strong().size(14.0));
                let (status, level) = status_badge(reading);
                let status_color = level.map_or(egui::Color32::from_gray(120), level_color);
                ui.label(egui::RichText::new(status).color(status_color).size(11.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui::RichText::new("Launch").size(11.0))
                        .clicked()
                    {
                        self.launch_account(&reading.account);
                    }
                });
            });
            ui.add_space(4.0);

            if reading.session.is_some() || reading.weekly.is_some() {
                if let Some(window) = reading.session {
                    Self::draw_gauge(ui, "5h", window);
                }
                if let Some(window) = reading.weekly {
                    Self::draw_gauge(ui, "7d", window);
                }
                if reading.buckets.is_empty() {
                    ui.label(
                        egui::RichText::new("per-model limits unavailable")
                            .monospace()
                            .size(10.0)
                            .color(egui::Color32::from_gray(110)),
                    );
                }
                for bucket in &reading.buckets {
                    let mut label = short_bucket_label(&bucket.label);
                    if bucket.source == Source::Observed {
                        label.push('!');
                    }
                    Self::draw_gauge(ui, &label, bucket.window);
                }
                if let Some(window) = reading.overage.as_ref().and_then(|o| o.window) {
                    Self::draw_gauge(ui, "$$", window);
                }
            } else if let Some(error) = &reading.error {
                ui.label(
                    egui::RichText::new(truncate_chars(&error.to_string(), 60))
                        .color(egui::Color32::from_rgb(244, 67, 54))
                        .size(11.0),
                );
            }
        });
    }

    fn draw_dangerous_toggle(&mut self, ui: &mut egui::Ui) {
        let is_on = self.fleet.launch.dangerous_mode;
        ui.horizontal(|ui| {
            let desired_size = egui::vec2(36.0, 18.0);
            let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
            if response.clicked() {
                let mut settings = self.fleet.launch.clone();
                settings.dangerous_mode = !is_on;
                self.save_launch_settings(settings);
            }

            let painter = ui.painter();
            let rounding = egui::CornerRadius::same(9);
            let track_color = if is_on {
                egui::Color32::from_rgb(244, 67, 54)
            } else {
                egui::Color32::from_gray(80)
            };
            if is_on {
                painter.rect_filled(
                    rect.expand(2.0),
                    egui::CornerRadius::same(11),
                    egui::Color32::from_rgba_unmultiplied(244, 67, 54, 40),
                );
            }
            painter.rect_filled(rect, rounding, track_color);
            let knob_radius = 7.0;
            let knob_x = if is_on {
                rect.right() - knob_radius - 2.0
            } else {
                rect.left() + knob_radius + 2.0
            };
            painter.circle_filled(
                egui::pos2(knob_x, rect.center().y),
                knob_radius,
                egui::Color32::WHITE,
            );

            let label_color = if is_on {
                egui::Color32::from_rgb(244, 67, 54)
            } else {
                egui::Color32::from_gray(120)
            };
            ui.label(
                egui::RichText::new("dangerous mode")
                    .color(label_color)
                    .size(11.0)
                    .strong(),
            );
        });
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Settings").strong().size(13.0));
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            settings_label(ui, "Launch args:");
            let mut args = self.settings_draft.launch_args.join(" ");
            let response = ui.add(
                egui::TextEdit::singleline(&mut args)
                    .desired_width(200.0)
                    .font(egui::TextStyle::Monospace),
            );
            if response.changed() {
                self.settings_draft.launch_args =
                    args.split_whitespace().map(String::from).collect();
            }
        });
        optional_text_row(
            ui,
            "Terminal:",
            &mut self.settings_draft.terminal,
            "auto-detect",
        );
        optional_text_row(
            ui,
            "Claude binary:",
            &mut self.settings_draft.claude_bin,
            "claude",
        );
        ui.horizontal(|ui| {
            settings_label(ui, "Probe interval:");
            ui.add(
                egui::DragValue::new(&mut self.settings_draft.probe_interval_secs)
                    .range(10..=300)
                    .suffix("s"),
            );
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                let draft = self.settings_draft.clone();
                self.save_launch_settings(draft);
                self.settings_open = false;
            }
            if ui.button("Cancel").clicked() {
                self.settings_draft = self.fleet.launch.clone();
                self.settings_open = false;
            }
        });
    }
}

fn settings_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(11.0)
            .color(egui::Color32::from_gray(160)),
    );
}

/// Label and a single-line edit where an empty string means unset.
fn optional_text_row(ui: &mut egui::Ui, label: &str, value: &mut Option<String>, hint: &str) {
    ui.horizontal(|ui| {
        settings_label(ui, label);
        let mut text = value.clone().unwrap_or_default();
        let response = ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(150.0)
                .hint_text(hint),
        );
        if response.changed() {
            *value = (!text.is_empty()).then_some(text);
        }
    });
}

impl eframe::App for TrayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.118, 0.118, 0.118, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tray-only until the icon is clicked.
        if self.first_frame {
            self.hide_window(ctx);
            self.first_frame = false;
        }

        if let Ok(TrayIconEvent::Click { rect, .. }) = TrayIconEvent::receiver().try_recv() {
            let x =
                rect.position.x + f64::from(rect.size.width) / 2.0 - f64::from(WINDOW_WIDTH) / 2.0;
            let y = rect.position.y + f64::from(rect.size.height) + 4.0;
            self.toggle_window(ctx, Some(egui::pos2(x as f32, y as f32)));
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if Some(&event.id) == self.menu_show_id.as_ref() {
                self.toggle_window(ctx, None);
            } else if Some(&event.id) == self.menu_refresh_id.as_ref() {
                let _ = self.bg_tx.send(BgCmd::Refresh);
            } else if Some(&event.id) == self.menu_quit_id.as_ref() {
                self.quit_requested = true;
                let _ = self.bg_tx.send(BgCmd::Shutdown);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        let (readings, last_probe, target_model) = {
            let state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (
                Arc::clone(&state.readings),
                state.last_probe,
                state.target_model.clone(),
            )
        };
        let best = select_best(&readings, &self.fleet, target_model.as_ref());
        self.update_tray_icon(best);

        // Closing the window hides it; only the menu quits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        if !readings.is_empty() {
            egui::TopBottomPanel::bottom("actions")
                .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                    left: 16,
                    right: 16,
                    top: 4,
                    bottom: 12,
                }))
                .show(ctx, |ui| {
                    ui.separator();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(
                            egui::RichText::new("\u{1F680} Launch Best")
                                .strong()
                                .size(13.0),
                        )
                        .min_size(egui::vec2(120.0, 28.0));
                        if ui.add_enabled(best.is_some(), button).clicked()
                            && let Some(reading) = best
                        {
                            self.launch_account(&reading.account);
                        }
                        if let Some(reading) = best {
                            let pct = (weekly_remaining(reading) * 100.0) as u32;
                            ui.label(
                                egui::RichText::new(format!("{} ({pct}%)", reading.account))
                                    .size(11.0)
                                    .color(egui::Color32::from_gray(140)),
                            );
                        }
                    });
                    ui.add_space(4.0);
                    self.draw_dangerous_toggle(ui);
                    if self.settings_open {
                        ui.add_space(4.0);
                        ui.separator();
                        self.draw_settings(ui);
                    }
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Tokeman").strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui::RichText::new("\u{2699}").size(16.0))
                            .on_hover_text("Settings")
                            .clicked()
                        {
                            self.settings_open = !self.settings_open;
                            if self.settings_open {
                                self.settings_draft = self.fleet.launch.clone();
                            }
                        }
                        if ui
                            .button(egui::RichText::new("\u{21BB}").size(14.0))
                            .on_hover_text("Refresh now")
                            .clicked()
                        {
                            let _ = self.bg_tx.send(BgCmd::Refresh);
                        }
                        if let Some(last) = last_probe {
                            ui.label(
                                egui::RichText::new(format!("{}s ago", last.elapsed().as_secs()))
                                    .size(10.0)
                                    .color(egui::Color32::from_gray(100)),
                            );
                        }
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                if readings.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Probing accounts...")
                                .color(egui::Color32::from_gray(120)),
                        );
                    });
                } else {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for reading in readings.iter() {
                            self.draw_account_card(ui, reading);
                            ui.add_space(6.0);
                        }
                    });
                }
            });

        // The "Ns ago" counter.
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

pub fn run(fleet: Fleet) -> Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([WINDOW_WIDTH, 520.0])
        .with_min_inner_size([360.0, 300.0])
        .with_title("Tokeman");
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "tokeman",
        options,
        Box::new(|cc| Ok(Box::new(TrayApp::new(cc, fleet)))),
    )
    .map_err(|error| anyhow!("eframe error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_disc_with_a_transparent_corner() {
        let icon = make_icon([1, 2, 3]);
        assert!(icon.is_some());
    }

    #[test]
    fn the_dot_follows_the_best_weekly_window() {
        assert_eq!(status_color(None), GRAY);
        let mut reading = Reading::failed("a", 0, crate::quota::ProbeError::NoCredential);
        reading.error = None;
        reading.weekly = Some(Window::new(0.1, 0));
        assert_eq!(status_color(Some(&reading)), level_rgb(Level::Ok));
        reading.weekly = Some(Window::new(0.9, 0));
        assert_eq!(status_color(Some(&reading)), level_rgb(Level::Critical));
    }
}
