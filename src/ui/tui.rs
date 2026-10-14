//! `tokeman --watch`: a ratatui dashboard that probes on an interval, shows
//! a card per account, and charts the history over six ranges.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::symbols;
use ratatui::widgets::*;
use tokio::task::JoinHandle;

use crate::account::Fleet;
use crate::anthropic::Client;
use crate::anthropic::probe::{PROBE_MODEL, PROBE_MODEL_LABEL, PROBE_TIMEOUT, probe_all};
use crate::claude::sessions::{self, Session};
use crate::claude::settings as claude_settings;
use crate::daemon;
use crate::observe;
use crate::policy::{self, Mode};
use crate::quota::{Level, ModelKey, Reading, Source, Window};
use crate::store::{Series, Snapshot, Store};
use crate::ui::chart::{Point, format_duration_hours, known_buckets, points, segments};
use crate::ui::text::{
    claim_label, format_reset_compact, short_bucket_label, status_badge, truncate_chars,
};

/// Chart ranges in days, cycled with `[` / `]`.
const RANGE_DAYS: [i64; 6] = [1, 3, 5, 7, 14, 30];
/// Points further apart than this are not joined.
const GAP_SECS: i64 = 600;

/// The series the chart pane shows, cycled with Tab.
#[derive(Clone, PartialEq, Eq)]
enum ChartWindow {
    Weekly,
    Session,
    Model { key: ModelKey, label: String },
    Overage,
}

impl ChartWindow {
    fn label(&self) -> &str {
        match self {
            Self::Session => "5h",
            Self::Weekly => "7d",
            Self::Model { label, .. } => label,
            Self::Overage => "$$",
        }
    }

    fn series(&self) -> Series<'_> {
        match self {
            Self::Session => Series::Session,
            Self::Weekly => Series::Weekly,
            Self::Model { key, .. } => Series::Model(key),
            Self::Overage => Series::Overage,
        }
    }

    fn signal_label(&self) -> &'static str {
        match self {
            Self::Model { .. } => "profile usage + observed rejection signal",
            Self::Session | Self::Weekly | Self::Overage => {
                "Haiku signal + observed rejection feedback"
            }
        }
    }
}

/// What the header says about the last probe.
#[derive(Clone, Copy)]
enum Status {
    Starting,
    Probing,
    Probed {
        ok: usize,
        total: usize,
        at: DateTime<Local>,
    },
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => f.write_str("Starting..."),
            Self::Probing => f.write_str("Probing..."),
            Self::Probed { ok, total, at } => {
                write!(f, "Probed {ok}/{total} at {}", at.format("%H:%M:%S"))
            }
        }
    }
}

struct App {
    fleet: Fleet,
    store: Store,
    readings: Vec<Reading>,
    selected: usize,
    last_probe: Instant,
    status: Status,
    show_chart: bool,
    chart_fullscreen: bool,
    chart_window: ChartWindow,
    /// Index into `RANGE_DAYS`.
    chart_range: usize,
    history: BTreeMap<String, Vec<Snapshot>>,
    refresh_interval: Duration,
    default_account: Option<String>,
    /// The model Claude starts on, as settings.json spells it.
    target_model: Option<String>,
    /// `None` until the first census, or when the census failed.
    live_sessions: Option<Vec<Session>>,
    mode: Mode,
}

impl App {
    fn new(fleet: Fleet, store: Store) -> Self {
        let refresh_interval = Duration::from_secs(fleet.launch.probe_interval_secs.max(1));
        let default_account = daemon::default_account(&fleet).ok().flatten();
        Self {
            store,
            readings: Vec::new(),
            selected: 0,
            last_probe: Instant::now() - refresh_interval,
            status: Status::Starting,
            show_chart: false,
            chart_fullscreen: false,
            chart_window: ChartWindow::Weekly,
            chart_range: 0,
            history: BTreeMap::new(),
            refresh_interval,
            default_account,
            target_model: target_model(),
            live_sessions: None,
            mode: Mode::Normal,
            fleet,
        }
    }

    fn target_key(&self) -> Option<ModelKey> {
        self.target_model.as_deref().map(ModelKey::new)
    }

    async fn apply_readings(&mut self, mut readings: Vec<Reading>) {
        let _ = observe::scan_transcripts(&self.fleet.accounts);
        observe::apply(&mut readings);
        for reading in &readings {
            let _ = self.store.insert(reading);
        }
        self.readings = readings;
        if self.selected >= self.readings.len() {
            self.selected = self.readings.len().saturating_sub(1);
        }
        self.last_probe = Instant::now();
        self.status = Status::Probed {
            ok: self.readings.iter().filter(|r| r.error.is_none()).count(),
            total: self.readings.len(),
            at: Local::now(),
        };
        self.default_account = daemon::default_account(&self.fleet).ok().flatten();
        self.target_model = target_model();
        self.live_sessions = sessions::live().await.ok();
        self.mode = policy::safe_mode_for(
            &self.readings,
            self.fleet.accounts.len(),
            &self.fleet.policy,
            self.mode,
            self.target_key().as_ref(),
        );
        self.load_history();
    }

    fn load_history(&mut self) {
        let since = Utc::now() - chrono::Duration::days(self.range_days());
        self.history.clear();
        for account in &self.fleet.accounts {
            if let Ok(snapshots) = self.store.for_account_since(&account.name, since) {
                self.history.insert(account.name.clone(), snapshots);
            }
        }
    }

    fn range_days(&self) -> i64 {
        RANGE_DAYS[self.chart_range]
    }

    fn range_label(&self) -> String {
        format!("{}d", self.range_days())
    }

    fn spawn_probe(&mut self) -> JoinHandle<Vec<Reading>> {
        self.status = Status::Probing;
        let accounts = self.fleet.accounts.clone();
        tokio::spawn(async move {
            let now = Utc::now().timestamp();
            probe_all(Client::shared(), &accounts, PROBE_MODEL, PROBE_TIMEOUT, now).await
        })
    }

    fn available_chart_windows(&self) -> Vec<ChartWindow> {
        let mut buckets = BTreeMap::<ModelKey, String>::new();
        for bucket in self.readings.iter().flat_map(|r| r.buckets.iter()) {
            buckets
                .entry(bucket.key.clone())
                .or_insert_with(|| bucket.label.clone());
        }
        for snapshots in self.history.values() {
            for (key, label) in known_buckets(snapshots) {
                buckets.entry(key).or_insert(label);
            }
        }
        let mut windows = vec![ChartWindow::Weekly, ChartWindow::Session];
        windows.extend(
            buckets
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
        self.refresh_interval
            .saturating_sub(self.last_probe.elapsed())
    }
}

fn target_model() -> Option<String> {
    let settings = claude_settings::load().ok()?;
    claude_settings::target_model(&settings).map(str::to_owned)
}

/// Raw mode and the alternate screen, restored on drop whatever happens.
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

pub async fn run(fleet: Fleet) -> Result<()> {
    let store = Store::open()?;
    let mut app = App::new(fleet, store);
    app.live_sessions = sessions::live().await.ok();
    let started_as = daemon::executable_identity();

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut active_probe: Option<JoinHandle<Vec<Reading>>> = None;
    let mut executable_replaced = false;

    loop {
        if started_as.is_some()
            && daemon::executable_identity().is_some_and(|current| Some(current) != started_as)
        {
            executable_replaced = true;
            break;
        }
        if active_probe.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(probe) = active_probe.take()
        {
            app.apply_readings(probe.await?).await;
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
                KeyCode::Tab => app.cycle_chart_window(),
                KeyCode::Char('[') => {
                    app.chart_range = (app.chart_range + RANGE_DAYS.len() - 1) % RANGE_DAYS.len();
                    app.load_history();
                }
                KeyCode::Char(']') => {
                    app.chart_range = (app.chart_range + 1) % RANGE_DAYS.len();
                    app.load_history();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.selected = app.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if app.selected + 1 < app.readings.len() => {
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

    let floors = app.fleet.policy.floors(app.mode);
    let header_text = format!(
        " Tokeman  |  {}  |  default(new): {} / {}  |  next {}s\n live pins (~=estimate): {}  |  {} floors: {:.0}% 5h / {:.0}% 7d",
        app.status,
        app.default_account.as_deref().unwrap_or("/login"),
        app.target_model.as_deref().unwrap_or("model?"),
        app.next_probe_in().as_secs(),
        sessions::summary(app.live_sessions.as_deref()),
        app.mode.label(),
        floors.session * 100.0,
        floors.weekly * 100.0,
    );
    let header = Paragraph::new(header_text)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    if app.readings.is_empty() {
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
        draw_cards(f, mid[0], app);
        draw_chart(f, mid[1], app);
    } else {
        draw_cards(f, chunks[1], app);
    }

    let footer_text = if app.show_chart {
        format!(
            " q: quit  r: refresh  c: split  C: full-screen  Tab: window [{}]  [/]: range [{}]",
            app.chart_window.label(),
            app.range_label()
        )
    } else {
        format!(
            " q: quit  r: refresh  j/k: navigate  c/C: charts  |  D=default(new), !=observed rejection  |  {PROBE_MODEL_LABEL} + model feedback"
        )
    };
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

/// The shared windows a card shows, with their gauge labels.
fn shared_windows(reading: &Reading) -> Vec<(&'static str, Window)> {
    [
        ("5h", reading.session),
        ("7d", reading.weekly),
        ("$$", reading.overage.as_ref().and_then(|o| o.window)),
    ]
    .into_iter()
    .filter_map(|(label, window)| Some((label, window?)))
    .collect()
}

/// Lines in a card: name, quota gauges (or the error), model gauges,
/// optional sparkline, spacer.
fn card_rows(reading: &Reading, show_spark: bool) -> u16 {
    let quota_rows = shared_windows(reading).len().max(1) as u16;
    2 + quota_rows + reading.buckets.len().max(1) as u16 + u16::from(show_spark)
}

/// Which cards fit, keeping the selected one visible when the fleet is
/// taller than the terminal: `(first, end)` indices.
fn visible_range(heights: &[u16], selected: usize, available: u16) -> (usize, usize) {
    let mut start = 0usize;
    let mut used_through_selected: u16 = heights.iter().take(selected + 1).sum();
    while used_through_selected > available && start < selected {
        used_through_selected = used_through_selected.saturating_sub(heights[start]);
        start += 1;
    }
    let mut end = start;
    let mut used = 0u16;
    while end < heights.len() && used.saturating_add(heights[end]) <= available {
        used = used.saturating_add(heights[end]);
        end += 1;
    }
    if end == start && start < heights.len() {
        end += 1;
    }
    (start, end)
}

fn draw_cards(f: &mut Frame, area: Rect, app: &App) {
    let spark_history = |index: usize, reading: &Reading| {
        app.history
            .get(&reading.account)
            .filter(|h| index == app.selected && h.len() >= 2)
    };
    let heights: Vec<u16> = app
        .readings
        .iter()
        .enumerate()
        .map(|(i, r)| card_rows(r, spark_history(i, r).is_some()))
        .collect();
    let (start, end) = visible_range(&heights, app.selected, area.height);
    let constraints: Vec<_> = heights[start..end]
        .iter()
        .copied()
        .map(Constraint::Length)
        .collect();
    let card_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (visible, reading) in app.readings[start..end].iter().enumerate() {
        let index = start + visible;
        draw_card(
            f,
            card_areas[visible],
            reading,
            heights[index],
            index == app.selected,
            app.default_account.as_deref() == Some(reading.account.as_str()),
            spark_history(index, reading),
        );
    }
}

fn draw_card(
    f: &mut Frame,
    area: Rect,
    reading: &Reading,
    row_count: u16,
    selected: bool,
    is_default: bool,
    spark_history: Option<&Vec<Snapshot>>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); row_count as usize])
        .split(area);

    let marker = match (selected, is_default) {
        (true, true) => ">D",
        (true, false) => "> ",
        (false, true) => " D",
        (false, false) => "  ",
    };
    let (status, level) = status_badge(reading);
    let status_color = level.map_or(Color::DarkGray, level_color);
    let claim = reading
        .claim
        .as_deref()
        .and_then(claim_label)
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
            &reading.account,
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(claim, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(name_line), rows[0]);

    let mut row = 1;
    let windows = shared_windows(reading);
    if !windows.is_empty() {
        for (label, window) in windows {
            f.render_widget(Paragraph::new(gauge_line(label, window)), rows[row]);
            row += 1;
        }
    } else {
        let (text, color) = match &reading.error {
            Some(error) => (
                format!("   error: {}", truncate_chars(&error.to_string(), 80)),
                Color::Red,
            ),
            None => ("   no unified quota headers".to_owned(), Color::DarkGray),
        };
        let line = Line::from(Span::styled(text, Style::default().fg(color)));
        f.render_widget(Paragraph::new(line), rows[row]);
        row += 1;
    }
    if reading.buckets.is_empty() {
        let reason = if reading.profile_error.is_some() {
            "profile credential expired/unavailable"
        } else {
            "profile usage not captured"
        };
        let line = Line::from(vec![
            Span::raw("   M7  "),
            Span::styled(reason, Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), rows[row]);
        row += 1;
    } else {
        for bucket in &reading.buckets {
            let label = compact_bucket_label(&bucket.label, bucket.source == Source::Observed);
            f.render_widget(Paragraph::new(gauge_line(&label, bucket.window)), rows[row]);
            row += 1;
        }
    }

    if let Some(snapshots) = spark_history {
        let spark_data: Vec<u64> = snapshots
            .iter()
            .filter_map(|s| s.weekly)
            .map(|w| (w.remaining() * 100.0).round().clamp(0.0, 100.0) as u64)
            .collect();
        if !spark_data.is_empty() {
            let label = "   7d ";
            let label_width = label.len() as u16;
            f.render_widget(
                Paragraph::new(Span::styled(label, Style::default().fg(Color::DarkGray))),
                Rect {
                    x: area.x,
                    y: rows[row].y,
                    width: label_width,
                    height: 1,
                },
            );
            let spark_area = Rect {
                x: area.x + label_width,
                y: rows[row].y,
                width: rows[row].width.saturating_sub(label_width + 2),
                height: 1,
            };
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

/// `O48`, with `!` when the bucket is an observed rejection.
fn compact_bucket_label(label: &str, rejected: bool) -> String {
    let short = short_bucket_label(label);
    if rejected { format!("{short}!") } else { short }
}

fn gauge_line<'a>(label: &str, window: Window) -> Line<'a> {
    let remaining = window.remaining().clamp(0.0, 1.0);
    let bar_width = 30usize;
    let filled = ((remaining * bar_width as f64).round() as usize).min(bar_width);
    let empty = bar_width - filled;
    let pct = (remaining * 100.0).round() as u8;
    let color = level_color(Level::of(remaining));
    let reset = format_reset_compact(window.resets_at);

    Line::from(vec![
        Span::raw(format!("   {label} ")),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!(" {pct:>3}% left")),
        Span::styled(
            format!("  resets {reset}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// One account's line, split where joining points would mislead.
struct ChartEntry {
    name: String,
    color: Color,
    /// `(hours ago, percent left)` runs.
    segments: Vec<Vec<(f64, f64)>>,
}

/// The chart's data points, `(hours before now, percent left)`.
fn chart_segments(points: &[Point], now: i64) -> Vec<Vec<(f64, f64)>> {
    segments(points, GAP_SECS)
        .into_iter()
        .map(|run| {
            run.into_iter()
                .map(|p| ((p.at - now) as f64 / 3600.0, p.remaining * 100.0))
                .collect()
        })
        .collect()
}

fn draw_chart(f: &mut Frame, area: Rect, app: &App) {
    let now = Utc::now().timestamp();
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

    let series = app.chart_window.series();
    let entries: Vec<ChartEntry> = app
        .history
        .iter()
        .enumerate()
        .filter_map(|(index, (name, snapshots))| {
            let points = points(snapshots, series);
            if points.is_empty() {
                return None;
            }
            Some(ChartEntry {
                name: if app.default_account.as_deref() == Some(name.as_str()) {
                    format!("D {name}")
                } else {
                    name.clone()
                },
                color: colors[index % colors.len()],
                segments: chart_segments(&points, now),
            })
        })
        .collect();

    let range_hours = (app.range_days() * 24) as f64;
    let normal = app.fleet.policy.floors(Mode::Normal);
    let sip = app.fleet.policy.floors(Mode::Sip);
    let floors = match &app.chart_window {
        ChartWindow::Session => Some((normal.session * 100.0, sip.session * 100.0)),
        ChartWindow::Weekly => Some((normal.weekly * 100.0, sip.weekly * 100.0)),
        ChartWindow::Model { .. } | ChartWindow::Overage => None,
    };
    let normal_floor = floors.map(|(normal, _)| vec![(-range_hours, normal), (0.0, normal)]);
    let sip_floor = floors.map(|(_, sip)| vec![(-range_hours, sip), (0.0, sip)]);

    // Only the first run of each account carries its name into the legend.
    let mut datasets: Vec<Dataset> = entries
        .iter()
        .flat_map(|entry| {
            entry.segments.iter().enumerate().map(move |(i, run)| {
                Dataset::default()
                    .name(if i == 0 { entry.name.as_str() } else { "" })
                    .marker(symbols::Marker::Braille)
                    .style(Style::default().fg(entry.color))
                    .graph_type(GraphType::Line)
                    .data(run)
            })
        })
        .collect();
    if let Some(data) = &normal_floor {
        datasets.push(
            Dataset::default()
                .name(format!("normal {:.0}%", data[0].1))
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Yellow))
                .graph_type(GraphType::Line)
                .data(data),
        );
    }
    if let Some(data) = &sip_floor {
        datasets.push(
            Dataset::default()
                .name(format!("sip {:.0}%", data[0].1))
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::LightRed))
                .graph_type(GraphType::Line)
                .data(data),
        );
    }

    let x_labels = vec![
        Span::raw(format!("-{}", app.range_label())),
        Span::raw(format!("-{}", format_duration_hours(range_hours / 2.0))),
        Span::raw("now"),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(format!(
                    " {} remaining ({}) · {} mode · {} ",
                    app.chart_window.label(),
                    app.range_label(),
                    app.mode.label(),
                    app.chart_window.signal_label(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Bucket, ProbeError, Verdict};

    fn reading() -> Reading {
        Reading {
            account: "a".into(),
            at: 0,
            verdict: Verdict::Allowed,
            claim: None,
            session: Some(Window::new(0.2, 0)),
            weekly: Some(Window::new(0.4, 0)),
            buckets: vec![Bucket {
                key: ModelKey::new("fable"),
                label: "Fable".into(),
                window: Window::new(0.1, 0),
                source: Source::Observed,
            }],
            overage: None,
            error: None,
            profile_error: None,
        }
    }

    #[test]
    fn card_rows_count_every_line() {
        assert_eq!(card_rows(&reading(), false), 2 + 2 + 1);
        assert_eq!(card_rows(&reading(), true), 2 + 2 + 1 + 1);
        let failed = Reading::failed("a", 0, ProbeError::NoCredential);
        assert_eq!(card_rows(&failed, false), 2 + 1 + 1);
    }

    #[test]
    fn the_selected_card_stays_visible() {
        let heights = [5, 5, 5, 5];
        assert_eq!(visible_range(&heights, 0, 12), (0, 2));
        assert_eq!(visible_range(&heights, 3, 12), (2, 4));
        assert_eq!(visible_range(&heights, 1, 3), (1, 2));
        assert_eq!(visible_range(&[], 0, 10), (0, 0));
    }

    #[test]
    fn observed_buckets_are_marked() {
        assert_eq!(compact_bucket_label("Opus 4.8", true), "O48!");
        assert_eq!(compact_bucket_label("Fable", false), "Fab");
    }

    #[test]
    fn chart_segments_count_hours_back_from_now() {
        let points = [
            Point {
                at: 3600,
                remaining: 0.5,
                resets_at: None,
            },
            Point {
                at: 3960,
                remaining: 0.25,
                resets_at: None,
            },
        ];
        assert_eq!(
            chart_segments(&points, 7200),
            [vec![(-1.0, 50.0), (-0.9, 25.0)]]
        );
    }
}
