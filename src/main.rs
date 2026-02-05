use std::collections::VecDeque;
use std::io;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::mpsc::{self};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::*;
use ratatui::text::Line;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, Gauge, Paragraph};

/// Default polling interval in milliseconds.
const DEFAULT_POLL_MS: u64 = 1000;
/// Default history window in seconds.
const DEFAULT_WINDOW_SECS: u64 = 5 * 60;

/// Parsed ccache statistics from the latest successful poll.
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    /// Total cacheable calls reported by ccache.
    cacheable_calls: u64,
    /// Total cache hits reported by ccache.
    hits: u64,
    /// Total cache misses reported by ccache.
    misses: u64,
    /// Total uncacheable calls reported by ccache.
    uncacheable_calls: u64,
    /// Current cache size in GB, if present in output.
    cache_size_used_gb: Option<f64>,
    /// Maximum cache size in GB, if present in output.
    cache_size_max_gb: Option<f64>,
}

/// Time-series sample used for charts.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Timestamp when the sample was captured.
    timestamp: Instant,
    /// Total hits at sample time.
    hits: u64,
    /// Total misses at sample time.
    misses: u64,
    /// Hit rate ratio at sample time (0.0 - 1.0).
    hit_rate: f64,
    /// Cache size in GB at sample time, if available.
    cache_size_used_gb: Option<f64>,
    /// Cache size limit in GB at sample time, if available.
    cache_size_max_gb: Option<f64>,
}

/// Captures a ccache invocation failure and its output.
#[derive(Debug, Clone)]
struct CcacheError {
    /// Process exit code if available.
    code: Option<i32>,
    /// Trimmed stderr/stdout or parse error message.
    message: String,
}

/// Command-line arguments for configuring the UI and poller.
#[derive(Debug, Parser)]
#[command(name = "ccache-graph", version, about = "Live ccache stats and trends")]
struct Args {
    /// History window length in seconds.
    #[arg(long, default_value_t = DEFAULT_WINDOW_SECS)]
    window_seconds: u64,
    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = DEFAULT_POLL_MS)]
    poll_ms: u64,
    /// Visual theme for chart and gauge colors.
    #[arg(long, value_enum, default_value_t = ThemeName::Classic)]
    theme: ThemeName,
}

/// Named color themes for the UI.
#[derive(Debug, Copy, Clone, ValueEnum)]
enum ThemeName {
    Classic,
    Mono,
    Solar,
}

/// Color palette used for charts and gauges.
#[derive(Debug, Clone, Copy)]
struct Theme {
    hits: Color,
    misses: Color,
    hit_rate: Color,
    cache: Color,
    hit_gauge: Color,
    cache_gauge: Color,
}

impl ThemeName {
    /// Maps theme names to concrete colors.
    fn to_theme(self) -> Theme {
        match self {
            ThemeName::Classic => Theme {
                hits: Color::Green,
                misses: Color::Red,
                hit_rate: Color::Cyan,
                cache: Color::Yellow,
                hit_gauge: Color::Green,
                cache_gauge: Color::Blue,
            },
            ThemeName::Mono => Theme {
                hits: Color::White,
                misses: Color::Gray,
                hit_rate: Color::White,
                cache: Color::Gray,
                hit_gauge: Color::White,
                cache_gauge: Color::Gray,
            },
            ThemeName::Solar => Theme {
                hits: Color::Yellow,
                misses: Color::Magenta,
                hit_rate: Color::Cyan,
                cache: Color::Blue,
                hit_gauge: Color::Yellow,
                cache_gauge: Color::Blue,
            },
        }
    }
}

/// Restores terminal state on drop, including raw mode and alternate screen.
struct TerminalCleanup;

impl TerminalCleanup {
    /// Installs cursor state changes required for TUI mode.
    fn install() -> io::Result<Self> {
        execute!(io::stdout(), Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
    }
}

/// Entry point that initializes the terminal and runs the TUI loop.
fn main() -> io::Result<()> {
    let args = Args::parse();
    let window = Duration::from_secs(args.window_seconds.max(1));
    let poll_interval = Duration::from_millis(args.poll_ms.max(1));
    let theme = args.theme.to_theme();

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    let _cleanup = TerminalCleanup::install()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut history: VecDeque<Sample> = VecDeque::new();
    let mut stats = Stats::default();
    let mut last_error: Option<CcacheError> = None;

    let (result_tx, result_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker = thread::spawn(move || poll_ccache(result_tx, stop_rx, poll_interval));

    let run_result = run_app(
        &mut terminal,
        &mut history,
        &mut stats,
        &mut last_error,
        result_rx,
        window,
        theme,
    );

    let _ = stop_tx.send(());
    let _ = worker.join();

    run_result
}

/// Runs the main event loop: render, handle input, and poll ccache.
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    history: &mut VecDeque<Sample>,
    stats: &mut Stats,
    last_error: &mut Option<CcacheError>,
    result_rx: Receiver<Result<Stats, CcacheError>>,
    window: Duration,
    theme: Theme,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| render(frame, *stats, history, last_error.as_ref(), window, theme))?;

        let timeout = Duration::from_millis(100);
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
            }
        }

        while let Ok(result) = result_rx.try_recv() {
            match result {
                Ok(new_stats) => {
                    *stats = new_stats;
                    *last_error = None;
                    push_sample(history, *stats, window);
                }
                Err(err) => {
                    *last_error = Some(err);
                }
            }
        }
    }

    Ok(())
}

/// Background poller that runs ccache periodically and sends results to the UI thread.
fn poll_ccache(
    result_tx: Sender<Result<Stats, CcacheError>>,
    stop_rx: Receiver<()>,
    poll_interval: Duration,
) {
    let mut next_tick = Instant::now();
    loop {
        let now = Instant::now();
        if now < next_tick {
            let timeout = next_tick - now;
            if stop_rx.recv_timeout(timeout).is_ok() {
                break;
            }
        } else if stop_rx.try_recv().is_ok() {
            break;
        }

        let result = read_ccache_stats();
        let _ = result_tx.send(result);
        next_tick += poll_interval;
    }
}

/// Executes `ccache -s` and parses stats or returns a structured error.
fn read_ccache_stats() -> Result<Stats, CcacheError> {
    let output = Command::new("ccache").arg("-s").output().map_err(|err| CcacheError {
        code: None,
        message: err.to_string(),
    })?;
    if !output.status.success() {
        let mut message = String::from_utf8_lossy(&output.stderr).to_string();
        if message.trim().is_empty() {
            message = String::from_utf8_lossy(&output.stdout).to_string();
        }
        return Err(CcacheError {
            code: output.status.code(),
            message: message.trim().to_string(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_ccache_stats(&text).ok_or_else(|| CcacheError {
        code: output.status.code(),
        message: "Failed to parse ccache output".to_string(),
    })
}

/// Parses ccache stats output into a Stats struct.
fn parse_ccache_stats(input: &str) -> Option<Stats> {
    let mut stats = Stats::default();
    let mut in_cacheable = false;
    let mut in_local = false;
    let mut saw_cacheable = false;
    let mut saw_hits = false;
    let mut saw_misses = false;

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("Cacheable calls:") {
            in_cacheable = true;
            in_local = false;
            if let Some((count, _total, _pct)) = parse_two_ints_and_percent(trimmed) {
                stats.cacheable_calls = count;
                saw_cacheable = true;
            }
            continue;
        }

        if trimmed.starts_with("Uncacheable calls:") {
            in_cacheable = false;
            in_local = false;
            if let Some((count, _total, _pct)) = parse_two_ints_and_percent(trimmed) {
                stats.uncacheable_calls = count;
            }
            continue;
        }

        if trimmed.starts_with("Local storage:") {
            in_local = true;
            in_cacheable = false;
            continue;
        }

        if in_cacheable && trimmed.starts_with("Hits:") {
            if let Some((count, _total, _pct)) = parse_two_ints_and_percent(trimmed) {
                stats.hits = count;
                saw_hits = true;
            }
            continue;
        }

        if in_cacheable && trimmed.starts_with("Misses:") {
            if let Some((count, _total, _pct)) = parse_two_ints_and_percent(trimmed) {
                stats.misses = count;
                saw_misses = true;
            }
            continue;
        }

        if in_local
            && (trimmed.starts_with("Cache size (GB):") || trimmed.starts_with("Cache size (GiB):"))
        {
            if let Some((used, max, _pct)) = parse_two_floats_and_percent(trimmed) {
                stats.cache_size_used_gb = Some(used);
                stats.cache_size_max_gb = Some(max);
            }
            continue;
        }
    }

    if saw_cacheable && saw_hits && saw_misses {
        Some(stats)
    } else {
        None
    }
}

/// Extracts two integers and a percent from a stats line.
fn parse_two_ints_and_percent(line: &str) -> Option<(u64, u64, f64)> {
    let cleaned: String = line
        .chars()
        .map(|c| if c.is_ascii_digit() || c == '.' { c } else { ' ' })
        .collect();
    let mut parts = cleaned.split_whitespace();
    let first = parts.next()?.parse::<u64>().ok()?;
    let second = parts.next()?.parse::<u64>().ok()?;
    let third = parts
        .next()
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or_else(|| {
            if second > 0 {
                (first as f64) / (second as f64) * 100.0
            } else {
                0.0
            }
        });
    Some((first, second, third))
}

/// Extracts two floats and a percent from a stats line.
fn parse_two_floats_and_percent(line: &str) -> Option<(f64, f64, f64)> {
    let cleaned: String = line
        .chars()
        .map(|c| if c.is_ascii_digit() || c == '.' { c } else { ' ' })
        .collect();
    let mut parts = cleaned.split_whitespace();
    let first = parts.next()?.parse::<f64>().ok()?;
    let second = parts.next()?.parse::<f64>().ok()?;
    let third = parts
        .next()
        .and_then(|p| p.parse::<f64>().ok())
        .unwrap_or_else(|| {
            if second > 0.0 {
                (first / second) * 100.0
            } else {
                0.0
            }
        });
    Some((first, second, third))
}

/// Adds a new sample to the history ring buffer.
fn push_sample(history: &mut VecDeque<Sample>, stats: Stats, window: Duration) {
    let now = Instant::now();
    let hit_rate = if stats.cacheable_calls > 0 {
        stats.hits as f64 / stats.cacheable_calls as f64
    } else {
        0.0
    };

    history.push_back(Sample {
        timestamp: now,
        hits: stats.hits,
        misses: stats.misses,
        hit_rate,
        cache_size_used_gb: stats.cache_size_used_gb,
        cache_size_max_gb: stats.cache_size_max_gb,
    });
    trim_history(history, window);
}

/// Renders the full UI layout.
fn render(
    frame: &mut Frame,
    stats: Stats,
    history: &VecDeque<Sample>,
    error: Option<&CcacheError>,
    window: Duration,
    theme: Theme,
) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Length(12), Constraint::Min(8)])
        .split(area);

    render_header(frame, rows[0], stats, history, theme);
    render_hits_misses_chart(frame, rows[1], history, window, theme);
    render_secondary_charts(frame, rows[2], history, window, theme);

    if let Some(err) = error {
        render_error_overlay(frame, area, err);
    }
}

/// Renders the top header row with gauges and summary info.
fn render_header(frame: &mut Frame, area: Rect, stats: Stats, history: &VecDeque<Sample>, theme: Theme) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(45), Constraint::Percentage(30)])
        .split(area);

    let hit_rate = if stats.cacheable_calls > 0 {
        stats.hits as f64 / stats.cacheable_calls as f64
    } else {
        0.0
    };
    let hit_rate_pct = hit_rate * 100.0;
    let gauge = Gauge::default()
        .block(Block::default().title("Hit Rate").borders(Borders::ALL))
        .gauge_style(Style::default().fg(theme.hit_gauge))
        .ratio(hit_rate)
        .label(format!("{hit_rate_pct:.2}%"));
    frame.render_widget(gauge, cols[0]);

    let avg_hit_rate_1m = average_hit_rate_window(history, Duration::from_secs(60)) * 100.0;
    let summary = vec![
        format!("Cacheable calls:           {}", fmt_u64(stats.cacheable_calls)),
        format!("Hits:                      {}", fmt_u64(stats.hits)),
        format!("Misses:                    {}", fmt_u64(stats.misses)),
        format!("Uncacheable:               {}", fmt_u64(stats.uncacheable_calls)),
        format!("Avg hit rate (last 1 min): {avg_hit_rate_1m:.2}%"),
    ]
    .join("\n");
    let summary_widget =
        Paragraph::new(summary).block(Block::default().title("Summary").borders(Borders::ALL));
    frame.render_widget(summary_widget, cols[1]);

    let cache_widget = if let (Some(used), Some(max)) = (stats.cache_size_used_gb, stats.cache_size_max_gb) {
        let ratio = if max > 0.0 { used / max } else { 0.0 };
        Gauge::default()
            .block(Block::default().title("Cache Size").borders(Borders::ALL))
            .gauge_style(Style::default().fg(theme.cache_gauge))
            .ratio(ratio)
            .label(format!("{used:.1} / {max:.1} GB"))
    } else {
        Gauge::default()
            .block(Block::default().title("Cache Size").borders(Borders::ALL))
            .gauge_style(Style::default().fg(theme.cache_gauge))
            .ratio(0.0)
            .label("n/a")
    };
    frame.render_widget(cache_widget, cols[2]);
}

/// Renders the hits vs misses per-second chart.
fn render_hits_misses_chart(
    frame: &mut Frame,
    area: Rect,
    history: &VecDeque<Sample>,
    window: Duration,
    theme: Theme,
) {
    let (hits_series, misses_series) = delta_series(history);
    let max_y = hits_series
        .iter()
        .chain(misses_series.iter())
        .map(|(_, y)| *y)
        .fold(0.0, f64::max)
        .max(1.0);

    let (current_hits, current_misses) = current_deltas(history);
    let title = format!(
        "Hits vs Misses (per second) | {} | current H {} /s, M {} /s",
        window_label(window),
        fmt_u64(current_hits),
        fmt_u64(current_misses)
    );

    let datasets = vec![
        Dataset::default()
            .name("Hits / sec")
            .marker(ratatui::symbols::Marker::Braille)
            .style(Style::default().fg(theme.hits))
            .data(&hits_series),
        Dataset::default()
            .name("Misses / sec")
            .marker(ratatui::symbols::Marker::Braille)
            .style(Style::default().fg(theme.misses))
            .data(&misses_series),
    ];

    let chart = Chart::new(datasets)
        .block(Block::default().title(title).borders(Borders::ALL))
        .x_axis(
            Axis::default()
                .bounds(x_bounds(history))
                .labels(x_axis_labels_time(history)),
        )
        .y_axis(Axis::default().bounds([0.0, max_y]).labels(vec![
            Line::from("0"),
            Line::from(format!("{:.0}", max_y / 2.0)),
            Line::from(format!("{:.0}", max_y)),
        ]));
    frame.render_widget(chart, area);
}

/// Renders secondary charts for hit rate and cache size trends.
fn render_secondary_charts(
    frame: &mut Frame,
    area: Rect,
    history: &VecDeque<Sample>,
    window: Duration,
    theme: Theme,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let hit_rate_series = history
        .iter()
        .map(|s| (sample_x(history, s), s.hit_rate * 100.0))
        .collect::<Vec<_>>();

    let current_hit_rate = history
        .back()
        .map(|s| s.hit_rate * 100.0)
        .unwrap_or(0.0);
    let hit_rate_title = format!(
        "Hit Rate Trend | {} | current {:.2}%",
        window_label(window),
        current_hit_rate
    );

    let hit_rate_chart = Chart::new(vec![Dataset::default()
        .name("Hit rate %")
        .style(Style::default().fg(theme.hit_rate))
        .marker(ratatui::symbols::Marker::Braille)
        .data(&hit_rate_series)])
        .block(Block::default().title(hit_rate_title).borders(Borders::ALL))
        .x_axis(
            Axis::default()
                .bounds(x_bounds(history))
                .labels(x_axis_labels_time(history)),
        )
        .y_axis(Axis::default().bounds([0.0, 100.0]).labels(vec![
            Line::from("0"),
            Line::from("50"),
            Line::from("100"),
        ]));
    frame.render_widget(hit_rate_chart, cols[0]);

    let current_cache_label = history
        .back()
        .and_then(|s| {
            if let (Some(used), Some(max)) = (s.cache_size_used_gb, s.cache_size_max_gb) {
                Some(format!("{used:.1} / {max:.1} GB"))
            } else if let Some(used) = s.cache_size_used_gb {
                Some(format!("{used:.1} GB"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| "n/a".to_string());
    let cache_title = format!(
        "Cache Size Trend | {} | current {current_cache_label}",
        window_label(window)
    );

    let cache_series = history
        .iter()
        .filter_map(|s| s.cache_size_used_gb.map(|used| (sample_x(history, s), used)))
        .collect::<Vec<_>>();
    let max_cache = cache_series
        .iter()
        .map(|(_, y)| *y)
        .fold(0.0, f64::max)
        .max(1.0);
    let cache_chart = Chart::new(vec![Dataset::default()
        .name("Cache size GB")
        .style(Style::default().fg(theme.cache))
        .marker(ratatui::symbols::Marker::Braille)
        .data(&cache_series)])
        .block(Block::default().title(cache_title).borders(Borders::ALL))
        .x_axis(
            Axis::default()
                .bounds(x_bounds(history))
                .labels(x_axis_labels_time(history)),
        )
        .y_axis(Axis::default().bounds([0.0, max_cache]).labels(vec![
            Line::from("0"),
            Line::from(format!("{:.1}", max_cache / 2.0)),
            Line::from(format!("{:.1}", max_cache)),
        ]));
    frame.render_widget(cache_chart, cols[1]);
}

/// Converts total hit/miss counters into per-second deltas.
fn delta_series(history: &VecDeque<Sample>) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let mut hits = Vec::with_capacity(history.len());
    let mut misses = Vec::with_capacity(history.len());

    let mut prev: Option<Sample> = None;
    for sample in history {
        if let Some(last) = prev {
            let hit_delta = sample.hits.saturating_sub(last.hits) as f64;
            let miss_delta = sample.misses.saturating_sub(last.misses) as f64;
            let x = sample_x(history, sample);
            hits.push((x, hit_delta));
            misses.push((x, miss_delta));
        } else {
            let x = sample_x(history, sample);
            hits.push((x, 0.0));
            misses.push((x, 0.0));
        }
        prev = Some(*sample);
    }

    (hits, misses)
}

/// Returns the latest per-second deltas for hits and misses.
fn current_deltas(history: &VecDeque<Sample>) -> (u64, u64) {
    if history.len() < 2 {
        return (0, 0);
    }
    let last = history[history.len() - 1];
    let prev = history[history.len() - 2];
    (
        last.hits.saturating_sub(prev.hits),
        last.misses.saturating_sub(prev.misses),
    )
}

/// Computes x-axis bounds from history.
fn x_bounds(history: &VecDeque<Sample>) -> [f64; 2] {
    if let (Some(first), Some(last)) = (history.front(), history.back()) {
        let span = last.timestamp.duration_since(first.timestamp).as_secs_f64();
        [0.0, span.max(1.0)]
    } else {
        [0.0, 1.0]
    }
}

/// Computes the average hit rate over a time window ending at the latest sample.
fn average_hit_rate_window(history: &VecDeque<Sample>, window: Duration) -> f64 {
    let Some(latest) = history.back() else {
        return 0.0;
    };
    let mut sum = 0.0;
    let mut count = 0usize;
    for sample in history.iter().rev() {
        if latest.timestamp.duration_since(sample.timestamp) > window {
            break;
        }
        sum += sample.hit_rate;
        count += 1;
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

/// Formats an integer with thousands separators.
fn fmt_u64(value: u64) -> String {
    let mut s = value.to_string();
    let mut i = s.len() as isize - 3;
    while i > 0 {
        s.insert(i as usize, ',');
        i -= 3;
    }
    s
}

/// Trims history to the configured time window.
fn trim_history(history: &mut VecDeque<Sample>, window: Duration) {
    let Some(latest_ts) = history.back().map(|s| s.timestamp) else {
        return;
    };
    while let Some(front) = history.front() {
        if latest_ts.duration_since(front.timestamp) > window {
            history.pop_front();
        } else {
            break;
        }
    }
}

/// Computes the X-axis coordinate in seconds since the oldest sample.
fn sample_x(history: &VecDeque<Sample>, sample: &Sample) -> f64 {
    if let Some(first) = history.front() {
        sample.timestamp.duration_since(first.timestamp).as_secs_f64()
    } else {
        0.0
    }
}

/// Builds X-axis labels with "time ago" semantics (max -> 0).
fn x_axis_labels_time(history: &VecDeque<Sample>) -> Vec<Line<'static>> {
    if history.is_empty() {
        return vec![Line::from("1m ago"), Line::from("30s ago"), Line::from("now")];
    }
    let bounds = x_bounds(history);
    let max_secs = bounds[1].max(1.0);
    let mid_secs = max_secs / 2.0;
    if max_secs < 120.0 {
        vec![
            Line::from(format!("{:.0}s ago", max_secs)),
            Line::from(format!("{:.0}s ago", mid_secs)),
            Line::from("now"),
        ]
    } else {
        vec![
            Line::from(format!("{:.1}m ago", max_secs / 60.0)),
            Line::from(format!("{:.1}m ago", mid_secs / 60.0)),
            Line::from("now"),
        ]
    }
}

/// Formats the window size for chart titles.
fn window_label(window: Duration) -> String {
    let secs = window.as_secs().max(1);
    if secs % 60 == 0 {
        format!("last {}m", secs / 60)
    } else {
        format!("last {}s", secs)
    }
}

/// Draws a centered error overlay when ccache invocation fails.
fn render_error_overlay(frame: &mut Frame, area: Rect, err: &CcacheError) {
    let overlay = centered_rect(80, 60, area);
    let mut lines = Vec::new();
    let code_line = match err.code {
        Some(code) => format!("Exit code: {code}"),
        None => "Exit code: n/a".to_string(),
    };
    lines.push(code_line);
    lines.push(String::new());
    if err.message.trim().is_empty() {
        lines.push("No error output captured.".to_string());
    } else {
        lines.extend(err.message.lines().map(|line| line.to_string()));
    }

    let block = Block::default()
        .title("CCACHE ERROR")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Red).bg(Color::Black));
    let paragraph = Paragraph::new(lines.join("\n"))
        .block(block)
        .style(Style::default().fg(Color::Red));
    frame.render_widget(paragraph, overlay);
}

/// Creates a centered rectangle as a percentage of the terminal size.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a representative ccache output and extracts core fields.
    #[test]
    fn parse_sample_output() {
        let sample = r#"Cacheable calls:   490357 / 575000 (85.28%)
  Hits:            122262 / 490357 (24.93%)
    Direct:        111785 / 122262 (91.43%)
    Preprocessed:   10477 / 122262 ( 8.57%)
  Misses:          368095 / 490357 (75.07%)
Uncacheable calls:  84643 / 575000 (14.72%)
Local storage:
  Cache size (GB):   15.0 /   15.0 (99.94%)
  Cleanups:          9053
  Hits:            122262 / 490357 (24.93%)
  Misses:          368095 / 490357 (75.07%)
"#;
        let stats = parse_ccache_stats(sample).expect("stats should parse");
        assert_eq!(stats.cacheable_calls, 490_357);
        assert_eq!(stats.hits, 122_262);
        assert_eq!(stats.misses, 368_095);
        assert_eq!(stats.uncacheable_calls, 84_643);
        assert_eq!(stats.cache_size_used_gb, Some(15.0));
        assert_eq!(stats.cache_size_max_gb, Some(15.0));
    }

    /// Rejects outputs missing required cacheable hit/miss fields.
    #[test]
    fn parse_requires_hits_and_misses() {
        let sample = r#"Cacheable calls:   10 / 20 (50.00%)
  Misses:          5 / 10 (50.00%)
"#;
        let stats = parse_ccache_stats(sample);
        assert!(stats.is_none());
    }

    /// Accepts GiB cache size output.
    #[test]
    fn parse_gib_cache_size() {
        let sample = r#"Cacheable calls:   10 / 20 (50.00%)
  Hits:            6 / 10 (60.00%)
  Misses:          4 / 10 (40.00%)
Local storage:
  Cache size (GiB):   12.0 /   16.0 (75.00%)
"#;
        let stats = parse_ccache_stats(sample).expect("stats should parse");
        assert_eq!(stats.cache_size_used_gb, Some(12.0));
        assert_eq!(stats.cache_size_max_gb, Some(16.0));
    }
}
