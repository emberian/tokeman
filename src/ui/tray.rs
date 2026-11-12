//! The egui tray (feature `tray`): a menu-bar dot coloured by the best
//! Claude account's weekly room, a window with a card per account of every
//! service, launch buttons, and the launch settings, edited through
//! `Fleet::update`.
//!
//! It draws the fleet view (`ui::overview`) and reads nothing itself: its
//! background thread asks `commands::view::overview`, which is the stored
//! view while the background service runs and a read of every account
//! otherwise. "Read now" forces a read.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::account::Fleet;
use crate::commands::view;
use crate::daemon;
use crate::launch;
use crate::policy;
use crate::quota::{Level, Limit, ModelKey, Reading, Service};
use crate::settings::LaunchSettings;
use crate::ui::overview::{AccountView, Origin, Overview};
use crate::ui::text::{
    format_age, format_reset_compact, limit_name, limit_note, limits_in_order, truncate_chars,
    view_caption,
};

const WINDOW_WIDTH: f32 = 420.0;
const GRAY: [u8; 3] = [158, 158, 158];
/// How often the stored view is read again while the service keeps it fresh.
const VIEW_INTERVAL: Duration = Duration::from_secs(15);

enum BgCmd {
    /// Read every account now, whoever else reads them.
    ReadNow,
    /// The fleet changed on disk (settings); show the new one.
    Fleet(Box<Fleet>),
    Shutdown,
}

/// What the background thread hands the UI.
#[derive(Default)]
struct Shared {
    overview: Option<Arc<Overview>>,
    loaded_at: Option<Instant>,
    loading: bool,
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

/// The tray palette for [`super::level_color`].
const LEVELS: [[u8; 3]; 3] = [[76, 175, 80], [255, 193, 7], [244, 67, 54]];

fn level_rgb(level: Level) -> [u8; 3] {
    super::level_color(level, LEVELS)
}

fn level_color32(level: Level) -> egui::Color32 {
    let [r, g, b] = level_rgb(level);
    egui::Color32::from_rgb(r, g, b)
}

/// The dot's colour: the level of the most weekly room among the Claude
/// accounts that allow work, grey when none does.
fn status_color(overview: Option<&Overview>) -> [u8; 3] {
    overview
        .and_then(|overview| overview.best_weekly_remaining)
        .map_or(GRAY, |remaining| level_rgb(Level::of(remaining)))
}

/// The Claude account "Launch Best" would open: where the rotation would
/// land, judged on the view's readings.
fn best_landing(overview: &Overview, fleet: &Fleet) -> Option<(String, f64)> {
    let readings: Vec<Reading> = overview
        .of(Service::Claude)
        .filter_map(|account| account.reading.clone())
        .collect();
    let target = overview.target_model.as_deref().map(ModelKey::new);
    let best = policy::choose_landing(&readings, &fleet.policy, target.as_ref())?;
    Some((best.account.clone(), best.weekly_remaining().unwrap_or(0.0)))
}

/// Keep the shared view current: the stored view every [`VIEW_INTERVAL`]
/// while the background service reads the accounts, a read of every account
/// on the configured interval when nothing else does.
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
                crate::ui::text::notice(&format!("could not start the tray runtime: {error}"));
                return;
            }
        };
        runtime.block_on(async move {
            let mut fleet = fleet;
            let mut force = false;
            loop {
                shared.lock().unwrap_or_else(|e| e.into_inner()).loading = true;
                ctx.request_repaint();
                let overview = view::overview(&fleet, force).await;
                let viewer = overview.origin == Origin::Stored;
                {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.overview = Some(Arc::new(overview));
                    state.loaded_at = Some(Instant::now());
                    state.loading = false;
                }
                ctx.request_repaint();

                let interval = |fleet: &Fleet| {
                    if viewer {
                        VIEW_INTERVAL
                    } else {
                        Duration::from_secs(fleet.launch.probe_interval_secs.max(1))
                    }
                };
                let mut deadline = Instant::now() + interval(&fleet);
                force = loop {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(BgCmd::ReadNow) => break true,
                        Err(mpsc::RecvTimeoutError::Timeout) => break false,
                        Ok(BgCmd::Fleet(fresh)) => {
                            fleet = *fresh;
                            deadline = Instant::now() + interval(&fleet);
                        }
                        Ok(BgCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
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
        let refresh = MenuItem::new("Read Now", true, None);
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
            crate::ui::text::notice("Failed to build the tray icon image");
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
            Err(error) => crate::ui::text::notice(&format!("Failed to create tray icon: {error}")),
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

    fn update_tray_icon(&mut self, overview: Option<&Overview>) {
        let color = status_color(overview);
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
            crate::ui::text::notice(&format!("Failed to activate account: {error:#}"));
            return;
        }
        let (binary, args) = self.fleet.launch.command([]);
        if let Err(error) =
            launch::in_terminal(&binary, &args, self.fleet.launch.terminal.as_deref())
        {
            crate::ui::text::notice(&format!("Failed to launch terminal: {error}"));
        }
    }

    /// Write a change to tokens.toml as it is now (this window's copy of the
    /// credentials may be hours stale) and hand the result to the reader.
    fn save_launch_settings(&mut self, settings: LaunchSettings) {
        match Fleet::update(|fleet| {
            fleet.launch = settings;
            Ok(())
        }) {
            Ok((fresh, ())) => self.fleet = fresh,
            Err(error) => crate::ui::text::notice(&format!("Failed to save settings: {error:#}")),
        }
        let _ = self.bg_tx.send(BgCmd::Fleet(Box::new(self.fleet.clone())));
    }

    /// One limit: its name in a column `width` characters wide, a bar of
    /// what is left, the percentage, and under it the reset and what else
    /// there is to say about it.
    fn draw_limit(ui: &mut egui::Ui, limit: &Limit, reading: &Reading, width: usize) {
        let remaining = limit.window.remaining().clamp(0.0, 1.0);
        let color = level_color32(Level::of(remaining));
        let remaining = remaining as f32;
        let pct = (remaining * 100.0).round() as u32;
        let bar_bg = egui::Color32::from_white_alpha(30);

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("{:>width$}", limit_name(limit)))
                    .monospace()
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
            let bar_height = 14.0;
            let bar_width = (ui.available_width() - 55.0).max(60.0);
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
                egui::RichText::new(limit_caption(limit, reading))
                    .size(10.0)
                    .color(egui::Color32::from_gray(130)),
            );
        });
    }

    fn card_frame() -> egui::Frame {
        egui::Frame::NONE
            .fill(egui::Color32::from_black_alpha(140))
            .stroke(egui::Stroke::new(
                1.0_f32,
                egui::Color32::from_white_alpha(30),
            ))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(10))
    }

    fn caption(ui: &mut egui::Ui, text: impl Into<String>, color: egui::Color32) {
        ui.label(egui::RichText::new(text).size(10.0).color(color));
    }

    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(12.0)
                .color(egui::Color32::from_gray(170)),
        );
        ui.add_space(2.0);
    }

    /// One account of any service: who it is and its state, a gauge per
    /// limit, then what else its reading says. `aged_at` is the view's time
    /// when its readings are stored ones.
    fn draw_account(&self, ui: &mut egui::Ui, account: &AccountView, aged_at: Option<i64>) {
        Self::card_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let reading = account.reading.as_ref();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&account.name).strong().size(13.0));
                if let Some(plan) = reading.and_then(|reading| reading.plan.as_deref()) {
                    Self::caption(ui, format!("[{plan}]"), DIM);
                }
                if account.parked {
                    Self::caption(ui, "parked", DIM);
                }
                if account.held {
                    Self::caption(ui, held_word(account.service), OFFER);
                }
                if let Some(state) = &account.state {
                    let color = state.level.map_or(DIM, level_color32);
                    ui.label(
                        egui::RichText::new(truncate_chars(&state.word, 40))
                            .color(color)
                            .size(11.0),
                    );
                }
                if account.service == Service::Claude {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui::RichText::new("Launch").size(11.0))
                            .clicked()
                        {
                            self.launch_account(&account.name);
                        }
                    });
                }
            });
            let Some(reading) = reading else {
                Self::caption(ui, "not read yet", DIM);
                return;
            };
            if let Some(error) = &account.error {
                Self::caption(ui, truncate_chars(error, 160), ERROR);
            }
            let limits = limits_in_order(reading);
            let width = limits
                .iter()
                .map(|limit| limit_name(limit).chars().count())
                .max()
                .unwrap_or(0);
            for limit in limits {
                Self::draw_limit(ui, limit, reading, width);
            }
            for note in &account.notes {
                let color = if note.starts_with('!') { WARN } else { DIM };
                Self::caption(ui, note, color);
            }
            if let Some(login) = &account.login
                && let Some(note) = &login.note
            {
                Self::caption(ui, note, if login.warning { WARN } else { DIM });
            }
            if let Some(now) = aged_at {
                Self::caption(ui, format!("read {}", format_age(reading.at, now)), DIM);
            }
        });
    }

    /// Every account, under its service's name, then the reset offers.
    fn draw_overview(&self, ui: &mut egui::Ui, overview: &Overview) {
        let aged_at = (overview.origin == Origin::Stored).then_some(overview.at);
        for service in overview.services() {
            Self::section(ui, service.label());
            for account in overview.of(service) {
                self.draw_account(ui, account, aged_at);
                ui.add_space(4.0);
            }
        }
        if !overview.resets_on_offer.is_empty() {
            Self::section(ui, "Resets on offer");
            for offer in &overview.resets_on_offer {
                Self::caption(
                    ui,
                    format!(
                        "{}: {}  (read {})",
                        offer.account,
                        offer.offer,
                        format_age(offer.read_at, overview.at)
                    ),
                    OFFER,
                );
            }
        }
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

const DIM: egui::Color32 = egui::Color32::from_gray(120);
const WARN: egui::Color32 = egui::Color32::from_rgb(255, 193, 7);
const OFFER: egui::Color32 = egui::Color32::from_rgb(129, 199, 132);
const ERROR: egui::Color32 = egui::Color32::from_rgb(244, 67, 54);

/// What holding an account means for its service, in a word.
fn held_word(service: Service) -> &'static str {
    match service {
        Service::Claude => "default",
        Service::Antigravity => "agy's login",
        _ => "in use",
    }
}

/// `resets 2h10m`, `no reset pending` for an idle window, then what else
/// there is to say about the limit.
fn limit_caption(limit: &Limit, reading: &Reading) -> String {
    let reset = if limit.window.resets_at == 0 {
        "no reset pending".to_owned()
    } else {
        format!("resets {}", format_reset_compact(limit.window.resets_at))
    };
    match limit_note(limit, reading) {
        Some(note) => format!("{reset}  ·  {note}"),
        None => reset,
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
                let _ = self.bg_tx.send(BgCmd::ReadNow);
            } else if Some(&event.id) == self.menu_quit_id.as_ref() {
                self.quit_requested = true;
                let _ = self.bg_tx.send(BgCmd::Shutdown);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        let (overview, loaded_at, loading) = {
            let state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (state.overview.clone(), state.loaded_at, state.loading)
        };
        self.update_tray_icon(overview.as_deref());
        let best = overview
            .as_deref()
            .and_then(|overview| best_landing(overview, &self.fleet));

        // Closing the window hides it; only the menu quits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        if overview.is_some() {
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
                            && let Some((name, _)) = &best
                        {
                            self.launch_account(name);
                        }
                        if let Some((name, weekly)) = &best {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{name} ({}%)",
                                    (weekly * 100.0) as u32
                                ))
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
                            .on_hover_text("Read every account now")
                            .clicked()
                        {
                            let _ = self.bg_tx.send(BgCmd::ReadNow);
                        }
                        if loading {
                            Self::caption(ui, "reading...", DIM);
                        } else if let Some(at) = loaded_at {
                            Self::caption(
                                ui,
                                format!("shown {}s ago", at.elapsed().as_secs()),
                                DIM,
                            );
                        }
                    });
                });
                if let Some(overview) = &overview {
                    let stale = overview.origin == Origin::Stored
                        && overview
                            .read_at
                            .is_none_or(|at| overview.at - at > crate::ui::text::STALE_SECS);
                    Self::caption(ui, view_caption(overview), if stale { WARN } else { DIM });
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                match &overview {
                    None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(20.0);
                            ui.spinner();
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Reading the fleet view...")
                                    .color(egui::Color32::from_gray(120)),
                            );
                        });
                    }
                    Some(overview) => {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            self.draw_overview(ui, overview);
                        });
                    }
                }
            });

        // The "shown Ns ago" counter.
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
    use crate::quota::{Source, Window};
    use crate::ui::overview::Facts;

    #[test]
    fn the_icon_is_a_disc_with_a_transparent_corner() {
        let icon = make_icon([1, 2, 3]);
        assert!(icon.is_some());
    }

    #[test]
    fn a_limits_caption_is_its_reset_and_what_else_there_is_to_say() {
        let mut reading = crate::testing::reading("a", 0.0, 0.4);
        let mut fable = crate::testing::model_limit("fable", "Fable", 0.5, Source::Profile);
        fable.share = Some(0.5);
        reading.limits.push(fable);
        assert_eq!(
            limit_caption(&reading.limits[2], &reading),
            "no reset pending  ·  profile; 50% of the weekly, 25% of the weekly left for it"
        );
        reading.set_weekly(Some(Window::new(0.4, 1)));
        assert_eq!(limit_caption(&reading.limits[1], &reading), "resets now");
    }

    #[test]
    fn the_dot_and_the_launch_follow_the_view() {
        assert_eq!(status_color(None), GRAY);
        let fleet = Fleet {
            accounts: vec![
                crate::testing::setup_account("a"),
                crate::testing::setup_account("b"),
            ],
            ..Fleet::default()
        };
        let view = |a: f64, b: f64| {
            let facts = Facts {
                readings: vec![
                    crate::testing::reading("a", 0.0, a),
                    crate::testing::reading("b", 0.0, b),
                ],
                ..Facts::default()
            };
            Overview::build(&fleet, facts, None, 0)
        };
        let roomy = view(0.9, 0.1);
        assert_eq!(status_color(Some(&roomy)), level_rgb(Level::Ok));
        let (name, weekly) = best_landing(&roomy, &fleet).unwrap();
        assert_eq!(name, "b");
        assert!((weekly - 0.9).abs() < 1e-12);
        let spent = view(0.9, 0.99);
        assert_eq!(status_color(Some(&spent)), level_rgb(Level::Critical));
        assert_eq!(best_landing(&spent, &fleet).unwrap().0, "a");
        let empty = Overview::build(&fleet, Facts::default(), None, 0);
        assert_eq!(status_color(Some(&empty)), GRAY);
        assert_eq!(best_landing(&empty, &fleet), None);
    }
}
