use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{Duration, Utc};
use clap::ValueEnum;

use crate::config::Config;
use crate::store::{Snapshot, Store};

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

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChartMetric {
    FiveHour,
    SevenDay,
    OpusWeekly,
    SonnetWeekly,
    Overage,
}

impl ChartMetric {
    fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5-HOUR CAPACITY REMAINING",
            Self::SevenDay => "7-DAY CAPACITY REMAINING",
            Self::OpusWeekly => "OPUS 7-DAY CAPACITY REMAINING",
            Self::SonnetWeekly => "SONNET 7-DAY CAPACITY REMAINING",
            Self::Overage => "OVERAGE CAPACITY REMAINING",
        }
    }

    fn signal_label(self) -> &'static str {
        match self {
            Self::OpusWeekly | Self::SonnetWeekly => {
                "PROFILE USAGE SIGNAL · COLD-REQUEST ADMISSION CAN BE STRICTER"
            }
            Self::FiveHour | Self::SevenDay | Self::Overage => {
                "HAIKU 4.5 QUOTA SIGNAL · PREMIUM ADMISSION UNMEASURED"
            }
        }
    }

    fn utilization(self, snapshot: &Snapshot) -> Option<f64> {
        match self {
            Self::FiveHour => snapshot.utilization_5h,
            Self::SevenDay => snapshot.utilization_7d,
            Self::OpusWeekly => snapshot.utilization_opus_7d,
            Self::SonnetWeekly => snapshot.utilization_sonnet_7d,
            Self::Overage => snapshot.utilization_overage,
        }
    }

    fn reset(self, snapshot: &Snapshot) -> Option<i64> {
        match self {
            Self::FiveHour => snapshot.reset_5h,
            Self::SevenDay => snapshot.reset_7d,
            Self::OpusWeekly => snapshot.reset_opus_7d,
            Self::SonnetWeekly => snapshot.reset_sonnet_7d,
            Self::Overage => snapshot.reset_overage,
        }
    }
}

pub struct ChartOptions {
    pub hours: f64,
    pub metric: ChartMetric,
    pub width: u32,
    pub height: u32,
    pub output: Option<PathBuf>,
    pub force_iterm: bool,
}

#[derive(Clone, Copy)]
struct Point {
    timestamp: i64,
    remaining: f64,
    reset: Option<i64>,
}

pub fn run(config: &Config, options: ChartOptions) -> Result<PathBuf> {
    if !options.hours.is_finite() || options.hours <= 0.0 {
        bail!("--hours must be greater than zero");
    }
    if !(640..=3840).contains(&options.width) {
        bail!("--width must be between 640 and 3840");
    }
    if !(360..=2160).contains(&options.height) {
        bail!("--height must be between 360 and 2160");
    }

    let cutoff = Utc::now() - Duration::milliseconds((options.hours * 3_600_000.0).round() as i64);
    let snapshots = Store::open()?.all_since(cutoff)?;
    let png = render_png(
        config,
        &snapshots,
        options.metric,
        options.hours,
        options.width,
        options.height,
    )?;

    let output = options.output.unwrap_or_else(default_chart_path);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&output, &png)
        .with_context(|| format!("failed to write {}", output.display()))?;

    if options.force_iterm || (is_iterm2() && std::env::var_os("TMUX").is_none()) {
        print_iterm_image(&png);
    } else {
        println!(
            "{} (run directly inside iTerm2, or pass --iterm, for an inline chart)",
            output.display()
        );
    }
    Ok(output)
}

fn default_chart_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("tokeman").join("chart.png")
}

fn is_iterm2() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|value| value == "iTerm2")
}

fn print_iterm_image(png: &[u8]) {
    let name = base64::engine::general_purpose::STANDARD.encode("tokeman-chart.png");
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    println!(
        "\x1b]1337;File=name={name};inline=1;width=auto;height=auto;preserveAspectRatio=1:{data}\x07"
    );
}

fn render_png(
    config: &Config,
    snapshots: &[Snapshot],
    metric: ChartMetric,
    hours: f64,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let mut series: BTreeMap<String, Vec<Point>> = BTreeMap::new();
    for snapshot in snapshots {
        let Some(utilization) = metric.utilization(snapshot) else {
            continue;
        };
        if !utilization.is_finite() {
            continue;
        }
        series
            .entry(snapshot.token_name.clone())
            .or_default()
            .push(Point {
                timestamp: snapshot.probed_at.timestamp(),
                remaining: (1.0 - utilization).clamp(0.0, 1.0),
                reset: metric.reset(snapshot),
            });
    }
    if series.values().all(Vec::is_empty) {
        bail!("no chartable history in this range; run `tokeman` to collect a snapshot first");
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

    canvas.text(24, 22, metric.label(), 3, [229, 235, 244, 255]);
    canvas.text(
        24,
        52,
        &format!(
            "{} · LAST {}",
            metric.signal_label(),
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

    let (normal_floor, sip_floor) = match metric {
        ChartMetric::FiveHour => (
            config.rotation.normal_min_5h_remaining,
            config.rotation.sip_min_5h_remaining,
        ),
        ChartMetric::SevenDay => (
            config.rotation.normal_min_7d_remaining,
            config.rotation.sip_min_7d_remaining,
        ),
        ChartMetric::OpusWeekly | ChartMetric::SonnetWeekly => (0.0, 0.0),
        ChartMetric::Overage => (0.0, 0.0),
    };
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

    let chart_span_secs = (now - start).max(1);
    let gap_limit = (chart_span_secs / 100).clamp(180, 900);
    for (index, (name, points)) in series.iter().enumerate() {
        let color = COLORS[index % COLORS.len()];
        let mut previous: Option<Point> = None;
        for point in points {
            let x = map_x(point.timestamp, start, now, left, right);
            let y = map_y(point.remaining, top, bottom);
            if let Some(prev) = previous {
                let reset_changed =
                    prev.reset.is_some() && point.reset.is_some() && prev.reset != point.reset;
                let capacity_jumped = point.remaining > prev.remaining + 0.025;
                if point.timestamp - prev.timestamp <= gap_limit
                    && !reset_changed
                    && !capacity_jumped
                {
                    canvas.thick_line(
                        map_x(prev.timestamp, start, now, left, right),
                        map_y(prev.remaining, top, bottom),
                        x,
                        y,
                        color,
                    );
                }
            }
            canvas.circle(x, y, 2, color);
            previous = Some(*point);
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

fn format_duration_hours(hours: f64) -> String {
    if hours >= 24.0 && (hours / 24.0).fract().abs() < 0.01 {
        format!("{:.0}d", hours / 24.0)
    } else if hours >= 1.0 {
        format!("{hours:.0}h")
    } else {
        format!("{:.0}m", hours * 60.0)
    }
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
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&background);
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
}
