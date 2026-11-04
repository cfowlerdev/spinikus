//! Line charts of metrics, drawn on the server as inline SVG: no script, so the CSP holds.
//! Each chart is one measure on one axis. The plot stretches to its box (strokes keep their
//! width with `non-scaling-stroke`); the axis labels are HTML beside it, in text colors.
//! Hovering a period shows its values (the browser's own tooltip, from `<title>`), and
//! each chart has a table of its values.

use jiff::{SignedDuration, Timestamp};
use maud::{Markup, html};
use spinikus_core::metrics::format_bytes;

const WIDTH: f64 = 600.0;
const HEIGHT: f64 = 160.0;
/// Room above the highest value, so a line at the top isn't cut in half.
const HEADROOM: f64 = 4.0;

/// What a chart's values are.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Cores,
    Bytes,
    /// Bytes per second.
    Rate,
}

impl Unit {
    pub fn format(self, value: f64) -> String {
        match self {
            Self::Cores => match value {
                value if value < 10.0 => format!("{value:.2}"),
                value => format!("{value:.1}"),
            },
            Self::Bytes => format_bytes(value.max(0.0).round() as u64),
            Self::Rate => format!("{}/s", format_bytes(value.max(0.0).round() as u64)),
        }
    }

    /// The top of the axis: a round number at or above `max`.
    fn top(self, max: f64) -> f64 {
        match self {
            Self::Cores => nice(max.max(0.1)),
            Self::Bytes | Self::Rate => {
                let mut unit = 1.0;
                while max / unit >= 1024.0 {
                    unit *= 1024.0;
                }
                nice((max / unit).max(1.0)) * unit
            }
        }
    }
}

/// The smallest of 1, 2 or 5 times a power of ten that is at least `value`.
fn nice(value: f64) -> f64 {
    let power = 10f64.powf(value.log10().floor());
    let mantissa = value / power;
    let step = [1.0, 2.0, 5.0, 10.0].into_iter().find(|step| mantissa <= *step + 1e-9).unwrap_or(10.0);
    step * power
}

/// A series' hue: the palette's first two categorical slots, light and dark steps. Text
/// never wears them; only lines, fills and legend swatches do.
#[derive(Clone, Copy)]
pub enum Hue {
    Blue,
    Orange,
}

impl Hue {
    fn line(self) -> &'static str {
        match self {
            Self::Blue => "stroke-[#2a78d6] dark:stroke-[#3987e5]",
            Self::Orange => "stroke-[#eb6834] dark:stroke-[#d95926]",
        }
    }

    fn wash(self) -> &'static str {
        match self {
            Self::Blue => "fill-[#2a78d6]/10 dark:fill-[#3987e5]/10",
            Self::Orange => "fill-[#eb6834]/10 dark:fill-[#d95926]/10",
        }
    }

    fn swatch(self) -> &'static str {
        match self {
            Self::Blue => "bg-[#2a78d6] dark:bg-[#3987e5]",
            Self::Orange => "bg-[#eb6834] dark:bg-[#d95926]",
        }
    }
}

pub struct Series<'a> {
    pub label: &'a str,
    pub hue: Hue,
    /// Each period's value by the period's end, oldest first.
    pub points: Vec<(Timestamp, f64)>,
}

pub struct Chart<'a> {
    pub title: &'a str,
    pub unit: Unit,
    pub since: Timestamp,
    pub until: Timestamp,
    /// How long each point covers.
    pub step: SignedDuration,
    pub series: Vec<Series<'a>>,
    /// The axis's top when there is a natural one, such as a disk's size.
    pub ceiling: Option<f64>,
    /// What the latest value is out of, said after it: "of 100 GiB".
    pub of: Option<String>,
}

impl Chart<'_> {
    pub fn render(&self) -> Markup {
        let max = self.series.iter().flat_map(|series| series.points.iter().map(|(_, value)| *value)).fold(0.0, f64::max);
        let top = match self.ceiling {
            Some(ceiling) if ceiling > 0.0 => ceiling,
            _ => self.unit.top(max),
        };
        let empty = self.series.iter().all(|series| series.points.is_empty());
        let many = self.series.len() > 1;
        html! {
            section class="space-y-3" {
                div class="flex flex-wrap items-baseline gap-x-4 gap-y-1" {
                    h3 class="text-sm font-semibold" { (self.title) }
                    // The latest value of each series, keyed by its swatch when there are several.
                    div class="ml-auto flex flex-wrap gap-x-4 gap-y-1 text-sm" {
                        @for series in &self.series {
                            span class="inline-flex items-center gap-1.5" {
                                @if many { span class={ "inline-block h-0.5 w-3 rounded-full " (series.hue.swatch()) } {} }
                                @if many { span class="text-zinc-500 dark:text-zinc-400" { (series.label) } }
                                @match series.points.last() {
                                    Some((_, value)) => span class="font-medium tabular-nums" {
                                        (self.unit.format(*value))
                                        @if let Some(of) = &self.of { span class="font-normal text-zinc-500 dark:text-zinc-400" { " " (of) } }
                                    },
                                    None => span class="text-zinc-500 dark:text-zinc-400" { "–" },
                                }
                            }
                        }
                    }
                }
                div class="flex gap-2" {
                    div class="flex h-40 w-16 shrink-0 flex-col justify-between text-right text-xs tabular-nums text-zinc-500 dark:text-zinc-400" {
                        span class="-mt-2" { (self.unit.format(top)) }
                        span { (self.unit.format(top / 2.0)) }
                        span class="-mb-2" { (self.unit.format(0.0)) }
                    }
                    div class="relative min-w-0 flex-1" {
                        (self.plot(top))
                        @if empty {
                            p class="absolute inset-0 flex items-center justify-center text-sm text-zinc-500 dark:text-zinc-400" {
                                "Nothing measured in this range."
                            }
                        }
                        div class="mt-1 flex justify-between text-xs tabular-nums text-zinc-500 dark:text-zinc-400" {
                            span { (self.time_label(self.since)) }
                            span { (self.time_label(self.since + (self.until.duration_since(self.since) / 2))) }
                            span { (self.time_label(self.until)) }
                        }
                    }
                }
                @if !empty { (self.table()) }
            }
        }
    }

    fn plot(&self, top: f64) -> Markup {
        let span = self.until.duration_since(self.since).as_secs_f64().max(1.0);
        let x = |at: Timestamp| (at.duration_since(self.since).as_secs_f64() / span * WIDTH).clamp(0.0, WIDTH);
        let y = |value: f64| HEIGHT - (value / top).clamp(0.0, 1.0) * (HEIGHT - HEADROOM);
        let periods = self.periods();
        html! {
            svg viewBox={ "0 0 " (WIDTH) " " (HEIGHT) } preserveAspectRatio="none" role="img" aria-label=(self.title)
                class="block h-40 w-full overflow-visible" {
                @for fraction in [0.0, 0.5, 1.0] {
                    @let line_y = y(top * fraction);
                    line x1="0" x2=(WIDTH) y1=(line_y) y2=(line_y) vector-effect="non-scaling-stroke"
                        class="stroke-zinc-200 dark:stroke-zinc-800" stroke-width="1" {}
                }
                @for series in &self.series {
                    @for segment in segments(&series.points, self.step) {
                        path d=(area(&segment, &x, &y, self.step)) class=(series.hue.wash()) stroke="none" {}
                        path d=(line(&segment, &x, &y, self.step)) fill="none" stroke-width="2" stroke-linejoin="round"
                            stroke-linecap="round" vector-effect="non-scaling-stroke" class=(series.hue.line()) {}
                    }
                }
                // Hover targets, one per period, wider than the line.
                @for (at, values) in &periods {
                    @let left = x(*at - self.step);
                    rect x=(left) y="0" width=((x(*at) - left).max(1.0)) height=(HEIGHT)
                        class="fill-transparent hover:fill-zinc-500/10" {
                        title { (self.time_label_full(*at)) ": " (self.values_text(values)) }
                    }
                }
            }
        }
    }

    /// Every period with a value, newest last, with each series' value in it.
    fn periods(&self) -> Vec<(Timestamp, Vec<Option<f64>>)> {
        let mut times: Vec<Timestamp> = self.series.iter().flat_map(|series| series.points.iter().map(|(at, _)| *at)).collect();
        times.sort();
        times.dedup();
        times
            .into_iter()
            .map(|at| {
                let values = self
                    .series
                    .iter()
                    .map(|series| series.points.iter().find(|(when, _)| *when == at).map(|(_, value)| *value))
                    .collect();
                (at, values)
            })
            .collect()
    }

    fn values_text(&self, values: &[Option<f64>]) -> String {
        let text = |value: &Option<f64>| value.map_or("–".to_string(), |value| self.unit.format(value));
        match self.series.len() {
            1 => text(&values[0]),
            _ => self
                .series
                .iter()
                .zip(values)
                .map(|(series, value)| format!("{} {}", series.label, text(value)))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    fn table(&self) -> Markup {
        let periods = self.periods();
        html! {
            details class="text-sm" {
                summary class="cursor-pointer text-xs text-zinc-500 select-none hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100" {
                    "Table"
                }
                div class="mt-2 max-h-64 overflow-y-auto rounded-md border border-zinc-200 dark:border-zinc-800" {
                    table class="w-full text-left text-xs tabular-nums" {
                        thead class="sticky top-0 bg-zinc-50 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-400" {
                            tr {
                                th class="px-3 py-1.5 font-medium" { "Until (UTC)" }
                                @for series in &self.series { th class="px-3 py-1.5 font-medium" { (series.label) } }
                            }
                        }
                        tbody class="divide-y divide-zinc-100 dark:divide-zinc-800" {
                            @for (at, values) in periods.iter().rev() {
                                tr {
                                    td class="px-3 py-1" { (self.time_label_full(*at)) }
                                    @for value in values {
                                        td class="px-3 py-1" { (value.map_or("–".to_string(), |value| self.unit.format(value))) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// A time on the axis: the time of day for a day or less, else the date.
    fn time_label(&self, at: Timestamp) -> String {
        match self.until.duration_since(self.since) <= SignedDuration::from_hours(24) {
            true => at.strftime("%H:%M").to_string(),
            false => at.strftime("%b %-d").to_string(),
        }
    }

    fn time_label_full(&self, at: Timestamp) -> String {
        at.strftime("%Y-%m-%d %H:%M").to_string()
    }
}

/// Runs of points without gaps: a period with nothing measured breaks the line.
fn segments(points: &[(Timestamp, f64)], step: SignedDuration) -> Vec<Vec<(Timestamp, f64)>> {
    let mut segments: Vec<Vec<(Timestamp, f64)>> = Vec::new();
    for point in points {
        match segments.last_mut() {
            Some(segment) if segment.last().is_some_and(|(at, _)| point.0.duration_since(*at) <= step + step / 2) => {
                segment.push(*point)
            }
            _ => segments.push(vec![*point]),
        }
    }
    segments
}

/// The line through a segment's period ends; a lone point spans its period.
fn line(segment: &[(Timestamp, f64)], x: &impl Fn(Timestamp) -> f64, y: &impl Fn(f64) -> f64, step: SignedDuration) -> String {
    let mut path = String::new();
    if let [(at, value)] = segment {
        return format!("M{:.1},{:.1}L{:.1},{:.1}", x(*at - step), y(*value), x(*at), y(*value));
    }
    for (index, (at, value)) in segment.iter().enumerate() {
        let command = if index == 0 { 'M' } else { 'L' };
        path.push_str(&format!("{command}{:.1},{:.1}", x(*at), y(*value)));
    }
    path
}

/// The wash under a segment's line, down to the axis.
fn area(segment: &[(Timestamp, f64)], x: &impl Fn(Timestamp) -> f64, y: &impl Fn(f64) -> f64, step: SignedDuration) -> String {
    let (first, last) = (segment[0].0, segment[segment.len() - 1].0);
    let start = if segment.len() == 1 { x(first - step) } else { x(first) };
    let line = line(segment, x, y, step);
    format!("M{start:.1},{HEIGHT:.1}L{}L{:.1},{HEIGHT:.1}Z", &line[1..], x(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axes_end_on_round_numbers() {
        assert_eq!(Unit::Cores.top(0.0), 0.1);
        assert_eq!(Unit::Cores.top(0.42), 0.5);
        assert_eq!(Unit::Cores.top(1.2), 2.0);
        assert_eq!(Unit::Cores.top(3.0), 5.0);
        assert_eq!(Unit::Bytes.top(300.0 * 1024.0 * 1024.0), 500.0 * 1024.0 * 1024.0);
        assert_eq!(Unit::Rate.top(0.0), 1.0);
        assert_eq!(Unit::Rate.format(1536.0), "1.5 KiB/s");
    }

    #[test]
    fn gaps_break_the_line() {
        let at = |minutes: i64| "2026-11-04T10:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_mins(minutes);
        let points = [(at(1), 1.0), (at(2), 2.0), (at(5), 3.0)];
        let segments = segments(&points, SignedDuration::from_mins(1));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1], [(at(5), 3.0)]);
    }
}
