use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use super::bitmap::{Meteogram, CANVAS_HEIGHT, CANVAS_WIDTH};
use super::font;
use super::forecast::{Forecast, LocalHour};

const PLOT_LEFT: i32 = 31;
const PLOT_RIGHT: i32 = 330;
const FIRST_PANEL_TOP: i32 = 42;
const PANEL_HEIGHT: i32 = 74;
const PANEL_SPACING: i32 = 13;
const TOP_PANEL: Rect = panel(0);
const MIDDLE_PANEL: Rect = panel(1);
const BOTTOM_PANEL: Rect = panel(2);
const TOP_LEFT_AXIS_ZONE: Rect = Rect::new(0, 36, 11, 122);
const TOP_LEFT_TICK_ZONE: Rect = Rect::new(12, 36, 29, 122);
const CLOUD_LEFT_AXIS_ZONE: Rect = Rect::new(0, 123, 11, 209);
const CLOUD_LEFT_TICK_ZONE: Rect = Rect::new(12, 123, 29, 209);
const WIND_LEFT_AXIS_ZONE: Rect = Rect::new(0, 210, 11, 296);
const WIND_LEFT_TICK_ZONE: Rect = Rect::new(12, 210, 29, 296);
const TEMP_RIGHT_TICK_ZONE: Rect = Rect::new(332, 36, 349, 122);
const TEMP_RIGHT_AXIS_ZONE: Rect = Rect::new(350, 36, 361, 122);

const TOP_GRID_INTERVALS: i32 = 5;
const TOP_AXIS_STEPS: [i32; 10] = [1, 2, 3, 4, 5, 10, 15, 20, 25, 50];
const ALTITUDE_TICKS: [f64; 6] = [0.0, 1.5, 3.5, 6.0, 9.0, 14.0];
const CLOUD_PRESSURES: [i32; 21] = [
    1000, 975, 950, 925, 900, 875, 850, 800, 750, 700, 650, 600, 550, 500, 450, 400, 350, 300, 250,
    200, 150,
];
const HEIGHT_PRESSURES: [i32; 18] = [
    1000, 950, 900, 850, 800, 750, 700, 650, 600, 550, 500, 450, 400, 350, 300, 250, 200, 150,
];
const MPH_PER_MPS: f64 = 2.236936;
const GRID_DOT_SPACING: i32 = 5;
const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

const LIGHTNING_POSSIBLE: [&str; 9] = [
    "...#", "..#.", ".#..", "####", "..#.", ".#..", "#...", "#...", "....",
];
const LIGHTNING_LIKELY: [&str; 9] = [
    "..##", ".##.", "##..", "####", "..##", ".##.", "##..", "#...", "....",
];
const WIND_NORTH: [&str; 9] = [
    "....#....",
    "...###...",
    "..#####..",
    ".#######.",
    "#########",
    "...###...",
    "...###...",
    "...###...",
    "...###...",
];
const WIND_NORTH_EAST: [&str; 9] = [
    "..#######",
    "...######",
    "....#####",
    "...######",
    "..####.##",
    ".####...#",
    "####.....",
    "###......",
    ".#.......",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    TemperatureRange,
    PrecipitationRange,
    InvalidProfile,
    LabelDoesNotFit,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TemperatureRange => "temperature range does not fit the meteogram axis",
            Self::PrecipitationRange => "precipitation range does not fit the meteogram axis",
            Self::InvalidProfile => "cloud profile cannot be interpolated",
            Self::LabelDoesNotFit => "meteogram label does not fit its fixed zone",
        })
    }
}
impl std::error::Error for RenderError {}

#[derive(Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl Rect {
    const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
    fn width(self) -> i32 {
        self.right - self.left + 1
    }
    fn height(self) -> i32 {
        self.bottom - self.top + 1
    }
}
const fn panel(index: i32) -> Rect {
    let top = FIRST_PANEL_TOP + index * (PANEL_HEIGHT + PANEL_SPACING);
    Rect::new(PLOT_LEFT, top, PLOT_RIGHT, top + PANEL_HEIGHT)
}

#[derive(Clone, Copy)]
struct CloudSample {
    altitude_km: f64,
    scale_pressure: f64,
    cover: f64,
    interpolated_height: bool,
}

#[inline]
fn py_round(value: f64) -> i32 {
    value.round_ties_even() as i32
}

fn glyph_points(text: &str) -> Vec<(i32, i32)> {
    let mut points = Vec::new();
    for (character_index, character) in text.chars().enumerate() {
        for (y, row) in font::glyph(character).iter().copied().enumerate() {
            for x in 0..font::WIDTH {
                if row & (0x80 >> x) != 0 {
                    points.push((character_index as i32 * font::WIDTH + x, y as i32));
                }
            }
        }
    }
    points
}

fn draw_text(bitmap: &mut Meteogram, x: i32, y: i32, text: &str) {
    for (dx, dy) in glyph_points(text) {
        bitmap.point(x + dx, y + dy);
    }
}

fn text_ink_bounds(text: &str) -> Option<Rect> {
    let points = glyph_points(text);
    let left = points.iter().map(|p| p.0).min()?;
    let right = points.iter().map(|p| p.0).max()?;
    let top = points.iter().map(|p| p.1).min()?;
    let bottom = points.iter().map(|p| p.1).max()?;
    Some(Rect::new(left, top, right, bottom))
}

fn draw_vertical_label(bitmap: &mut Meteogram, text: &str, zone: Rect) -> Result<(), RenderError> {
    let bounds = text_ink_bounds(text).ok_or(RenderError::LabelDoesNotFit)?;
    let cropped_width = bounds.width();
    let cropped_height = bounds.height();
    let rotated_width = cropped_height;
    let rotated_height = cropped_width;
    if rotated_width > zone.width() || rotated_height > zone.height() {
        return Err(RenderError::LabelDoesNotFit);
    }
    let target_x = zone.left + (zone.width() - rotated_width) / 2;
    let target_y = zone.top + (zone.height() - rotated_height) / 2;
    for (x, y) in glyph_points(text) {
        let crop_x = x - bounds.left;
        let crop_y = y - bounds.top;
        // Pillow Image.rotate(90, expand=True): 90 degrees counter-clockwise.
        bitmap.point(target_x + crop_y, target_y + cropped_width - 1 - crop_x);
    }
    Ok(())
}

fn draw_tick_label(
    bitmap: &mut Meteogram,
    zone: Rect,
    y: i32,
    text: &str,
    right_aligned: bool,
) -> Result<(), RenderError> {
    let text_width = text.chars().count() as i32 * font::WIDTH;
    if text_width > zone.width() {
        return Err(RenderError::LabelDoesNotFit);
    }
    let x = if right_aligned {
        zone.right - text_width + 1
    } else {
        zone.left
    };
    let top = py_round(y as f64 - font::HEIGHT as f64 / 2.0)
        .clamp(zone.top, zone.bottom - font::HEIGHT + 1);
    draw_text(bitmap, x, top, text);
    Ok(())
}

fn standard_pressure(altitude_km: f64) -> f64 {
    let metres = altitude_km.clamp(0.0, 14.0) * 1000.0;
    if metres <= 11_000.0 {
        1013.25 * (1.0 - 2.25577e-5 * metres).powf(5.25588)
    } else {
        226.3206 * (-(metres - 11_000.0) / 6341.62).exp()
    }
}

fn altitude_to_y(altitude_km: f64) -> f64 {
    let pressure = standard_pressure(altitude_km);
    let top_pressure = standard_pressure(14.0);
    let fraction = (pressure - top_pressure) / (standard_pressure(0.0) - top_pressure);
    MIDDLE_PANEL.top as f64 + fraction * (MIDDLE_PANEL.bottom - MIDDLE_PANEL.top) as f64
}

fn y_to_scale_pressure(y: i32) -> f64 {
    let y = y.clamp(MIDDLE_PANEL.top, MIDDLE_PANEL.bottom);
    let fraction = (y - MIDDLE_PANEL.top) as f64 / (MIDDLE_PANEL.bottom - MIDDLE_PANEL.top) as f64;
    standard_pressure(14.0) + fraction * (standard_pressure(0.0) - standard_pressure(14.0))
}

fn height_index(pressure: i32) -> Option<usize> {
    HEIGHT_PRESSURES.iter().position(|value| *value == pressure)
}

fn interpolate_height(forecast: &Forecast, hour: usize, pressure: i32) -> Result<f64, RenderError> {
    if let Some(index) = height_index(pressure) {
        return Ok(forecast.height[index][hour] as f64);
    }
    let higher = HEIGHT_PRESSURES
        .iter()
        .copied()
        .filter(|p| *p > pressure)
        .min()
        .ok_or(RenderError::InvalidProfile)?;
    let lower = HEIGHT_PRESSURES
        .iter()
        .copied()
        .filter(|p| *p < pressure)
        .max()
        .ok_or(RenderError::InvalidProfile)?;
    let high_height = forecast.height[height_index(higher).unwrap()][hour] as f64;
    let low_height = forecast.height[height_index(lower).unwrap()][hour] as f64;
    let fraction = ((higher as f64).ln() - (pressure as f64).ln())
        / ((higher as f64).ln() - (lower as f64).ln());
    Ok(high_height + (low_height - high_height) * fraction)
}

fn cloud_samples(forecast: &Forecast, hour: usize) -> Result<Vec<CloudSample>, RenderError> {
    let mut samples = Vec::with_capacity(21);
    for (index, pressure) in CLOUD_PRESSURES.iter().copied().enumerate() {
        let Some(values) = &forecast.cloud[index] else {
            continue;
        };
        let altitude_km = interpolate_height(forecast, hour, pressure)? / 1000.0;
        samples.push(CloudSample {
            altitude_km,
            scale_pressure: standard_pressure(altitude_km),
            cover: values[hour] as f64,
            interpolated_height: height_index(pressure).is_none(),
        });
    }
    samples.sort_by(|a, b| {
        a.scale_pressure
            .partial_cmp(&b.scale_pressure)
            .unwrap_or(Ordering::Equal)
    });
    Ok(samples)
}

fn interpolated_cloud_cover(samples: &[CloudSample], pressure: f64) -> Option<f64> {
    let upper = samples.partition_point(|sample| sample.scale_pressure <= pressure);
    if upper == 0 || upper >= samples.len() {
        return None;
    }
    let lower_sample = samples[upper - 1];
    let upper_sample = samples[upper];
    let span = upper_sample.scale_pressure - lower_sample.scale_pressure;
    if span <= 0.0 {
        return Some(lower_sample.cover.max(upper_sample.cover));
    }
    let fraction = (pressure - lower_sample.scale_pressure) / span;
    Some(lower_sample.cover + (upper_sample.cover - lower_sample.cover) * fraction)
}

fn cloud_black(cover: f64, x: i32, y: i32) -> bool {
    let level = py_round(cover.clamp(0.0, 100.0) * 16.0 / 100.0);
    BAYER[(y & 3) as usize][(x & 3) as usize] < level
}

fn hour_x(index: usize, count: usize) -> i32 {
    py_round(PLOT_LEFT as f64 + index as f64 * (PLOT_RIGHT - PLOT_LEFT) as f64 / (count - 1) as f64)
}

fn hour_cell(index: usize, count: usize) -> (i32, i32) {
    let width = (PLOT_RIGHT - PLOT_LEFT) as f64;
    let centre = PLOT_LEFT as f64 + index as f64 * width / (count - 1) as f64;
    let previous = if index == 0 {
        PLOT_LEFT as f64
    } else {
        PLOT_LEFT as f64 + (index - 1) as f64 * width / (count - 1) as f64
    };
    let following = if index + 1 == count {
        PLOT_RIGHT as f64
    } else {
        PLOT_LEFT as f64 + (index + 1) as f64 * width / (count - 1) as f64
    };
    let left = if index == 0 {
        PLOT_LEFT
    } else {
        py_round((previous + centre) / 2.0)
    };
    let right = if index + 1 == count {
        PLOT_RIGHT
    } else {
        py_round((centre + following) / 2.0)
    };
    (left, right.max(left))
}

fn hour_for_x(x: i32, count: usize) -> usize {
    py_round((x - PLOT_LEFT) as f64 / (PLOT_RIGHT - PLOT_LEFT) as f64 * (count - 1) as f64)
        .clamp(0, count as i32 - 1) as usize
}

fn draw_cloud(bitmap: &mut Meteogram, forecast: &Forecast) -> Result<(), RenderError> {
    let count = forecast.hour_count();
    for hour in 0..count {
        let (left, right) = hour_cell(hour, count);
        for sample in cloud_samples(forecast, hour)? {
            if sample.interpolated_height {
                continue;
            }
            let y = py_round(altitude_to_y(sample.altitude_km));
            for x in left..=right {
                if cloud_black(sample.cover, x, y) {
                    bitmap.point(x, y);
                }
            }
        }
    }
    let mut cached_hour = usize::MAX;
    let mut samples = Vec::new();
    let mut direct_rows = Vec::new();
    for x in PLOT_LEFT..=PLOT_RIGHT {
        let hour = hour_for_x(x, count);
        if hour != cached_hour {
            samples = cloud_samples(forecast, hour)?;
            direct_rows = samples
                .iter()
                .filter(|s| !s.interpolated_height)
                .map(|s| py_round(altitude_to_y(s.altitude_km)))
                .collect();
            cached_hour = hour;
        }
        for y in MIDDLE_PANEL.top..=MIDDLE_PANEL.bottom {
            if direct_rows.contains(&y) {
                continue;
            }
            if interpolated_cloud_cover(&samples, y_to_scale_pressure(y))
                .is_some_and(|cover| cloud_black(cover, x, y))
            {
                bitmap.point(x, y);
            }
        }
    }
    Ok(())
}

fn anchored_dots(anchors: &[i32]) -> Vec<i32> {
    let mut distinct = anchors.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let mut dots = vec![distinct[0]];
    for pair in distinct.windows(2) {
        let gap = pair[1] - pair[0];
        let steps = py_round(gap as f64 / GRID_DOT_SPACING as f64).max(1);
        for step in 1..=steps {
            dots.push(pair[0] + py_round(step as f64 * gap as f64 / steps as f64));
        }
    }
    dots
}

fn draw_grid<F: Fn(i32) -> i32>(
    bitmap: &mut Meteogram,
    times: &[LocalHour],
    top: i32,
    bottom: i32,
    ticks: &[i32],
    guide_floor: F,
) {
    let guides: Vec<(i32, bool)> = times
        .iter()
        .enumerate()
        .filter(|(_, time)| matches!(time.hour, 0 | 6 | 12 | 18))
        .map(|(index, time)| (hour_x(index, times.len()), time.hour == 0))
        .collect();
    let mut anchors = vec![PLOT_LEFT, PLOT_RIGHT];
    anchors.extend(guides.iter().map(|g| g.0));
    let columns = anchored_dots(&anchors);
    for y in ticks {
        for x in &columns {
            bitmap.point(*x, *y);
        }
    }
    let mut row_anchors = vec![top, bottom];
    row_anchors.extend_from_slice(ticks);
    let rows = anchored_dots(&row_anchors);
    for (x, midnight) in guides {
        if midnight {
            bitmap.vertical(x, top, bottom, true);
        } else {
            let floor = guide_floor(x);
            for y in &rows {
                if *y <= floor {
                    bitmap.point(x, *y);
                }
            }
        }
    }
}

fn month_name(month: u8) -> &'static str {
    [
        "???", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize]
}
fn weekday_name(day: u8) -> &'static str {
    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][day as usize]
}

fn draw_layout(bitmap: &mut Meteogram, forecast: &Forecast) {
    let ground = if forecast.ground_metres.fract() == 0.0 {
        format!("{}", forecast.ground_metres as i32)
    } else {
        forecast.ground_metres.to_string()
    };
    draw_text(
        bitmap,
        2,
        0,
        &format!("{} {ground}m", forecast.location_name),
    );
    let first = forecast.times[0];
    let last = forecast.times[forecast.times.len() - 1];
    draw_text(
        bitmap,
        2,
        12,
        &format!(
            "{:02}{}-{:02}{}",
            first.day,
            month_name(first.month),
            last.day,
            month_name(last.month)
        ),
    );
    for panel in [TOP_PANEL, MIDDLE_PANEL, BOTTOM_PANEL] {
        bitmap.rect_outline(panel.left, panel.top, panel.right, panel.bottom);
    }
    for (index, time) in forecast.times.iter().enumerate() {
        if time.hour != 12 {
            continue;
        }
        let text = weekday_name(time.weekday);
        let width = text.len() as i32 * font::WIDTH;
        draw_text(
            bitmap,
            py_round(hour_x(index, forecast.hour_count()) as f64 - width as f64 / 2.0),
            29,
            text,
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TemperatureAxis {
    base: i32,
    step: i32,
}
impl TemperatureAxis {
    fn ticks(self) -> impl Iterator<Item = i32> {
        (0..=TOP_GRID_INTERVALS).map(move |i| self.base + i * self.step)
    }
    fn row(self, value: f64) -> i32 {
        py_round(
            TOP_PANEL.bottom as f64
                - (value - self.base as f64) / self.step as f64
                    * (TOP_PANEL.bottom - TOP_PANEL.top) as f64
                    / TOP_GRID_INTERVALS as f64,
        )
    }
}

fn temperature_axis(low: f64, high: f64) -> Result<TemperatureAxis, RenderError> {
    let interval_rows = (TOP_PANEL.bottom - TOP_PANEL.top) as f64 / TOP_GRID_INTERVALS as f64;
    let bottom_clearance = 11.0 / interval_rows;
    let top_clearance = 3.0 / interval_rows;
    for step in TOP_AXIS_STEPS {
        let base = (low / step as f64 - bottom_clearance).floor() as i32 * step;
        if base as f64 + step as f64 * (TOP_GRID_INTERVALS as f64 - top_clearance) >= high {
            return Ok(TemperatureAxis { base, step });
        }
    }
    Err(RenderError::TemperatureRange)
}

fn line_pixels(start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    let (mut x, mut y) = start;
    let dx = (end.0 - x).abs();
    let dy = -(end.1 - y).abs();
    let step_x = if end.0 > x { 1 } else { -1 };
    let step_y = if end.1 > y { 1 } else { -1 };
    let mut error = dx + dy;
    let mut result = Vec::new();
    loop {
        result.push((x, y));
        if (x, y) == end {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += step_x;
        }
        if doubled <= dx {
            error += dx;
            y += step_y;
        }
    }
    result
}

fn polyline(points: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut path = vec![points[0]];
    for pair in points.windows(2) {
        path.extend(line_pixels(pair[0], pair[1]).into_iter().skip(1));
    }
    path
}

fn turning_points(values: &[f32]) -> (Vec<usize>, Vec<usize>) {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if runs.last().is_some_and(|run| values[run.0] == *value) {
            runs.last_mut().unwrap().1 = index;
        } else {
            runs.push((index, index));
        }
    }
    let mut maxima = Vec::new();
    let mut minima = Vec::new();
    for window in runs.windows(3) {
        let previous = window[0];
        let current = window[1];
        let following = window[2];
        let middle = (current.0 + current.1) / 2;
        if values[current.0] > values[previous.0] && values[current.0] > values[following.0] {
            maxima.push(middle);
        } else if values[current.0] < values[previous.0] && values[current.0] < values[following.0]
        {
            minima.push(middle);
        }
    }
    (maxima, minima)
}

fn daily_turning_extremes(times: &[LocalHour], temperatures: &[f32]) -> Vec<usize> {
    let mut counts: BTreeMap<(i32, u8, u8), usize> = BTreeMap::new();
    for time in times {
        *counts.entry((time.year, time.month, time.day)).or_default() += 1;
    }
    let (maxima, minima) = turning_points(temperatures);
    let mut chosen = Vec::new();
    for (candidates, maximum) in [(maxima, true), (minima, false)] {
        let mut by_day: BTreeMap<(i32, u8, u8), Vec<usize>> = BTreeMap::new();
        for index in candidates {
            let time = times[index];
            by_day
                .entry((time.year, time.month, time.day))
                .or_default()
                .push(index);
        }
        for (day, indices) in by_day {
            if counts[&day] < 12 {
                continue;
            }
            let selected = indices
                .into_iter()
                .reduce(|best, item| {
                    let better = if maximum {
                        temperatures[item] > temperatures[best]
                    } else {
                        temperatures[item] < temperatures[best]
                    };
                    if better {
                        item
                    } else {
                        best
                    }
                })
                .unwrap();
            chosen.push(selected);
        }
    }
    chosen.sort_unstable();
    chosen
}

#[derive(Clone)]
struct TemperatureLabel {
    text: String,
    ink_left: i32,
    ink_top: i32,
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.left <= b.right && b.left <= a.right && a.top <= b.bottom && b.top <= a.bottom
}

fn place_temperature_labels(
    points: &[(i32, i32)],
    lowest: &[i32; 300],
    temperatures: &[f32],
    indices: &[usize],
) -> Vec<TemperatureLabel> {
    let mut labels = Vec::new();
    let mut boxes = Vec::new();
    for index in indices {
        let text = py_round(temperatures[*index] as f64).to_string();
        let bounds = text_ink_bounds(&text).unwrap();
        let width = bounds.width();
        let x = points[*index].0;
        let left = (x - width / 2).clamp(PLOT_LEFT + 2, PLOT_RIGHT - 1 - width);
        let mut line_lowest = 0;
        for column in left - 2..left + width + 2 {
            if (PLOT_LEFT..=PLOT_RIGHT).contains(&column) {
                line_lowest = line_lowest.max(lowest[(column - PLOT_LEFT) as usize]);
            }
        }
        let top = line_lowest + 2;
        let bbox = Rect::new(left - 1, top - 1, left + width, top + 8);
        let spaced = Rect::new(bbox.left - 4, bbox.top, bbox.right + 4, bbox.bottom);
        if boxes.iter().copied().any(|placed| overlaps(spaced, placed)) {
            continue;
        }
        boxes.push(bbox);
        labels.push(TemperatureLabel {
            text,
            ink_left: left,
            ink_top: top,
        });
    }
    labels
}

fn draw_temperature_label(bitmap: &mut Meteogram, label: &TemperatureLabel) {
    let points = glyph_points(&label.text);
    let bounds = text_ink_bounds(&label.text).unwrap();
    let left = label.ink_left - bounds.left;
    let top = label.ink_top - bounds.top;
    for (x, y) in &points {
        for dx in -1..=1 {
            for dy in -1..=1 {
                bitmap.white_point(left + x + dx, top + y + dy);
            }
        }
    }
    for (x, y) in points {
        bitmap.point(left + x, top + y);
    }
}

fn precipitation_step(maximum: f64) -> Result<i32, RenderError> {
    if maximum <= 5.0 {
        return Ok(1);
    }
    let usable = 5.0 - 3.0 / ((TOP_PANEL.bottom - TOP_PANEL.top) as f64 / 5.0);
    TOP_AXIS_STEPS
        .into_iter()
        .find(|step| maximum <= *step as f64 * usable)
        .ok_or(RenderError::PrecipitationRange)
}

fn in_any_rect(x: i32, y: i32, rectangles: &[Rect]) -> bool {
    rectangles
        .iter()
        .any(|rect| x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom)
}

fn lightning_marks(codes: &[i16]) -> Vec<(usize, usize, bool)> {
    let mut starts: Vec<usize> = (0..codes.len()).step_by(4).collect();
    if starts.len() > 1 && codes.len() - starts[starts.len() - 1] < 4 {
        starts.pop();
    }
    let mut result = Vec::new();
    for i in 0..starts.len() {
        let start = starts[i];
        let end = starts.get(i + 1).copied().unwrap_or(codes.len());
        let likely = codes[start..end]
            .iter()
            .any(|code| matches!(code, 27 | 28 | 30));
        let possible = codes[start..end]
            .iter()
            .any(|code| matches!(code, 10 | 11 | 12));
        if likely || possible {
            result.push((start, end - 1, likely));
        }
    }
    result
}

fn draw_precipitation(bitmap: &mut Meteogram, forecast: &Forecast) -> Result<(), RenderError> {
    draw_vertical_label(bitmap, "Precip (mm/h)", TOP_LEFT_AXIS_ZONE)?;
    draw_vertical_label(bitmap, "Temp (C)", TEMP_RIGHT_AXIS_ZONE)?;
    let temperatures: Vec<f64> = forecast.temperature.iter().map(|v| *v as f64).collect();
    let axis = temperature_axis(
        temperatures.iter().copied().fold(f64::INFINITY, f64::min),
        temperatures
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max),
    )?;
    let points: Vec<(i32, i32)> = temperatures
        .iter()
        .enumerate()
        .map(|(i, v)| (hour_x(i, temperatures.len()), axis.row(*v)))
        .collect();
    let path = polyline(&points);
    let mut highest = [TOP_PANEL.bottom; 300];
    let mut lowest = [0; 300];
    for (x, y) in &path {
        if (PLOT_LEFT..=PLOT_RIGHT).contains(x) {
            let index = (*x - PLOT_LEFT) as usize;
            highest[index] = highest[index].min(*y);
            lowest[index] = lowest[index].max(*y);
        }
    }
    let labels = place_temperature_labels(
        &points,
        &lowest,
        &forecast.temperature,
        &daily_turning_extremes(&forecast.times, &forecast.temperature),
    );
    let amounts: Vec<f64> = forecast.precipitation.iter().map(|v| *v as f64).collect();
    let precip_step = precipitation_step(amounts.iter().copied().fold(0.0, f64::max))?;
    let tick_ys: Vec<i32> = axis.ticks().map(|tick| axis.row(tick as f64)).collect();
    for (interval, y) in tick_ys.iter().enumerate() {
        draw_tick_label(
            bitmap,
            TOP_LEFT_TICK_ZONE,
            *y,
            &(interval as i32 * precip_step).to_string(),
            true,
        )?;
    }
    for (tick, y) in axis.ticks().zip(&tick_ys) {
        draw_tick_label(bitmap, TEMP_RIGHT_TICK_ZONE, *y, &tick.to_string(), false)?;
    }
    draw_grid(
        bitmap,
        &forecast.times,
        TOP_PANEL.top,
        TOP_PANEL.bottom,
        &tick_ys,
        |x| highest[(x - PLOT_LEFT).clamp(0, 299) as usize] - 2,
    );
    if axis.base <= 0 && axis.base + 5 * axis.step >= 0 {
        bitmap.horizontal(PLOT_LEFT, PLOT_RIGHT, axis.row(0.0), true);
    }

    let mut all_bars = Vec::new();
    let mut snow_bars = Vec::new();
    let full_scale = 5.0 * precip_step as f64;
    for (index, amount) in amounts.iter().copied().enumerate() {
        if amount == 0.0 {
            continue;
        }
        let (left, right) = hour_cell(index, amounts.len());
        let height =
            py_round(amount / full_scale * (TOP_PANEL.bottom - TOP_PANEL.top) as f64).max(1);
        let bar = Rect::new(left, TOP_PANEL.bottom - height + 1, right, TOP_PANEL.bottom);
        all_bars.push(bar);
        if forecast.snowfraction[index] as f64 >= 0.5 {
            snow_bars.push(bar);
        } else {
            for y in bar.top..=bar.bottom {
                bitmap.horizontal(bar.left, bar.right, y, true);
            }
        }
    }
    for bar in &snow_bars {
        for y in bar.top..=bar.bottom {
            for x in bar.left..=bar.right {
                let edge = !in_any_rect(x - 1, y, &snow_bars)
                    || !in_any_rect(x + 1, y, &snow_bars)
                    || !in_any_rect(x, y - 1, &snow_bars)
                    || !in_any_rect(x, y + 1, &snow_bars);
                if (x - y).rem_euclid(3) == 0 || edge {
                    bitmap.point(x, y);
                }
            }
        }
    }
    for (x, y) in &path {
        for dx in -1..=1 {
            for dy in -1..=1 {
                if (dx != 0 || dy != 0)
                    && in_any_rect(x + dx, y + dy, &all_bars)
                    && !path.contains(&(x + dx, y + dy))
                {
                    bitmap.white_point(x + dx, y + dy);
                }
            }
        }
    }
    for (x, y) in path {
        bitmap.point(x, y);
    }
    for label in &labels {
        draw_temperature_label(bitmap, label);
    }

    for (start, last, likely) in lightning_marks(&forecast.pictocode) {
        let centre = py_round(
            PLOT_LEFT as f64
                + (start + last) as f64 / 2.0 * (PLOT_RIGHT - PLOT_LEFT) as f64
                    / (forecast.hour_count() - 1) as f64,
        )
        .clamp(PLOT_LEFT + 2, PLOT_RIGHT - 1);
        let glyph = if likely {
            &LIGHTNING_LIKELY
        } else {
            &LIGHTNING_POSSIBLE
        };
        for (y, row) in glyph.iter().enumerate() {
            for (x, cell) in row.bytes().enumerate() {
                if cell == b'#' {
                    bitmap.point(centre - 2 + x as i32, TOP_PANEL.bottom + 2 + y as i32);
                }
            }
        }
    }
    Ok(())
}

fn wind_axis(maximum: f64) -> (i32, i32) {
    let mut magnitude = 1;
    loop {
        for factor in [1, 2, 5] {
            let step = factor * magnitude;
            let high = ((maximum / step as f64).ceil() as i32 * step).max(step);
            if high / step <= 5 {
                return (high, step);
            }
        }
        magnitude *= 10;
    }
}

fn wind_arrow_offsets(point: i32) -> Vec<(i32, i32)> {
    let rows = if point % 2 == 0 {
        &WIND_NORTH
    } else {
        &WIND_NORTH_EAST
    };
    let mut offsets: Vec<(i32, i32)> = rows
        .iter()
        .enumerate()
        .flat_map(|(y, row)| {
            row.bytes()
                .enumerate()
                .filter(|(_, c)| *c == b'#')
                .map(move |(x, _)| (x as i32 - 4, y as i32 - 4))
        })
        .collect();
    for _ in 0..point / 2 {
        offsets = offsets.into_iter().map(|(x, y)| (-y, x)).collect();
    }
    offsets
}

fn draw_wind(bitmap: &mut Meteogram, forecast: &Forecast) -> Result<(), RenderError> {
    draw_vertical_label(bitmap, "Wind (mph)", WIND_LEFT_AXIS_ZONE)?;
    let speeds: Vec<f64> = forecast
        .windspeed
        .iter()
        .map(|v| *v as f64 * MPH_PER_MPS)
        .collect();
    let gusts: Vec<f64> = forecast
        .gust
        .iter()
        .map(|v| *v as f64 * MPH_PER_MPS)
        .collect();
    let (high, step) = wind_axis(speeds.iter().chain(&gusts).copied().fold(0.0, f64::max));
    let top = BOTTOM_PANEL.top + 12;
    let bottom = BOTTOM_PANEL.bottom;
    let y_of = |mph: f64| py_round(bottom as f64 - mph / high as f64 * (bottom - top) as f64);
    let ticks: Vec<i32> = (0..=high / step).map(|i| i * step).collect();
    for tick in &ticks {
        draw_tick_label(
            bitmap,
            WIND_LEFT_TICK_ZONE,
            y_of(*tick as f64),
            &tick.to_string(),
            true,
        )?;
    }
    let tick_rows: Vec<i32> = ticks.iter().map(|v| y_of(*v as f64)).collect();
    draw_grid(bitmap, &forecast.times, top, bottom, &tick_rows, |_| bottom);
    let speed_points: Vec<_> = speeds
        .iter()
        .enumerate()
        .map(|(i, v)| (hour_x(i, speeds.len()), y_of(*v)))
        .collect();
    for (x, y) in polyline(&speed_points) {
        for (dx, dy) in [(0, 0), (1, 0), (0, -1), (1, -1)] {
            if x + dx <= PLOT_RIGHT {
                bitmap.point(x + dx, y + dy);
            }
        }
    }
    let gust_points: Vec<_> = gusts
        .iter()
        .enumerate()
        .map(|(i, v)| (hour_x(i, gusts.len()), y_of(*v)))
        .collect();
    for (x, y) in polyline(&gust_points) {
        bitmap.point(x, y);
    }
    let arrow_y = BOTTOM_PANEL.top + 1 + 11 / 2;
    for (index, time) in forecast.times.iter().enumerate() {
        if !matches!(time.hour, 3 | 9 | 15 | 21) {
            continue;
        }
        let x = hour_x(index, forecast.hour_count()).clamp(PLOT_LEFT + 6, PLOT_RIGHT - 6);
        let point =
            py_round(((forecast.winddirection[index] as f64 + 180.0) % 360.0) / 45.0).rem_euclid(8);
        for (dx, dy) in wind_arrow_offsets(point) {
            bitmap.point(x + dx, arrow_y + dy);
        }
    }
    Ok(())
}

fn draw_cloud_structure(bitmap: &mut Meteogram, forecast: &Forecast) -> Result<(), RenderError> {
    draw_vertical_label(bitmap, "Alt (Km)", CLOUD_LEFT_AXIS_ZONE)?;
    let ticks: Vec<i32> = ALTITUDE_TICKS
        .iter()
        .map(|v| py_round(altitude_to_y(*v)))
        .collect();
    for (altitude, y) in ALTITUDE_TICKS.iter().zip(&ticks) {
        let text = if altitude.fract() == 0.0 {
            format!("{}", *altitude as i32)
        } else {
            altitude.to_string()
        };
        draw_tick_label(bitmap, CLOUD_LEFT_TICK_ZONE, *y, &text, true)?;
    }
    draw_grid(
        bitmap,
        &forecast.times,
        MIDDLE_PANEL.top,
        MIDDLE_PANEL.bottom,
        &ticks,
        |_| MIDDLE_PANEL.bottom,
    );
    let ground = py_round(altitude_to_y(forecast.ground_metres as f64 / 1000.0));
    for y in ground + 1..MIDDLE_PANEL.bottom {
        for x in PLOT_LEFT + 1..PLOT_RIGHT {
            if (x + y).rem_euclid(8) == 0 {
                bitmap.point(x, y);
            }
        }
    }
    bitmap.horizontal(PLOT_LEFT, PLOT_RIGHT, ground, true);
    Ok(())
}

/// Render the production combined layer directly into its packed bitmap.
pub fn render(forecast: &Forecast) -> Result<Meteogram, RenderError> {
    let mut bitmap = Meteogram::white();
    draw_cloud(&mut bitmap, forecast)?;
    draw_layout(&mut bitmap, forecast);
    draw_precipitation(&mut bitmap, forecast)?;
    draw_cloud_structure(&mut bitmap, forecast)?;
    draw_wind(&mut bitmap, forecast)?;
    debug_assert_eq!(
        bitmap.bytes().len(),
        ((CANVAS_WIDTH as usize + 7) / 8) * CANVAS_HEIGHT as usize
    );
    Ok(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteo::{decode_forecast, ForecastRequest, Location};
    use std::fs::File;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }
    fn golden(location: Location, fixture: &str, expected: &str) {
        let root = root();
        let request = ForecastRequest::new(location);
        let forecast = decode_forecast(
            File::open(root.join("meteograms").join(fixture)).unwrap(),
            &request,
        )
        .unwrap();
        let actual = render(&forecast).unwrap();
        let expected = std::fs::read(
            root.join("meteogram-poc/out-source")
                .join(expected)
                .join("combined/bitmap.bin"),
        )
        .unwrap();
        assert_eq!(actual.bytes(), expected.as_slice());
    }

    #[test]
    fn shelterstone_matches_poc_byte_for_byte() {
        golden(
            Location::new(
                "shelterstone",
                "Shelterstone",
                57.09,
                -3.65,
                Some(1047),
                "Europe/London",
            )
            .unwrap(),
            "shelterstone-forecast.json",
            "shelterstone",
        );
    }
    #[test]
    fn varied_locations_match_poc_byte_for_byte() {
        golden(
            Location::new(
                "babbitt",
                "Babbitt",
                47.71,
                -91.94,
                Some(452),
                "America/Chicago",
            )
            .unwrap(),
            "babbitt-forecast.json",
            "babbitt",
        );
        golden(
            Location::new("alaska", "Alaska", 59.9, -150.15, None, "America/Anchorage").unwrap(),
            "alaska-forecast.json",
            "alaska",
        );
        golden(
            Location::new("ongole", "Ongole", 15.51, 80.05, None, "Asia/Kolkata").unwrap(),
            "ongole-forecast.json",
            "ongole",
        );
    }
}
