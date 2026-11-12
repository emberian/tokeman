//! History charts: a PNG (inline in iTerm2), and the series, segment and
//! axis logic the TUI chart draws with.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{Duration, Utc};

use crate::account::Fleet;
use crate::anthropic::probe::PROBE_MODEL_LABEL;
use crate::paths;
use crate::policy::Mode;
use crate::quota::{Limit, ModelKey, Reading, Scope, Service, Span};
use crate::settings::PolicySettings;
use crate::store::{self, Series, Store};
use crate::ui::text::limit_name;

const COLORS: [[u8; 4]; 10] = [
    [91, 192, 235, 255],
    [253, 184, 51, 255],
    [155, 93, 229, 255],
    [0, 245, 212, 255],
    [255, 107, 107, 255],
    [127, 255, 127, 255],
    [255, 132, 219, 255],
    [255, 159, 67, 255],
    [72, 219, 251, 255],
    [200, 214, 229, 255],
];

/// `tokeman chart`, as the command line spells it.
pub struct Request {
    pub service: Service,
    pub hours: f64,
    /// Which limit to draw; `None` is the service's first account-wide one.
    pub metric: Option<String>,
    pub output: Option<PathBuf>,
    pub iterm: bool,
    pub width: u32,
    pub height: u32,
}

/// A window length by name: `5h`, `five-hour`, `weekly`, `7d`, `seven-day`,
/// `monthly`, `daily`.
fn span_named(word: &str) -> Option<Span> {
    Some(match word.to_ascii_lowercase().as_str() {
        "5h" | "five-hour" => Span::FiveHour,
        "7d" | "seven-day" | "weekly" => Span::Weekly,
        "monthly" => Span::Monthly,
        "daily" => Span::Other(Span::DAY_SECS),
        _ => return None,
    })
}

/// The series `--metric` names, among the limits `readings` hold.
///
/// A length alone (`5h`, `weekly`, `monthly`, `daily`) is the account-wide
/// window of that length; `overage` is Claude's extra usage; anything else
/// names a scoped limit by its model, group or label (`fable`, `"Opus 4.8"`,
/// `gemini`, `spark`), optionally followed by a length (`"gemini 5h"`). With
/// no metric: the shortest account-wide window in the history, whichever
/// account was read last; with none, the first limit of any kind.
pub fn resolve(metric: Option<&str>, readings: &[Reading]) -> Result<Series> {
    let known = Series::known(readings);
    let names = || {
        let names: Vec<String> = known.iter().map(|(_, limit)| limit_name(limit)).collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    };
    let Some(metric) = metric.map(str::trim) else {
        return known
            .iter()
            .filter(|(series, _)| series.scope == Scope::All)
            .min_by_key(|(series, _)| series.span.seconds().unwrap_or(i64::MAX))
            .or(known.first())
            .map(|(series, _)| series.clone())
            .context("no chartable history in this range; run `tokeman --fresh` to read once");
    };
    if let Some(span) = span_named(metric) {
        return Ok(Series::new(span, Scope::All));
    }
    if metric == "overage" {
        return Ok(Series::new(Span::Unknown, Scope::extra_usage()));
    }
    let (needle, span) = match metric.rsplit_once(char::is_whitespace) {
        Some((needle, last)) if span_named(last).is_some() => (needle.trim(), span_named(last)),
        _ => (metric, None),
    };
    let key = ModelKey::new(needle);
    if key.is_empty() {
        bail!("unknown metric {metric:?}; recorded limits: {}", names());
    }
    let scoped = |matches: &dyn Fn(&Limit) -> bool| {
        known
            .iter()
            .filter(|(series, _)| span.is_none_or(|span| series.span == span))
            .find(|(_, limit)| matches(limit))
            .map(|(series, _)| series.clone())
    };
    scoped(&|limit| limit.key() == Some(&key))
        .or_else(|| scoped(&|limit| limit.matches(needle)))
        .with_context(|| {
            format!(
                "no history for a limit named {metric:?} in this range; recorded limits: {}",
                names()
            )
        })
}

/// The (normal, sip) floors a series is judged by, as fractions remaining:
/// the rotation's, for Claude's account-wide 5-hour and weekly windows.
pub fn floors(service: Service, series: &Series, policy: &PolicySettings) -> Option<(f64, f64)> {
    if service != Service::Claude || series.scope != Scope::All {
        return None;
    }
    let normal = policy.floors(Mode::Normal);
    let sip = policy.floors(Mode::Sip);
    match series.span {
        Span::FiveHour => Some((normal.session, sip.session)),
        Span::Weekly => Some((normal.weekly, sip.weekly)),
        _ => None,
    }
}

/// What a chart of `series` measures, for its caption.
pub fn signal_label(service: Service, series: &Series) -> String {
    match (service, &series.scope) {
        (Service::Claude, Scope::Model(_)) => {
            "PER-MODEL WEEKLY LIMIT · COLD-REQUEST ADMISSION CAN BE STRICTER".into()
        }
        (Service::Claude, _) => format!(
            "{} QUOTA SIGNAL · PREMIUM ADMISSION UNMEASURED",
            PROBE_MODEL_LABEL.to_uppercase()
        ),
        (service, _) => format!("{} USAGE ENDPOINT", service.label().to_uppercase()),
    }
}

/// The name of `series`, as the newest reading that has it names its limit.
pub fn series_name(series: &Series, readings: &[Reading]) -> String {
    readings
        .iter()
        .rev()
        .find_map(|reading| series.limit(reading))
        .map_or_else(|| series.span.label(), limit_name)
}

/// One reading of one series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// Epoch seconds.
    pub at: i64,
    /// Fraction of capacity left, clamped to 0..1.
    pub remaining: f64,
    /// `None` when the server sent no reset.
    pub resets_at: Option<i64>,
}

/// The points of `series` in `snapshots`, in their order, skipping readings
/// without the window or with a non-finite value.
pub fn points(snapshots: &[Reading], series: &Series) -> Vec<Point> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let window = series.window(snapshot)?;
            window.used.is_finite().then(|| Point {
                at: snapshot.at,
                remaining: window.remaining().clamp(0.0, 1.0),
                resets_at: (window.resets_at != 0).then_some(window.resets_at),
            })
        })
        .collect()
}

/// Whether a line from `prev` to `cur` would mislead: a time gap, a reset
/// change (both known), or capacity rising by more than 2.5 points.
pub fn breaks_segment(prev: &Point, cur: &Point, gap_secs: i64) -> bool {
    let reset_changed =
        prev.resets_at.is_some() && cur.resets_at.is_some() && prev.resets_at != cur.resets_at;
    cur.at - prev.at > gap_secs || reset_changed || cur.remaining > prev.remaining + 0.025
}

/// The longest a value is held without a row, with room for one late read.
const HELD_SECS: i64 = store::HOLD_SECS + 300;

/// Runs of points a line may join. Points further apart than `gap_secs` were
/// not read back to back: up to the hold the store keeps (or two and a half
/// of the series' usual intervals, for a service read less often) the value
/// stood until the later read, and is drawn as a step; beyond that nothing
/// was reading, and the line breaks.
pub fn segments(points: &[Point], gap_secs: i64) -> Vec<Vec<Point>> {
    let mut intervals: Vec<i64> = points
        .windows(2)
        .map(|pair| pair[1].at - pair[0].at)
        .collect();
    intervals.sort_unstable();
    let usual = intervals.get(intervals.len() / 2).copied().unwrap_or(0);
    let held = gap_secs.max(HELD_SECS).max(usual * 5 / 2);
    let mut segments: Vec<Vec<Point>> = Vec::new();
    let mut previous: Option<&Point> = None;
    for point in points {
        match (previous, segments.last_mut()) {
            (Some(prev), Some(segment)) if !breaks_segment(prev, point, held) => {
                if point.at - prev.at > gap_secs && point.remaining != prev.remaining {
                    segment.push(Point {
                        at: point.at,
                        ..*prev
                    });
                }
                segment.push(*point);
            }
            _ => segments.push(vec![*point]),
        }
        previous = Some(point);
    }
    segments
}

/// `2d`, `36h`, `45m`.
pub fn format_duration_hours(hours: f64) -> String {
    if hours >= 24.0 && (hours / 24.0).fract().abs() < 0.01 {
        format!("{:.0}d", hours / 24.0)
    } else if hours >= 1.0 {
        format!("{hours:.0}h")
    } else {
        format!("{:.0}m", hours * 60.0)
    }
}

/// A chart written to disk, and the escape sequence that shows it inline
/// when the terminal is iTerm2 (or `--iterm` asked for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub output: std::path::PathBuf,
    pub inline: Option<String>,
}

pub fn run(request: &Request) -> Result<Rendered> {
    if !request.hours.is_finite() || request.hours <= 0.0 {
        bail!("--hours must be greater than zero");
    }
    if !(640..=3840).contains(&request.width) {
        bail!("--width must be between 640 and 3840");
    }
    if !(360..=2160).contains(&request.height) {
        bail!("--height must be between 360 and 2160");
    }
    let policy = Fleet::load()?.policy;

    let cutoff = Utc::now() - Duration::milliseconds((request.hours * 3_600_000.0).round() as i64);
    let readings = match Store::open_read_only() {
        Ok(store) => store.since(request.service, None, cutoff.timestamp())?,
        Err(_) => Vec::new(),
    };
    let series = resolve(request.metric.as_deref(), &readings)?;
    let png = render_png(
        &policy,
        request.service,
        &readings,
        &series,
        request.hours,
        request.width,
        request.height,
    )?;

    let output = request.output.clone().unwrap_or_else(paths::default_chart);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&output, &png)
        .with_context(|| format!("failed to write {}", output.display()))?;

    let inline = (request.iterm || (is_iterm2() && std::env::var_os("TMUX").is_none()))
        .then(|| iterm_image(&png));
    Ok(Rendered { output, inline })
}

fn is_iterm2() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|value| value == "iTerm2")
}

/// iTerm2's inline-image escape sequence for `png`.
fn iterm_image(png: &[u8]) -> String {
    let name = base64::engine::general_purpose::STANDARD.encode("tokeman-chart.png");
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "\x1b]1337;File=name={name};inline=1;width=auto;height=auto;preserveAspectRatio=1:{data}\x07"
    )
}

fn render_png(
    policy: &PolicySettings,
    service: Service,
    snapshots: &[Reading],
    series: &Series,
    hours: f64,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let mut by_account: BTreeMap<&str, Vec<Reading>> = BTreeMap::new();
    for snapshot in snapshots {
        by_account
            .entry(snapshot.account.as_str())
            .or_default()
            .push(snapshot.clone());
    }
    let lines: Vec<(&str, Vec<Point>)> = by_account
        .iter()
        .map(|(name, rows)| (*name, points(rows, series)))
        .filter(|(_, points)| !points.is_empty())
        .collect();
    if lines.is_empty() {
        let known: Vec<String> = Series::known(snapshots)
            .iter()
            .map(|(_, limit)| limit_name(limit))
            .collect();
        bail!(
            "no history for the {} limit of {} in this range; recorded limits: {}",
            series_name(series, snapshots),
            service.label(),
            if known.is_empty() {
                "none (run `tokeman --fresh` to read once)".into()
            } else {
                known.join(", ")
            }
        );
    }

    let mut canvas = Canvas::new(width, height, [12, 17, 27, 255]);
    let legend_width = if width >= 1000 { 310 } else { 205 };
    let left = 72;
    let top = 74;
    let right = width.saturating_sub(legend_width);
    let bottom = height.saturating_sub(62);
    if right <= left + 100 || bottom <= top + 100 {
        bail!("chart dimensions are too small");
    }

    let title = format!(
        "{} {} REMAINING",
        crate::commands::services::command_word(service),
        series_name(series, snapshots)
    );
    canvas.text(24, 22, &title, 3, [229, 235, 244, 255]);
    canvas.text(
        24,
        52,
        &format!(
            "{} · LAST {}",
            signal_label(service, series),
            format_duration_hours(hours).to_uppercase()
        ),
        1,
        [125, 145, 170, 255],
    );

    for percent in [0, 25, 50, 75, 100] {
        let y = map_y(percent as f64 / 100.0, top, bottom);
        canvas.line(left, y, right, y, [45, 56, 72, 255]);
        canvas.text(
            17,
            y.saturating_sub(4),
            &format!("{percent:>3}%"),
            1,
            [125, 145, 170, 255],
        );
    }
    canvas.line(left, top, left, bottom, [91, 105, 125, 255]);
    canvas.line(left, bottom, right, bottom, [91, 105, 125, 255]);

    let now = Utc::now().timestamp();
    let start = now - (hours * 3600.0).round() as i64;
    for quarter in 0..=4 {
        let x = left + ((right - left) * quarter / 4);
        canvas.line(x, top, x, bottom, [31, 41, 55, 255]);
        let remaining_hours = hours * (4 - quarter) as f64 / 4.0;
        let label = if quarter == 4 {
            "NOW".into()
        } else {
            format!("-{}", format_duration_hours(remaining_hours))
        };
        canvas.text(
            x.saturating_sub((label.len() as u32 * 3).min(38)),
            bottom + 16,
            &label,
            1,
            [125, 145, 170, 255],
        );
    }

    if let Some((normal_floor, sip_floor)) = floors(service, series, policy) {
        if normal_floor > 0.0 {
            let y = map_y(normal_floor, top, bottom);
            canvas.dashed_line(left, y, right, 8, [253, 184, 51, 210]);
            canvas.text(
                left + 8,
                y.saturating_sub(13),
                &format!("NORMAL {:.0}%", normal_floor * 100.0),
                1,
                [253, 184, 51, 255],
            );
        }
        if sip_floor > 0.0 {
            let y = map_y(sip_floor, top, bottom);
            canvas.dashed_line(left, y, right, 4, [255, 107, 107, 220]);
            canvas.text(
                left + 105,
                y.saturating_sub(13),
                &format!("SIP {:.0}%", sip_floor * 100.0),
                1,
                [255, 107, 107, 255],
            );
        }
    }

    let chart_span_secs = (now - start).max(1);
    let gap_limit = (chart_span_secs / 100).clamp(180, 900);
    for (index, (name, points)) in lines.iter().enumerate() {
        let color = COLORS[index % COLORS.len()];
        for segment in segments(points, gap_limit) {
            let mut previous: Option<Point> = None;
            for point in segment {
                let x = map_x(point.at, start, now, left, right);
                let y = map_y(point.remaining, top, bottom);
                if let Some(prev) = previous {
                    canvas.thick_line(
                        map_x(prev.at, start, now, left, right),
                        map_y(prev.remaining, top, bottom),
                        x,
                        y,
                        color,
                    );
                }
                canvas.circle(x, y, 2, color);
                previous = Some(point);
            }
        }

        let legend_x = right + 24;
        let legend_y = top + 8 + index as u32 * 34;
        canvas.line(legend_x, legend_y + 6, legend_x + 22, legend_y + 6, color);
        canvas.circle(legend_x + 11, legend_y + 6, 3, color);
        canvas.text(
            legend_x + 31,
            legend_y,
            &ellipsize(name, if width >= 1000 { 28 } else { 15 }),
            1,
            [215, 224, 237, 255],
        );
        if let Some(last) = points.last() {
            canvas.text(
                legend_x + 31,
                legend_y + 14,
                &format!("{:.1}% LEFT", last.remaining * 100.0),
                1,
                color,
            );
        }
    }

    let mut png_bytes = Vec::new();
    {
        let cursor = Cursor::new(&mut png_bytes);
        let mut encoder = png::Encoder::new(cursor, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&canvas.pixels)?;
    }
    Ok(png_bytes)
}

fn ellipsize(value: &str, max_chars: usize) -> String {
    let chars: Vec<_> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    chars
        .into_iter()
        .take(max_chars.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn map_x(timestamp: i64, start: i64, end: i64, left: u32, right: u32) -> u32 {
    let position = (timestamp - start) as f64 / (end - start).max(1) as f64;
    (left as f64 + position.clamp(0.0, 1.0) * (right - left) as f64).round() as u32
}

fn map_y(remaining: f64, top: u32, bottom: u32) -> u32 {
    (bottom as f64 - remaining.clamp(0.0, 1.0) * (bottom - top) as f64).round() as u32
}

struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32, background: [u8; 4]) -> Self {
        let mut pixels = vec![0; width as usize * height as usize * 4];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            *pixel = background;
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    fn pixel(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let offset = (y as usize * self.width as usize + x as usize) * 4;
        let alpha = color[3] as f32 / 255.0;
        for (index, channel) in color[..3].iter().enumerate() {
            self.pixels[offset + index] = (*channel as f32 * alpha
                + self.pixels[offset + index] as f32 * (1.0 - alpha))
                .round() as u8;
        }
        self.pixels[offset + 3] = 255;
    }

    fn line(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, color: [u8; 4]) {
        let (mut x0, mut y0, x1, y1) = (x0 as i32, y0 as i32, x1 as i32, y1 as i32);
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut error = dx + dy;
        loop {
            self.pixel(x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let double = 2 * error;
            if double >= dy {
                error += dy;
                x0 += sx;
            }
            if double <= dx {
                error += dx;
                y0 += sy;
            }
        }
    }

    fn thick_line(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, color: [u8; 4]) {
        for offset in -1..=1 {
            self.line(
                x0,
                (y0 as i32 + offset).max(0) as u32,
                x1,
                (y1 as i32 + offset).max(0) as u32,
                color,
            );
        }
    }

    fn dashed_line(&mut self, x0: u32, y: u32, x1: u32, dash: u32, color: [u8; 4]) {
        let mut x = x0;
        while x < x1 {
            self.line(x, y, (x + dash).min(x1), y, color);
            x = x.saturating_add(dash * 2);
        }
    }

    fn circle(&mut self, center_x: u32, center_y: u32, radius: i32, color: [u8; 4]) {
        for y in -radius..=radius {
            for x in -radius..=radius {
                if x * x + y * y <= radius * radius {
                    self.pixel(center_x as i32 + x, center_y as i32 + y, color);
                }
            }
        }
    }

    fn text(&mut self, x: u32, y: u32, text: &str, scale: u32, color: [u8; 4]) {
        let mut cursor = x;
        for character in text.chars() {
            let glyph = glyph(character);
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                self.pixel(
                                    (cursor + column * scale + sx) as i32,
                                    (y + row as u32 * scale + sy) as i32,
                                    color,
                                );
                            }
                        }
                    }
                }
            }
            cursor = cursor.saturating_add(6 * scale);
        }
    }
}

/// A 5x7 bitmap font: ASCII letters, digits and the punctuation the labels use.
fn glyph(character: char) -> [u8; 7] {
    match character.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 2, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '@' => [14, 17, 23, 21, 23, 16, 14],
        '%' => [17, 2, 4, 8, 16, 17, 0],
        '-' | '–' | '—' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        '/' => [1, 2, 4, 8, 16, 0, 0],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '·' => [0, 0, 0, 4, 0, 0, 0],
        '…' => [0, 0, 0, 0, 0, 21, 0],
        ' ' => [0; 7],
        _ => [0, 0, 14, 2, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::point;

    #[test]
    fn ellipsize_respects_character_count() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn coordinate_mapping_clamps() {
        assert_eq!(map_x(-10, 0, 100, 10, 110), 10);
        assert_eq!(map_x(150, 0, 100, 10, 110), 110);
        assert_eq!(map_y(0.0, 10, 110), 110);
        assert_eq!(map_y(1.0, 10, 110), 10);
    }

    fn history() -> Vec<Reading> {
        let mut claude = crate::testing::reading("a", 0.2, 0.4);
        claude.limits.push(crate::testing::model_limit(
            "opus48",
            "Opus 4.8",
            0.5,
            crate::quota::Source::Profile,
        ));
        claude.limits.push(crate::testing::model_limit(
            "fable",
            "Fable",
            0.5,
            crate::quota::Source::Headers,
        ));
        claude.limits.push(Limit::extra_usage(
            crate::quota::Window::new(0.1, 0),
            crate::quota::Source::Headers,
        ));
        vec![claude]
    }

    fn groups() -> Vec<Reading> {
        let limit = |group: &str, label: &str, secs: i64| {
            crate::providers::limit(
                Some(group.to_owned()),
                label,
                Some(secs),
                crate::quota::Window::new(0.3, 0),
                None,
            )
        };
        vec![Reading {
            limits: vec![
                limit("Gemini Models", "weekly", Span::WEEK_SECS),
                limit("Gemini Models", "5h", Span::FIVE_HOUR_SECS),
                limit("Claude and GPT models", "weekly", Span::WEEK_SECS),
            ],
            ..Reading::empty(Service::Antigravity, "g", 0)
        }]
    }

    #[test]
    fn a_metric_names_a_window_by_length_or_a_limit_by_what_it_covers() {
        let claude = history();
        let of = |metric: &str| resolve(Some(metric), &claude).unwrap();
        assert_eq!(of("five-hour"), Series::session());
        assert_eq!(of("5h"), Series::session());
        assert_eq!(of("seven-day"), Series::weekly());
        assert_eq!(of("monthly"), Series::new(Span::Monthly, Scope::All));
        assert_eq!(
            of("overage"),
            Series::new(Span::Unknown, Scope::extra_usage())
        );
        let model = |key: &str| Series::new(Span::Weekly, Scope::Model(ModelKey::new(key)));
        assert_eq!(of("Opus 4.8"), model("claude-opus-4-8"));
        assert_eq!(of("fable"), model("fable"));
        assert_eq!(of("opus"), model("opus48"), "a loose match on the label");
        assert_eq!(resolve(None, &claude).unwrap(), Series::session());
        let unknown = resolve(Some("sonnet"), &claude).unwrap_err().to_string();
        assert!(
            unknown.contains(
                "recorded limits: 5h, weekly, Opus 4.8 weekly, Fable weekly, extra usage"
            ),
            "{unknown}"
        );
        assert!(resolve(Some("---"), &claude).is_err());

        let agy = groups();
        let group = |span, name: &str| Series::new(span, Scope::Group(name.to_owned()));
        let of = |metric: &str| resolve(Some(metric), &agy).unwrap();
        assert_eq!(of("gemini"), group(Span::Weekly, "Gemini Models"));
        assert_eq!(of("gemini 5h"), group(Span::FiveHour, "Gemini Models"));
        assert_eq!(of("claude"), group(Span::Weekly, "Claude and GPT models"));
        assert!(resolve(Some("claude 5h"), &agy).is_err());
        // No account-wide limit: the first one reported.
        assert_eq!(
            resolve(None, &agy).unwrap(),
            group(Span::Weekly, "Gemini Models")
        );
        assert_eq!(series_name(&of("gemini 5h"), &agy), "Gemini Models 5h");
        assert!(resolve(None, &[]).is_err());
    }

    #[test]
    fn the_default_metric_does_not_depend_on_which_account_was_read_last() {
        // One account has a 5-hour limit; the other has the weekly alone.
        let both = |at: i64| {
            let mut reading = crate::testing::reading("a", 0.2, 0.4);
            reading.at = at;
            reading
        };
        let weekly_only = |at: i64| {
            let mut reading = crate::testing::reading("b", 0.0, 0.3);
            reading.set_session(None);
            reading.at = at;
            reading
        };
        assert_eq!(
            resolve(None, &[weekly_only(10), both(20)]).unwrap(),
            Series::session()
        );
        assert_eq!(
            resolve(None, &[both(10), weekly_only(20)]).unwrap(),
            Series::session()
        );
        // Nor on the order a reading lists its limits in.
        let mut reversed = both(30);
        reversed.limits.reverse();
        assert_eq!(resolve(None, &[reversed]).unwrap(), Series::session());
        // A history of weekly-only accounts charts the weekly.
        assert_eq!(resolve(None, &[weekly_only(10)]).unwrap(), Series::weekly());
    }

    #[test]
    fn only_the_rotation_windows_have_floors() {
        let policy = PolicySettings::default();
        assert_eq!(
            floors(Service::Claude, &Series::session(), &policy),
            Some((policy.normal_min_5h_remaining, policy.sip_min_5h_remaining))
        );
        assert_eq!(
            floors(Service::Claude, &Series::weekly(), &policy),
            Some((policy.normal_min_7d_remaining, policy.sip_min_7d_remaining))
        );
        assert_eq!(floors(Service::Codex, &Series::weekly(), &policy), None);
        let fable = Series::new(Span::Weekly, Scope::Model(ModelKey::new("fable")));
        assert_eq!(floors(Service::Claude, &fable, &policy), None);
    }

    #[test]
    fn a_chart_of_another_service_draws() {
        let mut later = groups().remove(0);
        later.at = 600;
        let readings = vec![groups().remove(0), later];
        let series = resolve(Some("gemini"), &readings).unwrap();
        let png = render_png(
            &PolicySettings::default(),
            Service::Antigravity,
            &readings,
            &series,
            1.0,
            800,
            400,
        )
        .unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let missing = Series::new(Span::Monthly, Scope::All);
        let error = render_png(
            &PolicySettings::default(),
            Service::Antigravity,
            &readings,
            &missing,
            1.0,
            800,
            400,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("recorded limits: Gemini Models weekly"),
            "{error}"
        );
    }

    #[test]
    fn a_held_value_is_a_step_and_a_silence_is_a_gap() {
        // Read every 20 s; the store kept a row per change and one per ten
        // minutes of hold.
        let points = [
            point(0, 0.9, Some(100)),
            point(20, 0.8, Some(100)),
            point(620, 0.8, Some(100)),
            point(1220, 0.7, Some(100)),
            point(5000, 0.7, Some(100)),
        ];
        let runs: Vec<Vec<(i64, f64)>> = segments(&points, 180)
            .iter()
            .map(|run| run.iter().map(|p| (p.at, p.remaining)).collect())
            .collect();
        assert_eq!(
            runs,
            [
                vec![(0, 0.9), (20, 0.8), (620, 0.8), (1220, 0.8), (1220, 0.7)],
                vec![(5000, 0.7)]
            ]
        );
        // A service read every half hour is joined at its own cadence.
        let slow: Vec<Point> = (0..5).map(|i| point(i * 1800, 0.5, None)).collect();
        assert_eq!(segments(&slow, 180).len(), 1);
    }

    #[test]
    fn segments_break_on_gaps_resets_and_refills() {
        let points = [
            point(0, 0.9, Some(100)),
            point(60, 0.8, Some(100)),
            point(1000, 0.7, Some(100)),
            point(1060, 0.6, None),
            point(1120, 0.5, Some(200)),
            point(1180, 0.9, Some(200)),
        ];
        let runs: Vec<Vec<i64>> = segments(&points, 600)
            .iter()
            .map(|run| run.iter().map(|p| p.at).collect())
            .collect();
        assert_eq!(runs, [vec![0, 60], vec![1000, 1060, 1120], vec![1180]]);
        assert!(segments(&[], 600).is_empty());
    }

    #[test]
    fn durations_read_as_days_hours_or_minutes() {
        assert_eq!(format_duration_hours(48.0), "2d");
        assert_eq!(format_duration_hours(36.0), "36h");
        assert_eq!(format_duration_hours(0.5), "30m");
    }
}
