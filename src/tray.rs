use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::{Config, LaunchSettings};
use crate::display::format_reset_compact;
use crate::launch::select_best;
use crate::probe::{self, ProbeResult, Window};
use crate::store::Store;
use crate::terminal;

// --- Background thread commands ---

enum BgCmd {
    ForceRefresh,
    UpdateConfig(Config),
    Shutdown,
}

// --- Shared state between bg thread and UI ---

struct SharedState {
    results: Vec<ProbeResult>,
    last_probe: Option<Instant>,
}

// --- Tray icon helpers ---

fn make_icon(color: [u8; 3]) -> tray_icon::Icon {
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
                // Slight anti-aliasing at the edge
                let alpha = if dist > radius - 1.0 {
                    ((radius - dist).clamp(0.0, 1.0) * 255.0) as u8
                } else {
                    255
                };
                rgba[idx] = color[0];
                rgba[idx + 1] = color[1];
                rgba[idx + 2] = color[2];
                rgba[idx + 3] = alpha;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}

fn status_color(results: &[ProbeResult]) -> [u8; 3] {
    let best = select_best(results);
    match best {
        Some(r) => {
            let remaining = r
                .quota
                .as_ref()
                .and_then(|q| q.weekly.as_ref())
                .map(|w| 1.0 - w.utilization)
                .unwrap_or(0.0);
            if remaining > 0.50 {
                [76, 175, 80] // green
            } else if remaining > 0.20 {
                [255, 193, 7] // amber
            } else {
                [244, 67, 54] // red
            }
        }
        None => [158, 158, 158], // gray
    }
}

// --- Background probing thread ---

fn spawn_bg_thread(
    config: Config,
    shared: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
) -> mpsc::Sender<BgCmd> {
    let (tx, rx) = mpsc::channel::<BgCmd>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let mut config = config;
            let store = Store::open().ok();

            loop {
                // Probe all tokens
                let results = probe::probe_all(&config.tokens).await;
                if let Some(ref s) = store {
                    for r in &results {
                        let _ = s.insert(r);
                    }
                }

                // Update shared state
                {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    state.results = results;
                    state.last_probe = Some(Instant::now());
                }
                ctx.request_repaint();

                // Wait for interval or command
                let interval_ms = config.settings.probe_interval_secs * 1000;
                let deadline = Instant::now()
                    + std::time::Duration::from_millis(interval_ms);

                loop {
                    match rx.try_recv() {
                        Ok(BgCmd::ForceRefresh) => break,
                        Ok(BgCmd::UpdateConfig(new_cfg)) => {
                            config = new_cfg;
                        }
                        Ok(BgCmd::Shutdown) => return,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        });
    });

    tx
}

// --- egui App ---

struct TokemaApp {
    config: Config,
    shared: Arc<Mutex<SharedState>>,
    bg_tx: mpsc::Sender<BgCmd>,
    tray_icon: Option<TrayIcon>,
    menu_show_id: Option<tray_icon::menu::MenuId>,
    menu_refresh_id: Option<tray_icon::menu::MenuId>,
    menu_quit_id: Option<tray_icon::menu::MenuId>,
    settings_open: bool,
    settings_draft: LaunchSettings,
    first_frame: bool,
    last_icon_color: [u8; 3],
    quit_requested: bool,
    window_visible: bool,
}

impl TokemaApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
    ) -> Self {
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

        let shared = Arc::new(Mutex::new(SharedState {
            results: Vec::new(),
            last_probe: None,
        }));

        let bg_tx = spawn_bg_thread(config.clone(), shared.clone(), cc.egui_ctx.clone());
        let settings_draft = config.settings.clone();

        let mut app = Self {
            config,
            shared,
            bg_tx,
            tray_icon: None,
            menu_show_id: None,
            menu_refresh_id: None,
            menu_quit_id: None,
            settings_open: false,
            settings_draft,
            first_frame: true,
            last_icon_color: [158, 158, 158],
            quit_requested: false,
            window_visible: true,
        };

        // Create tray icon
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

        let icon = make_icon([158, 158, 158]);
        let mut builder = TrayIconBuilder::new()
            .with_tooltip("tokeman")
            .with_icon(icon)
            .with_menu(Box::new(menu));

        // Left-click toggles window; right-click shows menu
        builder = builder.with_menu_on_left_click(false);

        match builder.build() {
            Ok(ti) => self.tray_icon = Some(ti),
            Err(e) => eprintln!("Failed to create tray icon: {e}"),
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

    fn update_tray_icon(&mut self, results: &[ProbeResult]) {
        let color = status_color(results);
        if color != self.last_icon_color {
            if let Some(ref ti) = self.tray_icon {
                let _ = ti.set_icon(Some(make_icon(color)));
            }
            self.last_icon_color = color;
        }
    }

    fn launch_token(&self, token_name: &str) {
        let token_key = self
            .config
            .tokens
            .iter()
            .find(|t| t.name == token_name)
            .map(|t| t.key.as_str());

        let Some(key) = token_key else {
            return;
        };

        let env_bin = std::env::var("TOKEMAN_CLAUDE_BIN").ok();
        let claude_bin = self
            .config
            .settings
            .claude_bin
            .as_deref()
            .or(env_bin.as_deref())
            .unwrap_or("claude");

        let mut args = self.config.settings.launch_args.clone();
        if self.config.settings.dangerous_mode
            && !args.iter().any(|a| a == "--dangerously-skip-permissions")
        {
            args.push("--dangerously-skip-permissions".into());
        }

        if let Err(e) = terminal::launch_in_terminal(
            claude_bin,
            &args,
            key,
            self.config.settings.terminal.as_deref(),
        ) {
            eprintln!("Failed to launch terminal: {e}");
        }
    }

    fn draw_gauge(ui: &mut egui::Ui, label: &str, window: &Window) {
        let remaining = (1.0 - window.utilization).clamp(0.0, 1.0) as f32;
        let pct = (remaining * 100.0).round() as u32;

        let color = if remaining > 0.50 {
            egui::Color32::from_rgb(76, 175, 80)
        } else if remaining > 0.20 {
            egui::Color32::from_rgb(255, 193, 7)
        } else {
            egui::Color32::from_rgb(244, 67, 54)
        };

        let bar_bg = egui::Color32::from_white_alpha(30);

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("{:>3}", label))
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

            // Background
            painter.rect_filled(rect, rounding, bar_bg);

            // Filled portion
            let filled_width = rect.width() * remaining;
            if filled_width > 0.5 {
                let filled_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(filled_width, rect.height()));
                painter.rect_filled(filled_rect, rounding, color);
            }

            ui.label(
                egui::RichText::new(format!("{:>3}%", pct))
                    .monospace()
                    .size(11.0)
                    .color(egui::Color32::from_gray(200)),
            );
        });

        // Reset time
        let reset_str = format_reset_compact(window.reset);
        ui.horizontal(|ui| {
            ui.add_space(35.0);
            ui.label(
                egui::RichText::new(format!("resets {}", reset_str))
                    .size(10.0)
                    .color(egui::Color32::from_gray(130)),
            );
        });
    }

    fn draw_token_card(&self, ui: &mut egui::Ui, result: &ProbeResult) {
        let frame = egui::Frame::NONE
            .fill(egui::Color32::from_black_alpha(140))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(30)))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(10));

        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            // Header: name + status + launch button
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&result.token_name)
                        .strong()
                        .size(14.0),
                );

                let (status_text, status_color) =
                    match result.quota.as_ref().map(|q| q.status.as_str()) {
                        Some("allowed") => ("allowed", egui::Color32::from_rgb(76, 175, 80)),
                        Some("allowed_warning") => {
                            ("warning", egui::Color32::from_rgb(255, 193, 7))
                        }
                        Some("rejected") => ("REJECTED", egui::Color32::from_rgb(244, 67, 54)),
                        Some(s) => (s, egui::Color32::from_rgb(255, 193, 7)),
                        None => {
                            if result.error.is_some() {
                                ("error", egui::Color32::from_rgb(244, 67, 54))
                            } else {
                                ("unknown", egui::Color32::from_gray(120))
                            }
                        }
                    };
                ui.label(egui::RichText::new(status_text).color(status_color).size(11.0));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui::RichText::new("Launch").size(11.0))
                        .clicked()
                    {
                        self.launch_token(&result.token_name);
                    }
                });
            });

            ui.add_space(4.0);

            // Gauge bars
            if let Some(ref q) = result.quota {
                if let Some(ref w) = q.session {
                    Self::draw_gauge(ui, "5h", w);
                }
                if let Some(ref w) = q.weekly {
                    Self::draw_gauge(ui, "7d", w);
                }
                if let Some(ref w) = q.overage {
                    Self::draw_gauge(ui, "$$", w);
                }
            } else if let Some(ref err) = result.error {
                let truncated: &str = match err.char_indices().nth(60) {
                    Some((idx, _)) => &err[..idx],
                    None => err,
                };
                ui.label(
                    egui::RichText::new(truncated)
                        .color(egui::Color32::from_rgb(244, 67, 54))
                        .size(11.0),
                );
            }
        });
    }

    fn draw_dangerous_toggle(&mut self, ui: &mut egui::Ui) {
        let is_on = self.config.settings.dangerous_mode;

        ui.horizontal(|ui| {
            // Custom toggle
            let desired_size = egui::vec2(36.0, 18.0);
            let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

            if response.clicked() {
                self.config.settings.dangerous_mode = !is_on;
                let _ = self.config.save();
                let _ = self.bg_tx.send(BgCmd::UpdateConfig(self.config.clone()));
            }

            let painter = ui.painter();
            let rounding = egui::CornerRadius::same(9);

            let track_color = if is_on {
                egui::Color32::from_rgb(244, 67, 54)
            } else {
                egui::Color32::from_gray(80)
            };
            painter.rect_filled(rect, rounding, track_color);

            // Glow when on
            if is_on {
                painter.rect_filled(
                    rect.expand(2.0),
                    egui::CornerRadius::same(11),
                    egui::Color32::from_rgba_unmultiplied(244, 67, 54, 40),
                );
                // Redraw track on top of glow
                painter.rect_filled(rect, rounding, track_color);
            }

            // Knob
            let knob_radius = 7.0;
            let knob_x = if is_on {
                rect.right() - knob_radius - 2.0
            } else {
                rect.left() + knob_radius + 2.0
            };
            let knob_center = egui::pos2(knob_x, rect.center().y);
            painter.circle_filled(knob_center, knob_radius, egui::Color32::WHITE);

            // Label
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
            ui.label(
                egui::RichText::new("Launch args:")
                    .size(11.0)
                    .color(egui::Color32::from_gray(160)),
            );
            let mut args_str = self.settings_draft.launch_args.join(" ");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut args_str)
                    .desired_width(200.0)
                    .font(egui::TextStyle::Monospace),
            );
            if resp.changed() {
                self.settings_draft.launch_args =
                    args_str.split_whitespace().map(String::from).collect();
            }
        });

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Terminal:")
                    .size(11.0)
                    .color(egui::Color32::from_gray(160)),
            );
            let mut term = self.settings_draft.terminal.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut term)
                    .desired_width(150.0)
                    .hint_text("auto-detect"),
            );
            if resp.changed() {
                self.settings_draft.terminal = if term.is_empty() { None } else { Some(term) };
            }
        });

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Claude binary:")
                    .size(11.0)
                    .color(egui::Color32::from_gray(160)),
            );
            let mut bin = self.settings_draft.claude_bin.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut bin)
                    .desired_width(150.0)
                    .hint_text("claude"),
            );
            if resp.changed() {
                self.settings_draft.claude_bin = if bin.is_empty() { None } else { Some(bin) };
            }
        });

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Probe interval:")
                    .size(11.0)
                    .color(egui::Color32::from_gray(160)),
            );
            ui.add(
                egui::DragValue::new(&mut self.settings_draft.probe_interval_secs)
                    .range(10..=300)
                    .suffix("s"),
            );
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.config.settings = self.settings_draft.clone();
                let _ = self.config.save();
                let _ = self.bg_tx.send(BgCmd::UpdateConfig(self.config.clone()));
                self.settings_open = false;
            }
            if ui.button("Cancel").clicked() {
                self.settings_draft = self.config.settings.clone();
                self.settings_open = false;
            }
        });
    }
}

impl eframe::App for TokemaApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.118, 0.118, 0.118, 1.0] // Opaque dark
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First-frame setup: start hidden (tray-only)
        if self.first_frame {
            self.hide_window(ctx);
            self.first_frame = false;
        }

        // Handle tray icon left-click — position below icon and toggle
        if let Ok(TrayIconEvent::Click { rect, .. }) = TrayIconEvent::receiver().try_recv() {
            if self.window_visible {
                self.hide_window(ctx);
            } else {
                // Position window centered below the tray icon
                let window_width = 420.0_f64;
                let x = rect.position.x + rect.size.width as f64 / 2.0 - window_width / 2.0;
                let y = rect.position.y + rect.size.height as f64 + 4.0;
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                    egui::pos2(x as f32, y as f32),
                ));
                self.show_window(ctx);
            }
        }

        // Handle menu events (right-click menu)
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if Some(&event.id) == self.menu_show_id.as_ref() {
                if self.window_visible {
                    self.hide_window(ctx);
                } else {
                    self.show_window(ctx);
                }
            } else if Some(&event.id) == self.menu_refresh_id.as_ref() {
                let _ = self.bg_tx.send(BgCmd::ForceRefresh);
            } else if Some(&event.id) == self.menu_quit_id.as_ref() {
                self.quit_requested = true;
                let _ = self.bg_tx.send(BgCmd::Shutdown);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Get current state
        let (results, last_probe) = {
            let state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (state.results.clone(), state.last_probe)
        };

        // Update tray icon color
        self.update_tray_icon(&results);

        // Intercept close — hide instead of quit (unless explicit quit requested)
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        // Bottom panel: actions (always visible, pinned to bottom)
        if !results.is_empty() {
            egui::TopBottomPanel::bottom("actions")
                .frame(
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: 16,
                            right: 16,
                            top: 4,
                            bottom: 12,
                        }),
                )
                .show(ctx, |ui| {
                    ui.separator();
                    ui.add_space(4.0);

                    // Launch Best button
                    let best = select_best(&results);
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(
                            egui::RichText::new("\u{1F680} Launch Best").strong().size(13.0),
                        )
                        .min_size(egui::vec2(120.0, 28.0));
                        let enabled = best.is_some();
                        if ui.add_enabled(enabled, btn).clicked()
                            && let Some(r) = best
                        {
                            self.launch_token(&r.token_name);
                        }

                        if let Some(r) = best {
                            let pct = r
                                .quota
                                .as_ref()
                                .and_then(|q| q.weekly.as_ref())
                                .map(|w| ((1.0 - w.utilization) * 100.0) as u32)
                                .unwrap_or(0);
                            ui.label(
                                egui::RichText::new(format!("{} ({}%)", r.token_name, pct))
                                    .size(11.0)
                                    .color(egui::Color32::from_gray(140)),
                            );
                        }
                    });

                    ui.add_space(4.0);
                    self.draw_dangerous_toggle(ui);

                    // Settings panel (collapsible)
                    if self.settings_open {
                        ui.add_space(4.0);
                        ui.separator();
                        self.draw_settings(ui);
                    }
                });
        }

        // Central panel: header + scrollable token cards
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                // Header
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Tokeman").strong().size(16.0));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Gear button
                        if ui
                            .button(egui::RichText::new("\u{2699}").size(16.0))
                            .on_hover_text("Settings")
                            .clicked()
                        {
                            self.settings_open = !self.settings_open;
                            if self.settings_open {
                                self.settings_draft = self.config.settings.clone();
                            }
                        }

                        // Refresh button
                        if ui
                            .button(egui::RichText::new("\u{21BB}").size(14.0))
                            .on_hover_text("Refresh now")
                            .clicked()
                        {
                            let _ = self.bg_tx.send(BgCmd::ForceRefresh);
                        }

                        // Last probe time
                        if let Some(last) = last_probe {
                            let ago = last.elapsed().as_secs();
                            ui.label(
                                egui::RichText::new(format!("{}s ago", ago))
                                    .size(10.0)
                                    .color(egui::Color32::from_gray(100)),
                            );
                        }
                    });
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                if results.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Probing tokens...")
                                .color(egui::Color32::from_gray(120)),
                        );
                    });
                } else {
                    // Token cards (scrollable)
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for result in &results {
                            self.draw_token_card(ui, result);
                            ui.add_space(6.0);
                        }
                    });
                }
            });

        // Request periodic repaint for "Xs ago" counter
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

// --- Entry point ---

pub fn run(config: Config) -> Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([420.0, 520.0])
        .with_min_inner_size([360.0, 300.0])
        .with_title("Tokeman");

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "tokeman",
        options,
        Box::new(|cc| Ok(Box::new(TokemaApp::new(cc, config)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
