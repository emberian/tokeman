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
use crate::chart::{Point, breaks_segment, format_duration_hours};
use crate::config::Config;
use crate::display::{claim_label, format_reset_compact, status_badge, truncate_chars};
use crate::probe::{self, Level, ModelQuotaBucket, ProbeResult, Window, level};
use crate::rotation::{self, RotationMode};
use crate::store::{Series, Snapshot, Store};

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

    fn series(&self) -> Series<'_> {
        match self {
            ChartWindow::FiveHour => Series::FiveHour,
            ChartWindow::SevenDay => Series::SevenDay,
            ChartWindow::Model { key, .. } => Series::Model(key),
            ChartWindow::Overage => Series::Overage,
        }
    }
}

/// Chart ranges in days, cycled with `[` / `]`.
const RANGE_DAYS: [i64; 6] = [1, 3, 5, 7, 14, 30];

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
    /// Index into `RANGE_DAYS`.
    chart_range: usize,
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
            chart_range: 0,
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
        let since = Utc::now() - chrono::Duration::days(self.range_days());
        self.history.clear();
        for token in &self.config.tokens {
            if let Ok(snaps) = self.store.for_token_since(&token.name, since) {
                self.history.insert(token.name.clone(), snaps);
            }
        }
    }

    fn range_days(&self) -> i64 {
        RANGE_DAYS[self.chart_range]
    }

    fn range_label(&self) -> String {
        format!("{}d", self.range_days())
    }

    fn spawn_probe(&mut self) -> tokio::task::JoinHandle<Vec<ProbeResult>> {
        self.status_msg = "Probing...".into();
        let tokens = self.config.tokens.clone();
        tokio::spawn(async move { probe::probe_all(&tokens).await })
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
            for bucket in snapshot.model_buckets() {
                model_buckets.entry(bucket.key).or_insert(bucket.label);
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
            active_probe = Some(app.spawn_probe());
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
                        active_probe = Some(app.spawn_probe());
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
                    app.chart_range = (app.chart_range + RANGE_DAYS.len() - 1) % RANGE_DAYS.len();
                    app.load_history();
                }
                KeyCode::Char(']') => {
                    app.chart_range = (app.chart_range + 1) % RANGE_DAYS.len();
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
            app.range_label()
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

/// Lines in a token card: name, quota gauges (or error), model gauges,
/// optional sparkline, spacer.
fn card_rows(result: &ProbeResult, model_rows: usize, show_spark: bool) -> u16 {
    let quota_rows = result
        .quota
        .as_ref()
        .map_or(1, |q| q.windows(["", "", ""]).count() as u16);
    2 + quota_rows + model_rows.max(1) as u16 + show_spark as u16
}

fn draw_tokens(
    f: &mut Frame,
    area: Rect,
    results: &[ProbeResult],
    selected: usize,
    history: &HashMap<String, Vec<Snapshot>>,
    default_token: Option<&str>,
) {
    let model_buckets: Vec<Vec<ModelQuotaBucket>> = results
        .iter()
        .map(|r| {
            r.model_usage
                .as_ref()
                .map(|u| u.buckets())
                .unwrap_or_default()
        })
        .collect();
    let spark_history = |i: usize, r: &ProbeResult| {
        history
            .get(&r.token_name)
            .filter(|h| i == selected && h.len() >= 2)
    };
    let heights: Vec<u16> = results
        .iter()
        .enumerate()
        .map(|(i, r)| card_rows(r, model_buckets[i].len(), spark_history(i, r).is_some()))
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
        draw_single_token(
            f,
            token_chunks[visible_index],
            result,
            &model_buckets[result_index],
            heights[result_index],
            result_index == selected,
            default_token == Some(result.token_name.as_str()),
            spark_history(result_index, result),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_single_token(
    f: &mut Frame,
    area: Rect,
    result: &ProbeResult,
    model_buckets: &[ModelQuotaBucket],
    row_count: u16,
    selected: bool,
    is_default: bool,
    spark_history: Option<&Vec<Snapshot>>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); row_count as usize])
        .split(area);

    // Name line
    let marker = match (selected, is_default) {
        (true, true) => ">D",
        (true, false) => "> ",
        (false, true) => " D",
        (false, false) => "  ",
    };
    let (status_str, status_level) = status_badge(result, "no quota");
    let status_color = status_level.map_or(Color::DarkGray, level_color);
    let claim_str = result
        .quota
        .as_ref()
        .and_then(|q| claim_label(&q.representative_claim))
        .map_or(String::new(), |(_, short)| format!(" {short}"));

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
        for (label, w) in q.windows(["5h", "7d", "$$"]) {
            f.render_widget(Paragraph::new(make_gauge_line(label, w)), rows[row_idx]);
            row_idx += 1;
        }
    } else if let Some(ref err) = result.error {
        let err_line = Line::from(Span::styled(
            format!("   error: {}", truncate_chars(err, 80)),
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
        for bucket in model_buckets {
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
    if let Some(snaps) = spark_history {
        let spark_data: Vec<u64> = snaps
            .iter()
            .filter_map(|s| s.utilization_7d)
            .map(|u| ((1.0 - u) * 100.0).round() as u64)
            .collect();
        if !spark_data.is_empty() {
            let label = "   7d ";
            let spark_area = Rect {
                x: area.x + label.len() as u16,
                y: rows[row_idx].y,
                width: rows[row_idx].width.saturating_sub(label.len() as u16 + 2),
                height: 1,
            };
            // Label
            f.render_widget(
                Paragraph::new(Span::styled(label, Style::default().fg(Color::DarkGray))),
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

fn level_color(level: Level) -> Color {
    match level {
        Level::Ok => Color::Green,
        Level::Low => Color::Yellow,
        Level::Critical => Color::Red,
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
    let remaining = window.remaining().clamp(0.0, 1.0);
    let bar_width = 30usize;
    let filled = (remaining * bar_width as f64).round() as usize;
    let empty = bar_width - filled;
    let pct = (remaining * 100.0).round() as u8;
    let color = level_color(level(remaining));

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
        let series = app.chart_window.series();
        let points: Vec<Point> = snaps
            .iter()
            .filter_map(|s| {
                let (util, reset) = s.window(series)?;
                Some(Point {
                    timestamp: s.probed_at.timestamp(),
                    remaining: 1.0 - util,
                    reset,
                })
            })
            .collect();
        if points.is_empty() {
            continue;
        }

        // Split into contiguous segments at gaps, resets and capacity jumps.
        let mut segments: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
        for (i, point) in points.iter().enumerate() {
            if i > 0 && breaks_segment(&points[i - 1], point, GAP_THRESHOLD_SECS) {
                segments.push(Vec::new());
            }
            let hours_ago = (point.timestamp as f64 - now) / 3600.0;
            segments
                .last_mut()
                .expect("segments starts non-empty")
                .push((hours_ago, point.remaining * 100.0));
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

    let range_hours = (app.range_days() * 24) as f64;
    let normal = rotation::floors(RotationMode::Normal, &app.config.rotation);
    let sip = rotation::floors(RotationMode::SipAndDrain, &app.config.rotation);
    let policy_floors = match &app.chart_window {
        ChartWindow::FiveHour => Some((normal.0 * 100.0, sip.0 * 100.0)),
        ChartWindow::SevenDay => Some((normal.1 * 100.0, sip.1 * 100.0)),
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
        Span::raw(format!("-{}", app.range_label())),
        Span::raw(format!("-{}", format_duration_hours(half))),
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
                    app.range_label(),
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
