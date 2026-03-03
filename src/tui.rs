use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::config::Config;
use crate::probe::{self, ProbeResult, Window};
use crate::store::Store;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

struct App {
    results: Vec<ProbeResult>,
    selected: usize,
    last_probe: Instant,
    store: Store,
    config: Config,
    status_msg: String,
}

impl App {
    fn new(config: Config, store: Store) -> Self {
        Self {
            results: Vec::new(),
            selected: 0,
            last_probe: Instant::now() - REFRESH_INTERVAL,
            store,
            config,
            status_msg: "Starting...".into(),
        }
    }

    async fn probe(&mut self) {
        self.status_msg = "Probing...".into();
        let results = probe::probe_all(&self.config.tokens).await;
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
    }

    fn next_probe_in(&self) -> Duration {
        let elapsed = self.last_probe.elapsed();
        if elapsed >= REFRESH_INTERVAL {
            Duration::ZERO
        } else {
            REFRESH_INTERVAL - elapsed
        }
    }
}

pub async fn run(config: Config) -> Result<()> {
    let store = Store::open()?;
    let mut app = App::new(config, store);

    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    loop {
        if app.last_probe.elapsed() >= REFRESH_INTERVAL {
            app.probe().await;
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
                    app.probe().await;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected > 0 {
                        app.selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !app.results.is_empty() && app.selected < app.results.len() - 1 {
                        app.selected += 1;
                    }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
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
    let header_text = format!(
        " tokeman  |  {}  |  next probe in {}s",
        app.status_msg,
        countdown.as_secs()
    );
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    // Token list
    if app.results.is_empty() {
        let empty = Paragraph::new("  No results yet. Waiting for first probe...")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, chunks[1]);
    } else {
        draw_tokens(f, chunks[1], &app.results, app.selected);
    }

    // Footer
    let footer = Paragraph::new(" q: quit  r: refresh  j/k: navigate")
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

fn draw_tokens(f: &mut Frame, area: Rect, results: &[ProbeResult], selected: usize) {
    // Each token takes up to 5 lines: name, 5h, 7d, overage, blank
    let constraints: Vec<Constraint> = results
        .iter()
        .map(|r| {
            let lines = 2 + r.quota.as_ref().map_or(1, |q| {
                q.session.is_some() as u16 + q.weekly.is_some() as u16 + q.overage.is_some() as u16
            });
            Constraint::Length(lines)
        })
        .collect();

    let token_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, result) in results.iter().enumerate() {
        if i >= token_chunks.len() {
            break;
        }
        draw_single_token(f, token_chunks[i], result, i == selected);
    }
}

fn draw_single_token(f: &mut Frame, area: Rect, result: &ProbeResult, selected: bool) {
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
    row_constraints.push(Constraint::Length(1)); // spacer

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    // Name line
    let marker = if selected { ">" } else { " " };
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

    let claim_str = result.quota.as_ref().map(|q| match q.representative_claim.as_str() {
        "five_hour" => " session",
        "seven_day" => " weekly",
        "seven_day_opus" => " Opus",
        "seven_day_sonnet" => " Sonnet",
        "overage" => " extra",
        _ => "",
    }).unwrap_or("");

    let name_line = Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
        Span::styled(&result.token_name, Style::default().add_modifier(Modifier::BOLD)),
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
            let _ = row_idx;
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
    }
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
        Span::styled("\u{2591}".repeat(empty), Style::default().fg(Color::DarkGray)),
        Span::raw(format!(" {:>3}% left", pct)),
        Span::styled(format!("  resets {reset}"), Style::default().fg(Color::DarkGray)),
    ])
}

use crate::display::format_reset_compact;
