use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{Local, Utc};
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::symbols;
use ratatui::widgets::*;

use crate::admission;
use crate::config::Config;
use crate::display::format_reset_compact;
use crate::probe::{self, ProbeResult, Window};
use crate::rotation;
use crate::store::{Snapshot, Store};

#[derive(Clone, PartialEq, Eq)]
enum ChartWindow {
    SevenDay,
    FiveHour,
    Model { key: String, label: String },
    Overage,
}

impl ChartWindow {
    fn label(&self) -> &str {
        match self {
            ChartWindow::FiveHour => "5h",
            ChartWindow::SevenDay => "7d",
            ChartWindow::Model { label, .. } => label,
            ChartWindow::Overage => "$$",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ChartRange {
    D1,
    D3,
    D5,
    D7,
    D14,
    D30,
}

impl ChartRange {
    fn days(self) -> i64 {
        match self {
            ChartRange::D1 => 1,
            ChartRange::D3 => 3,
            ChartRange::D5 => 5,
            ChartRange::D7 => 7,
            ChartRange::D14 => 14,
            ChartRange::D30 => 30,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ChartRange::D1 => "1d",
            ChartRange::D3 => "3d",
            ChartRange::D5 => "5d",
            ChartRange::D7 => "7d",
            ChartRange::D14 => "14d",
            ChartRange::D30 => "30d",
        }
    }

    fn next(self) -> Self {
        match self {
            ChartRange::D1 => ChartRange::D3,
            ChartRange::D3 => ChartRange::D5,
            ChartRange::D5 => ChartRange::D7,
            ChartRange::D7 => ChartRange::D14,
            ChartRange::D14 => ChartRange::D30,
            ChartRange::D30 => ChartRange::D1,
        }
    }

    fn prev(self) -> Self {
        match self {
            ChartRange::D1 => ChartRange::D30,
            ChartRange::D3 => ChartRange::D1,
            ChartRange::D5 => ChartRange::D3,
            ChartRange::D7 => ChartRange::D5,
            ChartRange::D14 => ChartRange::D7,
            ChartRange::D30 => ChartRange::D14,
        }
    }
}

struct App {
    results: Vec<ProbeResult>,
    selected: usize,
    last_probe: Instant,
    store: Store,
    config: Config,
    status_msg: String,
    show_chart: bool,
    chart_fullscreen: bool,
    chart_window: ChartWindow,
    chart_range: ChartRange,
    history: HashMap<String, Vec<Snapshot>>,
    refresh_interval: Duration,
    default_token: Option<String>,
    target_model: Option<String>,
    live_sessions: Vec<rotation::ClaudeSessionStatus>,
    policy_mode: rotation::RotationMode,
}

impl App {
    fn new(config: Config, store: Store) -> Self {
        let refresh_interval = Duration::from_secs(config.settings.probe_interval_secs.max(1));
        let default_token = rotation::default_token_name(&config).ok().flatten();
        let target_model = rotation::target_model_name().ok().flatten();
        let live_sessions = rotation::claude_sessions(&config);
        Self {
            results: Vec::new(),
            selected: 0,
            last_probe: Instant::now() - refresh_interval,
            store,
            config,
            status_msg: "Starting...".into(),
            show_chart: false,
            chart_fullscreen: false,
            chart_window: ChartWindow::SevenDay,
            chart_range: ChartRange::D1,
            history: HashMap::new(),
            refresh_interval,
            default_token,
            target_model,
            live_sessions,
            policy_mode: rotation::RotationMode::Normal,
        }
    }

    fn apply_probe_results(&mut self, mut results: Vec<ProbeResult>) {
        let _ = admission::scan_transcripts(&self.config.tokens);
        admission::apply_observed_limits(&mut results);
        for r in &results {
            let _ = self.store.insert(r);
        }
        self.results = results;
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
        self.last_probe = Instant::now();
        let ok = self.results.iter().filter(|r| r.error.is_none()).count();
        self.status_msg = format!(
            "Probed {}/{} at {}",
            ok,
            self.results.len(),
            Local::now().format("%H:%M:%S")
        );
        self.default_token = rotation::default_token_name(&self.config).ok().flatten();
        self.target_model = rotation::target_model_name().ok().flatten();
        self.live_sessions = rotation::claude_sessions(&self.config);
        self.policy_mode = rotation::safe_mode_for(
            &self.results,
            self.config.tokens.len(),
            &self.config.rotation,
            self.policy_mode,
            self.target_model.as_deref(),
        );

        // Load history for charts
        self.load_history();
    }

    fn load_history(&mut self) {
        let since = Utc::now() - chrono::Duration::days(self.chart_range.days());
        self.history.clear();
        for token in &self.config.tokens {
            if let Ok(snaps) = self.store.for_token_since(&token.name, since) {
                self.history.insert(token.name.clone(), snaps);
            }
        }
    }

    fn available_chart_windows(&self) -> Vec<ChartWindow> {
        let mut model_buckets = BTreeMap::<String, String>::new();
        for usage in self
            .results
            .iter()
            .filter_map(|result| result.model_usage.as_ref())
        {
            for bucket in usage.buckets() {
                model_buckets.entry(bucket.key).or_insert(bucket.label);
            }
        }
        for snapshot in self.history.values().flatten() {
            for bucket in &snapshot.model_usage_buckets {
                model_buckets
                    .entry(bucket.key.clone())
                    .or_insert_with(|| bucket.label.clone());
            }
            // Preserve access to history written before dynamic buckets were
            // added to the snapshot schema.
            if snapshot.utilization_opus_7d.is_some() {
                model_buckets
                    .entry("opus".into())
                    .or_insert_with(|| "Opus".into());
            }
            if snapshot.utilization_sonnet_7d.is_some() {
                model_buckets
                    .entry("sonnet".into())
                    .or_insert_with(|| "Sonnet".into());
            }
        }
        let mut windows = vec![ChartWindow::SevenDay, ChartWindow::FiveHour];
        windows.extend(
            model_buckets
                .into_iter()
                .map(|(key, label)| ChartWindow::Model { key, label }),
        );
        windows.push(ChartWindow::Overage);
        windows
    }

    fn cycle_chart_window(&mut self) {
        let windows = self.available_chart_windows();
        let current = windows
            .iter()
            .position(|window| window == &self.chart_window)
            .unwrap_or_else(|| windows.len().saturating_sub(1));
        self.chart_window = windows[(current + 1) % windows.len()].clone();
    }

    fn next_probe_in(&self) -> Duration {
        let elapsed = self.last_probe.elapsed();
        if elapsed >= self.refresh_interval {
            Duration::ZERO
        } else {
            self.refresh_interval - elapsed
        }
    }
}

struct TerminalSession;

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = std::io::stdout().execute(EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
    }
}

pub async fn run(config: Config) -> Result<()> {
    let store = Store::open()?;
    let mut app = App::new(config, store);
    let startup_executable = rotation::executable_identity();

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut active_probe: Option<tokio::task::JoinHandle<Vec<ProbeResult>>> = None;
    let mut executable_replaced = false;

    loop {
        if startup_executable.is_some()
            && rotation::executable_identity()
                .is_some_and(|current| Some(current) != startup_executable)
        {
            executable_replaced = true;
            break;
        }
        if active_probe
            .as_ref()
            .is_some_and(|probe| probe.is_finished())
        {
            let results = active_probe
                .take()
                .expect("active probe was present")
                .await?;
            app.apply_probe_results(results);
        }
        if app.last_probe.elapsed() >= app.refresh_interval && active_probe.is_none() {
            app.status_msg = "Probing...".into();
            let tokens = app.config.tokens.clone();
            active_probe = Some(tokio::spawn(async move { probe::probe_all(&tokens).await }));
        }

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('r') => {
                    if active_probe.is_none() {
                        app.status_msg = "Probing...".into();
                        let tokens = app.config.tokens.clone();
                        active_probe =
                            Some(tokio::spawn(async move { probe::probe_all(&tokens).await }));
                    }
                }
                KeyCode::Char('c') => {
                    app.show_chart = !app.show_chart;
                    app.chart_fullscreen = false;
                }
                KeyCode::Char('C') => {
                    if app.show_chart && app.chart_fullscreen {
                        app.show_chart = false;
                        app.chart_fullscreen = false;
                    } else {
                        app.show_chart = true;
                        app.chart_fullscreen = true;
                    }
                }
                KeyCode::Tab => {
                    app.cycle_chart_window();
                }
                KeyCode::Char('[') => {
                    app.chart_range = app.chart_range.prev();
                    app.load_history();
                }
                KeyCode::Char(']') => {
                    app.chart_range = app.chart_range.next();
                    app.load_history();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected > 0 {
                        app.selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j')
                    if !app.results.is_empty() && app.selected < app.results.len() - 1 =>
                {
                    app.selected += 1;
                }
                _ => {}
            }
        }
    }

    if let Some(probe) = active_probe {
        probe.abort();
    }
    drop(terminal);
    drop(terminal_session);
    if executable_replaced {
        eprintln!("tokeman was upgraded while --watch was open; rerun tokeman --watch");
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Header
    let countdown = app.next_probe_in();
    let mode = app.policy_mode;
    let (floor_5h, floor_7d) = rotation::floors(mode, &app.config.rotation);
    let header_text = format!(
        " Tokeman  |  {}  |  default(new): {} / {}  |  next {}s\n live pins (~=estimate): {}  |  {} floors: {:.0}% 5h / {:.0}% 7d",
        app.status_msg,
        app.default_token.as_deref().unwrap_or("/login"),
        app.target_model.as_deref().unwrap_or("model?"),
        countdown.as_secs(),
        rotation::session_summary(&app.live_sessions),
        mode.label(),
        floor_5h * 100.0,
        floor_7d * 100.0,
    );
    let header = Paragraph::new(header_text)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    // Middle: token list + optional chart
    if app.results.is_empty() {
        let empty = Paragraph::new("  No results yet. Waiting for first probe...")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, chunks[1]);
    } else if app.show_chart && app.chart_fullscreen {
        draw_chart(f, chunks[1], app);
    } else if app.show_chart {
        let mid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[1]);
        draw_tokens(
            f,
            mid[0],
            &app.results,
            app.selected,
            &app.history,
            app.default_token.as_deref(),
        );
        draw_chart(f, mid[1], app);
    } else {
        draw_tokens(
            f,
            chunks[1],
            &app.results,
            app.selected,
            &app.history,
            app.default_token.as_deref(),
        );
    }

    // Footer
    let footer_text = if app.show_chart {
        format!(
            " q: quit  r: refresh  c: split  C: full-screen  Tab: window [{}]  [/]: range [{}]",
            app.chart_window.label(),
            app.chart_range.label()
        )
    } else {
        format!(
            " q: quit  r: refresh  j/k: navigate  c/C: charts  |  D=default(new), !=observed rejection  |  {} + model feedback",
            crate::probe::PROBE_MODEL_LABEL
        )
    };
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

fn draw_tokens(
    f: &mut Frame,
    area: Rect,
    results: &[ProbeResult],
    selected: usize,
    history: &HashMap<String, Vec<Snapshot>>,
    default_token: Option<&str>,
) {
    // Each token: name + gauges + optional sparkline for selected + spacer
    let heights: Vec<u16> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let gauge_lines = r.quota.as_ref().map_or(1, |q| {
                q.session.is_some() as u16 + q.weekly.is_some() as u16 + q.overage.is_some() as u16
            }) + r
                .model_usage
                .as_ref()
                .map(|usage| usage.buckets().len().max(1) as u16)
                .unwrap_or(1);
            let sparkline =
                if i == selected && history.get(&r.token_name).is_some_and(|h| h.len() >= 2) {
                    1
                } else {
                    0
                };
            2 + gauge_lines + sparkline
        })
        .collect();

    // Keep the selected card visible when the fleet is taller than the terminal.
    let mut start = 0usize;
    let mut used_through_selected: u16 = heights.iter().take(selected + 1).sum();
    while used_through_selected > area.height && start < selected {
        used_through_selected = used_through_selected.saturating_sub(heights[start]);
        start += 1;
    }
    let mut end = start;
    let mut used = 0u16;
    while end < results.len() && used.saturating_add(heights[end]) <= area.height {
        used = used.saturating_add(heights[end]);
        end += 1;
    }
    if end == start && start < results.len() {
        end += 1;
    }
    let constraints: Vec<_> = heights[start..end]
        .iter()
        .copied()
        .map(Constraint::Length)
        .collect();
    let token_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (visible_index, result) in results[start..end].iter().enumerate() {
        let result_index = start + visible_index;
        let snaps = if result_index == selected {
            history.get(&result.token_name)
        } else {
            None
        };
        draw_single_token(
            f,
            token_chunks[visible_index],
            result,
            result_index == selected,
            default_token == Some(result.token_name.as_str()),
            snaps,
        );
    }
}

fn draw_single_token(
    f: &mut Frame,
    area: Rect,
    result: &ProbeResult,
    selected: bool,
    is_default: bool,
    history: Option<&Vec<Snapshot>>,
) {
    let mut row_constraints = vec![Constraint::Length(1)]; // name line

    if let Some(ref q) = result.quota {
        if q.session.is_some() {
            row_constraints.push(Constraint::Length(1));
        }
        if q.weekly.is_some() {
            row_constraints.push(Constraint::Length(1));
        }
        if q.overage.is_some() {
            row_constraints.push(Constraint::Length(1));
        }
    } else {
        row_constraints.push(Constraint::Length(1)); // error line
    }
    let model_buckets = result
        .model_usage
        .as_ref()
        .map(|usage| usage.buckets())
        .unwrap_or_default();
    let model_row_count = model_buckets.len().max(1);
    row_constraints.extend((0..model_row_count).map(|_| Constraint::Length(1)));

    // Sparkline row for selected token
    let show_spark = selected && history.is_some_and(|h| h.len() >= 2);
    if show_spark {
        row_constraints.push(Constraint::Length(1));
    }

    row_constraints.push(Constraint::Length(1)); // spacer

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    // Name line
    let marker = match (selected, is_default) {
        (true, true) => ">D",
        (true, false) => "> ",
        (false, true) => " D",
        (false, false) => "  ",
    };
    let (status_str, status_color) = match result.quota.as_ref().map(|q| q.status.as_str()) {
        Some("allowed") => ("allowed", Color::Green),
        Some("allowed_warning") => ("warning", Color::Yellow),
        Some("rejected") => ("REJECTED", Color::Red),
        Some(s) => (s, Color::Yellow),
        None => {
            if result.error.is_some() {
                ("error", Color::Red)
            } else {
                ("no quota", Color::DarkGray)
            }
        }
    };

    let claim_str = result
        .quota
        .as_ref()
        .map(|q| match q.representative_claim.as_str() {
            "five_hour" => " session",
            "seven_day" => " weekly",
            "seven_day_opus" => " Opus",
            "seven_day_sonnet" => " Sonnet",
            "overage" => " extra",
            _ => "",
        })
        .unwrap_or("");

    let name_line = Line::from(vec![
        Span::styled(
            format!("{marker} "),
            Style::default().fg(if is_default {
                Color::LightGreen
            } else {
                Color::Cyan
            }),
        ),
        Span::styled(
            &result.token_name,
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(status_str, Style::default().fg(status_color)),
        Span::styled(claim_str, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(name_line), rows[0]);

    let mut row_idx = 1;
    if let Some(ref q) = result.quota {
        if let Some(ref w) = q.session {
            let gauge = make_gauge_line("5h", w);
            f.render_widget(Paragraph::new(gauge), rows[row_idx]);
            row_idx += 1;
        }
        if let Some(ref w) = q.weekly {
            let gauge = make_gauge_line("7d", w);
            f.render_widget(Paragraph::new(gauge), rows[row_idx]);
            row_idx += 1;
        }
        if let Some(ref w) = q.overage {
            let gauge = make_gauge_line("$$", w);
            f.render_widget(Paragraph::new(gauge), rows[row_idx]);
            row_idx += 1;
        }
    } else if let Some(ref err) = result.error {
        let truncated: &str = match err.char_indices().nth(80) {
            Some((idx, _)) => &err[..idx],
            None => err,
        };
        let err_line = Line::from(Span::styled(
            format!("   error: {truncated}"),
            Style::default().fg(Color::Red),
        ));
        f.render_widget(Paragraph::new(err_line), rows[row_idx]);
        row_idx += 1;
    }
    if model_buckets.is_empty() {
        let reason = if result.model_usage_error.is_some() {
            "profile credential expired/unavailable"
        } else {
            "profile usage not captured"
        };
        let line = Line::from(vec![
            Span::raw("   M7  "),
            Span::styled(reason, Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), rows[row_idx]);
        row_idx += 1;
    } else {
        for bucket in &model_buckets {
            let label = compact_bucket_label(
                &bucket.label,
                bucket.source == crate::probe::ModelUsageSource::ObservedRejection,
            );
            f.render_widget(
                Paragraph::new(make_gauge_line(&label, &bucket.window)),
                rows[row_idx],
            );
            row_idx += 1;
        }
    }

    // Sparkline for the selected token's 7d utilization
    if show_spark && let Some(snaps) = history {
        let spark_data: Vec<u64> = snaps
            .iter()
            .filter_map(|s| s.utilization_7d)
            .map(|u| ((1.0 - u) * 100.0).round() as u64)
            .collect();
        if !spark_data.is_empty() {
            let label = "   7d ".to_string();
            let spark_area = Rect {
                x: area.x + label.len() as u16,
                y: rows[row_idx].y,
                width: rows[row_idx].width.saturating_sub(label.len() as u16 + 2),
                height: 1,
            };
            // Label
            f.render_widget(
                Paragraph::new(Span::styled(&label, Style::default().fg(Color::DarkGray))),
                Rect {
                    x: area.x,
                    y: rows[row_idx].y,
                    width: label.len() as u16,
                    height: 1,
                },
            );
            let sparkline = Sparkline::default()
                .data(&spark_data)
                .style(Style::default().fg(Color::Cyan));
            f.render_widget(sparkline, spark_area);
        }
    }
}

fn compact_bucket_label(label: &str, rejected: bool) -> String {
    let mut compact = label
        .split_whitespace()
        .map(|part| part.chars().next().unwrap_or('?'))
        .collect::<String>();
    if compact.len() < 2 {
        compact = label.chars().take(3).collect();
    }
    compact.truncate(3);
    format!("{compact}{}", if rejected { "!" } else { "7" })
}

fn make_gauge_line<'a>(label: &str, window: &Window) -> Line<'a> {
    let remaining = (1.0 - window.utilization).clamp(0.0, 1.0);
    let bar_width = 30usize;
    let filled = (remaining * bar_width as f64).round() as usize;
    let empty = bar_width - filled;
    let pct = (remaining * 100.0).round() as u8;

    let color = if remaining > 0.50 {
        Color::Green
    } else if remaining > 0.20 {
        Color::Yellow
    } else {
        Color::Red
    };

    let reset = format_reset_compact(window.reset);

    Line::from(vec![
        Span::raw(format!("   {label} ")),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!(" {:>3}% left", pct)),
        Span::styled(
            format!("  resets {reset}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn draw_chart(f: &mut Frame, area: Rect, app: &App) {
    let now = Utc::now().timestamp() as f64;
    let mode = app.policy_mode;
    let colors = [
        Color::Cyan,
        Color::Yellow,
        Color::Green,
        Color::Magenta,
        Color::Red,
        Color::LightBlue,
        Color::LightRed,
        Color::White,
    ];

    const GAP_THRESHOLD_SECS: i64 = 600; // 10 min — don't interpolate across gaps

    // Build segments per token, split at time gaps to avoid misleading interpolation
    // Each entry: (name, color_index, Vec<segment>) where segment = Vec<(f64, f64)>
    struct ChartEntry {
        name: String,
        color: Color,
        segments: Vec<Vec<(f64, f64)>>,
    }

    let mut entries: Vec<ChartEntry> = Vec::new();
    let mut sorted_history: Vec<_> = app.history.iter().collect();
    sorted_history.sort_by(|a, b| a.0.cmp(b.0));

    for (idx, (name, snaps)) in sorted_history.iter().enumerate() {
        let color = colors[idx % colors.len()];
        // Collect points with original timestamps for gap detection
        let points_with_ts: Vec<(i64, f64, f64, Option<i64>)> = snaps
            .iter()
            .filter_map(|s| {
                let (util, reset) = match &app.chart_window {
                    ChartWindow::FiveHour => (s.utilization_5h?, s.reset_5h),
                    ChartWindow::SevenDay => (s.utilization_7d?, s.reset_7d),
                    ChartWindow::Model { key, .. } => {
                        if let Some(bucket) = s
                            .model_usage_buckets
                            .iter()
                            .find(|bucket| bucket.key == *key)
                        {
                            (bucket.window.utilization, Some(bucket.window.reset))
                        } else {
                            match key.as_str() {
                                "opus" => (s.utilization_opus_7d?, s.reset_opus_7d),
                                "sonnet" => (s.utilization_sonnet_7d?, s.reset_sonnet_7d),
                                _ => return None,
                            }
                        }
                    }
                    ChartWindow::Overage => (s.utilization_overage?, s.reset_overage),
                };
                let ts = s.probed_at.timestamp();
                let hours_ago = (ts as f64 - now) / 3600.0;
                let remaining = (1.0 - util) * 100.0;
                Some((ts, hours_ago, remaining, reset))
            })
            .collect();

        if points_with_ts.is_empty() {
            continue;
        }

        // Split into contiguous segments at gaps > threshold
        let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut cur: Vec<(f64, f64)> = vec![(points_with_ts[0].1, points_with_ts[0].2)];
        for i in 1..points_with_ts.len() {
            let reset_changed = points_with_ts[i - 1].3.is_some()
                && points_with_ts[i].3.is_some()
                && points_with_ts[i - 1].3 != points_with_ts[i].3;
            let capacity_jumped = points_with_ts[i].2 > points_with_ts[i - 1].2 + 2.5;
            if points_with_ts[i].0 - points_with_ts[i - 1].0 > GAP_THRESHOLD_SECS
                || reset_changed
                || capacity_jumped
            {
                segments.push(std::mem::take(&mut cur));
            }
            cur.push((points_with_ts[i].1, points_with_ts[i].2));
        }
        if !cur.is_empty() {
            segments.push(cur);
        }

        entries.push(ChartEntry {
            name: if app.default_token.as_deref() == Some(name.as_str()) {
                format!("D {name}")
            } else {
                (*name).clone()
            },
            color,
            segments,
        });
    }

    let range_hours = (app.chart_range.days() * 24) as f64;
    let policy_floors = match &app.chart_window {
        ChartWindow::FiveHour => Some((
            app.config.rotation.normal_min_5h_remaining * 100.0,
            app.config.rotation.sip_min_5h_remaining * 100.0,
        )),
        ChartWindow::SevenDay => Some((
            app.config.rotation.normal_min_7d_remaining * 100.0,
            app.config.rotation.sip_min_7d_remaining * 100.0,
        )),
        ChartWindow::Model { .. } | ChartWindow::Overage => None,
    };
    let normal_floor_data =
        policy_floors.map(|(normal, _)| vec![(-range_hours, normal), (0.0, normal)]);
    let sip_floor_data = policy_floors.map(|(_, sip)| vec![(-range_hours, sip), (0.0, sip)]);

    // Flatten segments into datasets; only the first segment per token gets the name
    let mut datasets: Vec<Dataset> = entries
        .iter()
        .flat_map(|entry| {
            entry.segments.iter().enumerate().map(move |(i, seg)| {
                Dataset::default()
                    .name(if i == 0 { entry.name.as_str() } else { "" })
                    .marker(symbols::Marker::Braille)
                    .style(Style::default().fg(entry.color))
                    .graph_type(GraphType::Line)
                    .data(seg)
            })
        })
        .collect();
    if let Some(ref data) = normal_floor_data {
        datasets.push(
            Dataset::default()
                .name(format!("normal {:.0}%", data[0].1))
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Yellow))
                .graph_type(GraphType::Line)
                .data(data),
        );
    }
    if let Some(ref data) = sip_floor_data {
        datasets.push(
            Dataset::default()
                .name(format!("sip {:.0}%", data[0].1))
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::LightRed))
                .graph_type(GraphType::Line)
                .data(data),
        );
    }

    let half = range_hours / 2.0;
    let x_labels = vec![
        Span::raw(format!("-{}", app.chart_range.label())),
        Span::raw(format_axis_offset(half)),
        Span::raw("now"),
    ];

    let signal_label = match &app.chart_window {
        ChartWindow::Model { .. } => "profile usage + observed rejection signal",
        ChartWindow::FiveHour | ChartWindow::SevenDay | ChartWindow::Overage => {
            "Haiku signal + observed rejection feedback"
        }
    };
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(format!(
                    " {} remaining ({}) · {} mode · {} ",
                    app.chart_window.label(),
                    app.chart_range.label(),
                    mode.label(),
                    signal_label,
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .x_axis(
            Axis::default()
                .title("hours ago")
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-range_hours, 0.0])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("remaining %")
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, 100.0])
                .labels(vec![Span::raw("0%"), Span::raw("50%"), Span::raw("100%")]),
        )
        .legend_position(Some(LegendPosition::BottomLeft));

    f.render_widget(chart, area);
}

fn format_axis_offset(hours: f64) -> String {
    if hours >= 24.0 && (hours / 24.0).fract().abs() < f64::EPSILON {
        format!("-{:.0}d", hours / 24.0)
    } else {
        format!("-{hours:.0}h")
    }
}
