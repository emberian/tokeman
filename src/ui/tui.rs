//! `tokeman --watch`: a ratatui dashboard of the fleet view. A card per
//! account of every service, and a chart of any limit's history over six
//! ranges.
//!
//! While the background service runs this is a viewer: it re-reads the
//! stored view every few seconds and sends no request. Without the service
//! (or started with `--fresh`) it reads every account on the configured
//! interval through `commands::view`. `r` reads now either way.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
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
use crate::claude::sessions::{self, Session};
use crate::commands::view;
use crate::daemon;
use crate::policy::Mode;
use crate::quota::{Level, Limit, Reading, Service};
use crate::store::{Series, Store};
use crate::ui::chart::{self, Point, format_duration_hours, points, segments};
use crate::ui::overview::{AccountView, Origin, Overview};
use crate::ui::text::{
    format_age, format_reset_compact, limit_name, limits_in_order, truncate_chars, view_caption,
};

/// Chart ranges in days, cycled with `[` / `]`.
const RANGE_DAYS: [i64; 6] = [1, 3, 5, 7, 14, 30];
/// Points further apart than this were not read back to back.
const GAP_SECS: i64 = 600;
/// How often the stored view is read again while the service keeps it fresh.
const VIEW_INTERVAL: Duration = Duration::from_secs(5);
/// How often live Claude processes are counted and the history re-read.
const SLOW_INTERVAL: Duration = Duration::from_secs(30);

/// What a finished load hands the screen.
struct Loaded {
    overview: Overview,
    /// `Some` when a census was taken with this load; the inner `None` is a
    /// census that failed.
    sessions: Option<Option<Vec<Session>>>,
}

struct App {
    fleet: Fleet,
    /// Read every account on the interval even while the service runs.
    always_fresh: bool,
    overview: Option<Overview>,
    selected: usize,
    loaded_at: Option<Instant>,
    loading: bool,
    /// `None` until the first census, or when the census failed.
    live_sessions: Option<Vec<Session>>,
    census_at: Option<Instant>,
    show_chart: bool,
    chart_fullscreen: bool,
    /// The limit the chart follows; `None` is the first the history holds.
    chart_series: Option<Series>,
    /// Index into `RANGE_DAYS`.
    chart_range: usize,
    /// The selected account's service, read back `range_days`.
    history: Vec<Reading>,
    history_of: Option<(Service, usize)>,
    history_at: Option<Instant>,
}

impl App {
    fn new(fleet: Fleet, always_fresh: bool) -> Self {
        Self {
            fleet,
            always_fresh,
            overview: None,
            selected: 0,
            loaded_at: None,
            loading: false,
            live_sessions: None,
            census_at: None,
            show_chart: false,
            chart_fullscreen: false,
            chart_series: None,
            chart_range: 0,
            history: Vec::new(),
            history_of: None,
            history_at: None,
        }
    }

    /// Whether the background service is the reader and this only shows.
    fn is_viewer(&self) -> bool {
        !self.always_fresh
            && self
                .overview
                .as_ref()
                .is_none_or(|overview| overview.monitor.keeps_fresh())
    }

    fn interval(&self) -> Duration {
        if self.is_viewer() {
            VIEW_INTERVAL
        } else {
            Duration::from_secs(self.fleet.launch.probe_interval_secs.max(1))
        }
    }

    fn next_load_in(&self) -> Duration {
        match self.loaded_at {
            Some(at) => self.interval().saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        }
    }

    /// Build the view in the background: the stored one, or a read of every
    /// account when nothing else reads them or `force` asks.
    fn spawn_load(&mut self, force: bool) -> JoinHandle<Loaded> {
        self.loading = true;
        let fleet = self.fleet.clone();
        let force = force || self.always_fresh;
        let census = self
            .census_at
            .is_none_or(|at| at.elapsed() >= SLOW_INTERVAL);
        tokio::spawn(async move {
            let overview = view::overview(&fleet, force).await;
            let sessions = match census {
                true => Some(sessions::live().await.ok()),
                false => None,
            };
            Loaded { overview, sessions }
        })
    }

    fn apply(&mut self, loaded: Loaded) {
        if let Some(sessions) = loaded.sessions {
            self.live_sessions = sessions;
            self.census_at = Some(Instant::now());
        }
        self.overview = Some(loaded.overview);
        self.loading = false;
        self.loaded_at = Some(Instant::now());
        let count = self.accounts().len();
        if self.selected >= count {
            self.selected = count.saturating_sub(1);
        }
    }

    fn accounts(&self) -> &[AccountView] {
        self.overview
            .as_ref()
            .map_or(&[], |overview| overview.accounts.as_slice())
    }

    fn selected_account(&self) -> Option<&AccountView> {
        self.accounts().get(self.selected)
    }

    /// Read the selected account's service back over the chart range, when
    /// the selection or the range changed, or the last read is old.
    fn load_history(&mut self, now: bool) {
        let Some(service) = self.selected_account().map(|account| account.service) else {
            return;
        };
        let wanted = Some((service, self.chart_range));
        let stale = self
            .history_at
            .is_none_or(|at| at.elapsed() >= SLOW_INTERVAL);
        if !now && !stale && self.history_of == wanted {
            return;
        }
        let since = Utc::now().timestamp() - self.range_days() * 86_400;
        self.history = Store::open_read_only()
            .and_then(|store| store.since(service, None, since))
            .unwrap_or_default();
        if self.history_of.map(|(of, _)| of) != Some(service) {
            self.chart_series = None;
        }
        self.history_of = wanted;
        self.history_at = Some(Instant::now());
    }

    fn range_days(&self) -> i64 {
        RANGE_DAYS[self.chart_range]
    }

    fn range_label(&self) -> String {
        format!("{}d", self.range_days())
    }

    /// The limit the chart follows now.
    fn series(&self) -> Option<Series> {
        self.chart_series
            .clone()
            .or_else(|| chart::resolve(None, &self.history).ok())
    }

    fn series_label(&self) -> String {
        self.series().map_or_else(
            || "no history".to_owned(),
            |series| chart::series_name(&series, &self.history),
        )
    }

    /// Step to the next limit the history holds.
    fn cycle_series(&mut self) {
        let known: Vec<Series> = Series::known(&self.history)
            .into_iter()
            .map(|(series, _)| series)
            .collect();
        if known.is_empty() {
            return;
        }
        let current = self
            .series()
            .and_then(|series| known.iter().position(|other| *other == series))
            .unwrap_or(known.len() - 1);
        self.chart_series = Some(known[(current + 1) % known.len()].clone());
    }
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

pub async fn run(fleet: Fleet, always_fresh: bool) -> Result<()> {
    let mut app = App::new(fleet, always_fresh);
    let started_as = daemon::executable_identity();

    let terminal_session = TerminalSession::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut active: Option<JoinHandle<Loaded>> = None;
    let mut executable_replaced = false;

    loop {
        if started_as.is_some()
            && daemon::executable_identity().is_some_and(|current| Some(current) != started_as)
        {
            executable_replaced = true;
            break;
        }
        if let Some(load) = active.take_if(|load| load.is_finished()) {
            app.apply(load.await?);
            app.load_history(false);
        }
        if active.is_none() && app.next_load_in().is_zero() {
            active = Some(app.spawn_load(false));
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
                    if let Some(load) = active.take() {
                        load.abort();
                    }
                    active = Some(app.spawn_load(true));
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
                KeyCode::Tab => app.cycle_series(),
                KeyCode::Char('[') => {
                    app.chart_range = (app.chart_range + RANGE_DAYS.len() - 1) % RANGE_DAYS.len();
                    app.load_history(true);
                }
                KeyCode::Char(']') => {
                    app.chart_range = (app.chart_range + 1) % RANGE_DAYS.len();
                    app.load_history(true);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.selected = app.selected.saturating_sub(1);
                    app.load_history(false);
                }
                KeyCode::Down | KeyCode::Char('j') if app.selected + 1 < app.accounts().len() => {
                    app.selected += 1;
                    app.load_history(false);
                }
                _ => {}
            }
        }
    }

    if let Some(load) = active {
        load.abort();
    }
    drop(terminal);
    drop(terminal_session);
    if executable_replaced {
        super::text::notice("tokeman was upgraded while --watch was open; rerun tokeman --watch");
    }
    Ok(())
}

/// The two header lines: where the view comes from and how old it is, then
/// what Claude Code holds and the rotation's floors.
fn header_text(app: &App) -> String {
    let Some(overview) = &app.overview else {
        return " Tokeman  |  reading...".to_owned();
    };
    let next = if app.loading {
        "reading...".to_owned()
    } else if app.is_viewer() {
        "r: read now".to_owned()
    } else {
        format!("next read {}s", app.next_load_in().as_secs())
    };
    format!(
        " Tokeman  |  {}  |  {next}\n default(new): {} / {}  |  live pins (~=estimate): {}  |  {} floors: {:.0}% 5h / {:.0}% 7d",
        view_caption(overview),
        overview.default_account.as_deref().unwrap_or("/login"),
        overview.target_model.as_deref().unwrap_or("model?"),
        sessions::summary(app.live_sessions.as_deref()),
        overview.rotation.mode.label(),
        overview.rotation.min_session_remaining * 100.0,
        overview.rotation.min_weekly_remaining * 100.0,
    )
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

    let stale = app.overview.as_ref().is_some_and(|overview| {
        overview.origin == Origin::Stored
            && overview
                .read_at
                .is_none_or(|at| overview.at - at > crate::ui::text::STALE_SECS)
    });
    let header = Paragraph::new(header_text(app))
        .style(
            Style::default()
                .fg(if stale { Color::Yellow } else { Color::Cyan })
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    if app.accounts().is_empty() {
        let text = if app.overview.is_some() {
            "  No accounts configured."
        } else {
            "  Reading the fleet view..."
        };
        let empty = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
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
            " q: quit  r: read now  c: split  C: full-screen  Tab: limit [{}]  [/]: range [{}]",
            app.series_label(),
            app.range_label()
        )
    } else {
        " q: quit  r: read now  j/k: navigate  c/C: charts  |  * = the account its tool holds"
            .to_owned()
    };
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

/// Lines in a card: the name, a gauge per limit (or one line saying why
/// there is none), an optional sparkline, a spacer.
fn card_rows(account: &AccountView, show_spark: bool) -> u16 {
    let limits = account
        .reading
        .as_ref()
        .map_or(0, |reading| reading.limits.len());
    2 + limits.max(1) as u16 + u16::from(show_spark)
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

/// The selected account's points of the charted limit, when there are
/// enough to draw.
fn spark_points(app: &App, index: usize, account: &AccountView) -> Option<Vec<u64>> {
    if index != app.selected {
        return None;
    }
    let series = app.series()?;
    let data: Vec<u64> = app
        .history
        .iter()
        .filter(|reading| reading.account == account.name)
        .filter_map(|reading| series.window(reading))
        .map(|window| (window.remaining() * 100.0).round().clamp(0.0, 100.0) as u64)
        .collect();
    (data.len() >= 2).then_some(data)
}

fn draw_cards(f: &mut Frame, area: Rect, app: &App) {
    let accounts = app.accounts();
    let sparks: Vec<Option<Vec<u64>>> = accounts
        .iter()
        .enumerate()
        .map(|(index, account)| spark_points(app, index, account))
        .collect();
    let heights: Vec<u16> = accounts
        .iter()
        .zip(&sparks)
        .map(|(account, spark)| card_rows(account, spark.is_some()))
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
    let aged_at = app
        .overview
        .as_ref()
        .filter(|overview| overview.origin == Origin::Stored)
        .map(|overview| overview.at);

    for (visible, account) in accounts[start..end].iter().enumerate() {
        let index = start + visible;
        let lines = card_lines(account, index == app.selected, aged_at);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Length(1); heights[index] as usize])
            .split(card_areas[visible]);
        for (row, line) in lines.into_iter().enumerate() {
            f.render_widget(Paragraph::new(line), rows[row]);
        }
        if let Some(data) = &sparks[index] {
            let row = rows[heights[index] as usize - 2];
            let label = format!("   {} ", truncate_chars(&app.series_label(), 20));
            let label_width = label.chars().count() as u16;
            f.render_widget(
                Paragraph::new(Span::styled(label, Style::default().fg(Color::DarkGray))),
                Rect {
                    width: label_width.min(row.width),
                    ..row
                },
            );
            let sparkline = Sparkline::default()
                .data(data)
                .style(Style::default().fg(Color::Cyan));
            f.render_widget(
                sparkline,
                Rect {
                    x: row.x + label_width.min(row.width),
                    width: row.width.saturating_sub(label_width + 2),
                    ..row
                },
            );
        }
    }
}

/// One account's card, without its sparkline: the name line, then a gauge
/// per limit, or one line saying why there is none.
fn card_lines(account: &AccountView, selected: bool, aged_at: Option<i64>) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let marker = match (selected, account.held) {
        (true, true) => ">*",
        (true, false) => "> ",
        (false, true) => " *",
        (false, false) => "  ",
    };
    let mut name = vec![
        Span::styled(
            format!("{marker} "),
            Style::default().fg(if account.held {
                Color::LightGreen
            } else {
                Color::Cyan
            }),
        ),
        Span::styled(
            account.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", account.service_label), dim),
    ];
    if let Some(state) = &account.state {
        let color = state
            .level
            .map_or(Color::DarkGray, |level| super::level_color(level, LEVELS));
        name.push(Span::styled(
            format!("  {}", truncate_chars(&state.word, 40)),
            Style::default().fg(color),
        ));
    }
    if account.parked {
        name.push(Span::styled("  parked", dim));
    }
    if account.service == Service::Claude && account.viable == Some(false) {
        name.push(Span::styled(
            "  below floor",
            Style::default().fg(Color::Yellow),
        ));
    }
    let warning = account
        .notes
        .iter()
        .find(|note| note.starts_with('!'))
        .or(account
            .login
            .as_ref()
            .filter(|login| login.warning)
            .and_then(|login| login.note.as_ref()));
    if let Some(warning) = warning {
        name.push(Span::styled(
            format!("  {}", truncate_chars(warning, 60)),
            Style::default().fg(Color::Yellow),
        ));
    }
    let reading = account.reading.as_ref();
    if let (Some(reading), Some(now)) = (reading, aged_at) {
        name.push(Span::styled(
            format!("  {}", format_age(reading.at, now)),
            dim,
        ));
    }
    let mut lines = vec![Line::from(name)];
    let limits = reading.map(limits_in_order).unwrap_or_default();
    let width = limits
        .iter()
        .map(|limit| limit_name(limit).chars().count())
        .max()
        .unwrap_or(0);
    lines.extend(limits.iter().map(|limit| gauge_line(limit, width)));
    if limits.is_empty() {
        let (text, color) = match (&account.error, reading) {
            (Some(error), _) => (format!("error: {}", truncate_chars(error, 100)), Color::Red),
            (None, Some(_)) => (
                account
                    .notes
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "no limits reported".to_owned()),
                Color::DarkGray,
            ),
            (None, None) => ("not read yet".to_owned(), Color::DarkGray),
        };
        lines.push(Line::from(Span::styled(
            format!("   {text}"),
            Style::default().fg(color),
        )));
    }
    lines
}

/// The terminal palette for [`super::level_color`].
const LEVELS: [Color; 3] = [Color::Green, Color::Yellow, Color::Red];

/// One limit as a gauge: its name padded to `width`, a bar of what is left,
/// the percentage and the reset.
fn gauge_line(limit: &Limit, width: usize) -> Line<'static> {
    let remaining = limit.window.remaining().clamp(0.0, 1.0);
    let bar_width = 30usize;
    let filled = ((remaining * bar_width as f64).round() as usize).min(bar_width);
    let empty = bar_width - filled;
    let pct = (remaining * 100.0).round() as u8;
    let color = super::level_color(Level::of(remaining), LEVELS);
    let reset = format_reset_compact(limit.window.resets_at);
    let mark = if limit.is_rejected() || limit.source == crate::quota::Source::Observed {
        "!"
    } else {
        " "
    };

    Line::from(vec![
        Span::raw(format!("  {mark}{:<width$} ", limit_name(limit))),
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
    let (Some(service), Some(series)) = (app.history_of.map(|(service, _)| service), app.series())
    else {
        let empty = Paragraph::new("  No history in this range.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(empty, area);
        return;
    };
    let mut by_account: BTreeMap<&str, Vec<Reading>> = BTreeMap::new();
    for reading in &app.history {
        by_account
            .entry(reading.account.as_str())
            .or_default()
            .push(reading.clone());
    }
    let held: Vec<&str> = app
        .accounts()
        .iter()
        .filter(|account| account.service == service && account.held)
        .map(|account| account.name.as_str())
        .collect();
    let entries: Vec<ChartEntry> = by_account
        .iter()
        .enumerate()
        .filter_map(|(index, (name, readings))| {
            let points = points(readings, &series);
            if points.is_empty() {
                return None;
            }
            Some(ChartEntry {
                name: if held.contains(name) {
                    format!("* {name}")
                } else {
                    (*name).to_owned()
                },
                color: colors[index % colors.len()],
                segments: chart_segments(&points, now),
            })
        })
        .collect();

    let range_hours = (app.range_days() * 24) as f64;
    let floors = chart::floors(service, &series, &app.fleet.policy)
        .map(|(normal, sip)| (normal * 100.0, sip * 100.0));
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

    let mode = app
        .overview
        .as_ref()
        .map_or(Mode::Normal, |overview| overview.rotation.mode);
    let x_labels = vec![
        Span::raw(format!("-{}", app.range_label())),
        Span::raw(format!("-{}", format_duration_hours(range_hours / 2.0))),
        Span::raw("now"),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(format!(
                    " {} {} remaining ({}) · {} mode · {} ",
                    service.label(),
                    chart::series_name(&series, &app.history),
                    app.range_label(),
                    mode.label(),
                    chart::signal_label(service, &series),
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
    use crate::quota::{ReadError, Source, Window};
    use crate::testing::{model_limit, point};
    use crate::ui::overview::{Facts, Monitor};

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn reading() -> Reading {
        let mut r = crate::testing::reading("a", 0.2, 0.4);
        r.limits
            .push(model_limit("fable", "Fable", 0.1, Source::Observed));
        r
    }

    fn view(reading: Option<&Reading>) -> AccountView {
        let service = reading.map_or(Service::Kimi, |reading| reading.service);
        AccountView::of(service, "a", reading)
    }

    #[test]
    fn a_card_has_a_gauge_per_limit_whatever_the_service() {
        let claude = view(Some(&reading()));
        assert_eq!(card_rows(&claude, false), 2 + 3);
        assert_eq!(card_rows(&claude, true), 2 + 3 + 1);
        let lines: Vec<String> = card_lines(&claude, true, Some(reading().at + 300))
            .iter()
            .map(text)
            .collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with(">  a  Claude  allowed"), "{lines:?}");
        assert!(lines[0].ends_with("5m ago"), "{lines:?}");
        assert!(lines[1].starts_with("   5h "), "{lines:?}");
        assert!(lines[1].contains(" 80% left"), "{lines:?}");
        assert!(lines[2].starts_with("   weekly "), "{lines:?}");
        assert!(
            lines[3].starts_with("  !Fable weekly "),
            "an observed rejection is marked: {lines:?}"
        );

        let kimi = Reading {
            limits: vec![crate::providers::limit(
                Some("Tools".into()),
                "monthly",
                None,
                Window::new(0.9, 0),
                None,
            )],
            ..Reading::empty(Service::Kimi, "a", 0)
        };
        let mut held = view(Some(&kimi));
        held.held = true;
        let lines = card_lines(&held, false, None);
        assert_eq!(text(&lines[0]), " * a  Kimi Code");
        assert!(
            text(&lines[1]).starts_with("   Tools monthly "),
            "{lines:?}"
        );
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn a_card_without_limits_says_why() {
        let failed = Reading::failed(Service::Claude, "a", 0, ReadError::NoCredential);
        let account = view(Some(&failed));
        assert_eq!(card_rows(&account, false), 2 + 1);
        let lines = card_lines(&account, false, None);
        assert_eq!(text(&lines[1]), "   error: no usable credential");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
        let unread = card_lines(&view(None), false, None);
        assert_eq!(text(&unread[1]), "   not read yet");
        let empty = card_lines(
            &view(Some(&Reading::empty(Service::Zai, "a", 0))),
            false,
            None,
        );
        assert_eq!(text(&empty[1]), "   no limits reported");
    }

    #[test]
    fn the_header_says_stored_or_read_and_only_a_reader_counts_down() {
        let fleet = Fleet {
            accounts: vec![crate::testing::setup_account("a")],
            ..Fleet::default()
        };
        let mut app = App::new(fleet.clone(), false);
        assert_eq!(header_text(&app), " Tokeman  |  reading...");
        assert!(
            app.next_load_in().is_zero(),
            "the first load is due at once"
        );
        let facts = |monitor| Facts {
            monitor,
            readings: vec![reading()],
            ..Facts::default()
        };
        let at = reading().at + 30;
        app.apply(Loaded {
            overview: Overview::build(&fleet, facts(Monitor::Running), None, at),
            sessions: Some(Some(Vec::new())),
        });
        assert!(app.is_viewer());
        assert_eq!(app.interval(), VIEW_INTERVAL);
        let header = header_text(&app);
        assert!(
            header.contains("stored view, last read just now by the background service"),
            "{header}"
        );
        assert!(header.contains("r: read now"), "{header}");
        assert!(
            header.contains("live pins (~=estimate): none detected"),
            "{header}"
        );

        // Without the service it reads on the configured interval.
        app.apply(Loaded {
            overview: Overview::build(
                &fleet,
                facts(Monitor::NotInstalled).with_fresh(vec![reading()]),
                None,
                at,
            ),
            sessions: None,
        });
        assert!(!app.is_viewer());
        assert_eq!(
            app.interval(),
            Duration::from_secs(fleet.launch.probe_interval_secs)
        );
        let header = header_text(&app);
        assert!(
            header.contains("read now, 1/1 accounts answered"),
            "{header}"
        );
        assert!(header.contains("next read "), "{header}");
        assert!(
            header.contains("none detected"),
            "a load without a census keeps the last one: {header}"
        );
        // `--fresh` reads even while the service runs.
        let mut forced = App::new(fleet.clone(), true);
        forced.apply(Loaded {
            overview: Overview::build(&fleet, facts(Monitor::Running), None, at),
            sessions: None,
        });
        assert!(!forced.is_viewer());
    }

    #[test]
    fn tab_steps_through_the_limits_the_history_holds() {
        let mut app = App::new(Fleet::default(), false);
        app.history = vec![reading()];
        assert_eq!(app.series(), Some(Series::session()));
        assert_eq!(app.series_label(), "5h");
        app.cycle_series();
        assert_eq!(app.series(), Some(Series::weekly()));
        app.cycle_series();
        assert_eq!(app.series_label(), "Fable weekly");
        app.cycle_series();
        assert_eq!(app.series(), Some(Series::session()));
        app.history.clear();
        app.chart_series = None;
        app.cycle_series();
        assert_eq!(app.series(), None);
        assert_eq!(app.series_label(), "no history");
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
    fn chart_segments_count_hours_back_from_now() {
        let points = [point(3600, 0.5, None), point(3960, 0.25, None)];
        assert_eq!(
            chart_segments(&points, 7200),
            [vec![(-1.0, 50.0), (-0.9, 25.0)]]
        );
    }
}
