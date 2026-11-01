//! How results are printed: tables and sentences for people, the API's JSON for scripts.
//! Results go to stdout; progress and logs that accompany them go to stderr.

use std::io::{IsTerminal, Write};

use jiff::Timestamp;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Tables and sentences.
    #[default]
    Table,
    /// The API's JSON: the resource, or an array of every item for lists.
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Bold,
    Dim,
    Green,
    Red,
    Yellow,
}

#[derive(Debug, Clone, Copy)]
pub struct Output {
    pub format: Format,
    /// No messages or progress: only results and errors.
    pub quiet: bool,
    stdout_color: bool,
    stderr_color: bool,
}

impl Output {
    /// Colors only on terminals, and never with `--no-color` or `NO_COLOR`.
    pub fn new(format: Format, quiet: bool, no_color: bool) -> Self {
        let allowed = !no_color && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        Self {
            format,
            quiet,
            stdout_color: allowed && std::io::stdout().is_terminal(),
            stderr_color: allowed && std::io::stderr().is_terminal(),
        }
    }

    pub fn json(&self) -> bool {
        self.format == Format::Json
    }

    /// Prints `value` as pretty JSON on stdout.
    pub fn print_json(&self, value: &impl Serialize) -> anyhow::Result<()> {
        let mut text = serde_json::to_string_pretty(value)?;
        text.push('\n');
        write_out(&text)
    }

    /// A result line on stdout, for people (skipped with `--quiet`).
    pub fn done(&self, message: &str) -> anyhow::Result<()> {
        if self.quiet { Ok(()) } else { write_out(&format!("{message}\n")) }
    }

    /// A line of output on stdout, such as a log line: the result itself, so even with `--quiet`.
    pub fn line(&self, text: &str) -> anyhow::Result<()> {
        write_out(&format!("{text}\n"))
    }

    /// A line of progress on stderr (skipped with `--quiet`).
    pub fn info(&self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }

    /// A warning on stderr, even with `--quiet`.
    pub fn warn(&self, message: &str) {
        eprintln!("{} {message}", paint(self.stderr_color, "warning:", Style::Yellow));
    }

    pub fn paint(&self, text: &str, style: Style) -> String {
        paint(self.stdout_color, text, style)
    }

    pub fn paint_err(&self, text: &str, style: Style) -> String {
        paint(self.stderr_color, text, style)
    }

    /// A table with a header row, columns padded to their widest cell.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> anyhow::Result<()> {
        write_out(&self.render_table(headers, rows))
    }

    fn render_table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let width = |text: &str| visible_len(text);
        let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(width(cell));
            }
        }
        let mut text = String::new();
        let mut line = |cells: Vec<String>| {
            let last = cells.len() - 1;
            for (index, cell) in cells.iter().enumerate() {
                text.push_str(cell);
                if index < last {
                    text.push_str(&" ".repeat(widths[index] - width(cell) + 2));
                }
            }
            text.push('\n');
        };
        line(headers.iter().map(|header| self.paint(header, Style::Dim)).collect());
        for row in rows {
            line(row.clone());
        }
        text
    }
}

fn write_out(text: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush()) {
        // A closed pipe (`spinikus … | head`) is not an error.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

fn paint(color: bool, text: &str, style: Style) -> String {
    if !color {
        return text.to_string();
    }
    let code = match style {
        Style::Bold => "1",
        Style::Dim => "2",
        Style::Green => "32",
        Style::Red => "31",
        Style::Yellow => "33",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// Length without ANSI color codes.
fn visible_len(text: &str) -> usize {
    let mut length = 0;
    let mut in_escape = false;
    for char in text.chars() {
        match (in_escape, char) {
            (false, '\x1b') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, _) => length += 1,
        }
    }
    length
}

/// A status in its color: green when it went well, red when it failed, yellow under way.
pub fn status_style(status: &str) -> Style {
    match status {
        "live" | "succeeded" => Style::Green,
        "failed" | "canceled" => Style::Red,
        "superseded" | "draining" => Style::Dim,
        _ => Style::Yellow,
    }
}

/// An API timestamp as "5m ago", or the date for anything older than a week.
pub fn ago(timestamp: &str) -> String {
    let Ok(at) = timestamp.parse::<Timestamp>() else { return timestamp.to_string() };
    ago_from(at, Timestamp::now())
}

fn ago_from(at: Timestamp, now: Timestamp) -> String {
    let seconds = now.as_second() - at.as_second();
    match seconds {
        ..0 => at.strftime("%Y-%m-%d %H:%M").to_string(),
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ => at.strftime("%Y-%m-%d").to_string(),
    }
}

/// A byte count in the largest unit that keeps it at least 1.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_pad_columns_to_the_widest_cell() {
        let output = Output { format: Format::Table, quiet: false, stdout_color: false, stderr_color: false };
        let rows = vec![vec!["shop".to_string(), "Shop".to_string()], vec!["blog-site".to_string(), "Blog".to_string()]];
        assert_eq!(output.render_table(&["SLUG", "NAME"], &rows), "SLUG       NAME\nshop       Shop\nblog-site  Blog\n");
        let colored = paint(true, "live", Style::Green);
        assert_eq!(visible_len(&colored), 4);
    }

    #[test]
    fn times_and_sizes_read_naturally() {
        let now: Timestamp = "2026-11-01T12:00:00Z".parse().unwrap();
        let at = |text: &str| ago_from(text.parse().unwrap(), now);
        assert_eq!(at("2026-11-01T11:59:30Z"), "just now");
        assert_eq!(at("2026-11-01T11:15:00Z"), "45m ago");
        assert_eq!(at("2026-10-30T12:00:00Z"), "2d ago");
        assert_eq!(at("2026-09-01T12:00:00Z"), "2026-09-01");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(12_345_678), "12.3 MB");
    }
}
