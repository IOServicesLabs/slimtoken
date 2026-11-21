//! Drawing. Every frame is a pure function of the `Show` state.

use crate::events::{Arm, CheckState, Kind, Phase};
use crate::state::{ArmState, Row, RowState, Show};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Chart, Clear, Dataset, GraphType, LegendPosition, Padding, Paragraph,
    Wrap,
};
use ratatui::Frame;

const BG: Color = Color::Rgb(11, 13, 18);
const PANEL: Color = Color::Rgb(16, 19, 26);
const TEXT: Color = Color::Rgb(222, 226, 235);
const DIM: Color = Color::Rgb(124, 132, 150);
const FAINT: Color = Color::Rgb(58, 64, 80);
const CODE: Color = Color::Rgb(150, 160, 182);
const RED: Color = Color::Rgb(255, 99, 99);
const BLUE: Color = Color::Rgb(122, 170, 255);
const SINGLE: Color = Color::Rgb(255, 178, 82);
const SLIM: Color = Color::Rgb(70, 230, 160);

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the caller knows that the timeline does not.
pub struct View {
    /// "LIVE", "REPLAY" or "DEMO".
    pub mode: &'static str,
    pub results_open: bool,
    pub frame: usize,
    pub paused: bool,
    pub speed: f64,
    pub keys: String,
    /// Launch status of each app (by arm index): text and "is ready".
    pub launch: [Option<(String, bool)>; 2],
}

fn color_of(arm: Arm) -> Color {
    match arm {
        Arm::Single => SINGLE,
        Arm::Slim => SLIM,
    }
}

fn dimmed(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as u16 * 55 / 100) as u8,
            (g as u16 * 55 / 100) as u8,
            (b as u16 * 55 / 100) as u8,
        ),
        other => other,
    }
}

fn name_of(arm: Arm) -> &'static str {
    match arm {
        Arm::Single => "SINGLE STREAM",
        Arm::Slim => "SLIMTOKEN",
    }
}

pub fn fmt_int(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_tokens(value: f64) -> String {
    if value >= 10_000.0 {
        format!("{:.1}k", value / 1000.0)
    } else if value >= 1_000.0 {
        format!("{:.2}k", value / 1000.0)
    } else {
        format!("{value:.0}")
    }
}

fn clip_left(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let tail: String = text.chars().skip(count - (width - 1)).collect();
    format!("…{tail}")
}

fn clip_right(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let head: String = text.chars().take(width - 1).collect();
    format!("{head}…")
}

/// Three-row block digits.
fn glyph(ch: char) -> [&'static str; 3] {
    match ch {
        '0' => ["█▀█", "█ █", "▀▀▀"],
        '1' => ["▄█ ", " █ ", "▀▀▀"],
        '2' => ["▀▀█", "█▀▀", "▀▀▀"],
        '3' => ["▀▀█", " ▀█", "▀▀▀"],
        '4' => ["█ █", "▀▀█", "  ▀"],
        '5' => ["█▀▀", "▀▀█", "▀▀▀"],
        '6' => ["█▀▀", "█▀█", "▀▀▀"],
        '7' => ["▀▀█", "  █", "  ▀"],
        '8' => ["█▀█", "█▀█", "▀▀▀"],
        '9' => ["█▀█", "▀▀█", "▀▀▀"],
        '.' => [" ", " ", "▀"],
        'x' => ["   ", "▀▄▀", "▀ ▀"],
        '-' => ["   ", "▀▀▀", "   "],
        _ => ["  ", "  ", "  "],
    }
}

fn big(text: &str) -> [String; 3] {
    let mut rows = [String::new(), String::new(), String::new()];
    for (index, ch) in text.chars().enumerate() {
        let cell = glyph(ch);
        for (row, part) in rows.iter_mut().zip(cell) {
            if index > 0 {
                row.push(' ');
            }
            row.push_str(part);
        }
    }
    rows
}

fn big_width(text: &str) -> u16 {
    big(text)[0].chars().count() as u16
}

pub fn draw(frame: &mut Frame, show: &Show, view: &View) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), area);
    if area.width < 84 || area.height < 24 {
        let message = Paragraph::new(vec![
            Line::from("SlimToken Showdown"),
            Line::from(format!(
                "The window is {}x{}. Make it at least 84x24.",
                area.width, area.height
            )),
        ])
        .alignment(Alignment::Center)
        .style(Style::new().fg(DIM).bg(BG));
        frame.render_widget(message, area);
        return;
    }

    let chart_height = match area.height {
        0..=29 => 0,
        30..=37 => 8,
        38..=47 => 10,
        _ => 12,
    };
    let [header, panes, chart, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(12),
        Constraint::Length(chart_height),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_header(frame, header, show, view);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panes);
    let scale = show
        .arms
        .iter()
        .flat_map(|arm| arm.rows.iter().map(|row| row.chars))
        .max()
        .unwrap_or(0)
        .max(2_400);
    draw_pane(frame, left, show, show.arm(Arm::Single), view, scale);
    draw_pane(frame, right, show, show.arm(Arm::Slim), view, scale);
    if chart_height > 0 {
        draw_chart(frame, chart, show);
    }
    draw_footer(frame, footer, show, view);
    if view.results_open {
        draw_results(frame, area, show);
    }
}

/// The opening screen: the prompt that both sides are about to get, in
/// full, before any clock starts. `remaining`: seconds until the start
/// (`None`: it waits for a key).
pub fn draw_intro(
    frame: &mut Frame,
    objective: &str,
    model: &str,
    provider: &str,
    simulated: bool,
    remaining: Option<f64>,
) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(BG)), area);
    let width = area.width.saturating_sub(4).min(108);
    let height = area.height;
    let card = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(vec![
            Span::styled(
                " THE PROMPT ",
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("· both sides get exactly this text ", Style::new().fg(DIM)),
        ]))
        .padding(Padding::new(3, 3, 1, 0))
        .style(Style::new().bg(PANEL));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(inner);
    let sides = Line::from(vec![
        Span::styled(
            "■ SINGLE STREAM",
            Style::new().fg(SINGLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  one request", Style::new().fg(DIM)),
        Span::styled("    vs    ", Style::new().fg(FAINT)),
        Span::styled(
            "■ SLIMTOKEN",
            Style::new().fg(SLIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  a plan, then parallel streams", Style::new().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(sides).alignment(Alignment::Center), rows[0]);
    let text: Vec<Line> = objective
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), Style::new().fg(TEXT))))
        .collect();
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), rows[1]);
    let start = match remaining {
        Some(left) => format!("starts in {}", left.ceil().max(1.0) as u64),
        None => "press Enter to start".to_string(),
    };
    let mut foot = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("model  ", Style::new().fg(FAINT)),
            Span::styled(format!("{model} ({provider})"), Style::new().fg(DIM)),
            Span::styled(
                "   ·   same model, same client, same checks",
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(vec![
            Span::styled(start, Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
            Span::styled("   ·   Enter start now · q quit", Style::new().fg(DIM)),
        ]),
    ];
    if simulated {
        foot.insert(
            1,
            Line::from(Span::styled(
                "SIMULATED DATA: not a measurement",
                Style::new().fg(RED).add_modifier(Modifier::BOLD),
            )),
        );
    }
    frame.render_widget(Paragraph::new(foot).alignment(Alignment::Center), rows[2]);
}

fn draw_header(frame: &mut Frame, area: Rect, show: &Show, view: &View) {
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(4),
        ..area
    };
    let (badge, badge_color) = match view.mode {
        "LIVE" => ("● LIVE".to_string(), SLIM),
        "DEMO" if inner.width >= 120 => (
            "▲ DEMO · SIMULATED DATA, NOT A MEASUREMENT".to_string(),
            RED,
        ),
        "DEMO" => ("▲ DEMO · SIMULATED DATA".to_string(), RED),
        _ => {
            let pace = if view.paused {
                "paused".to_string()
            } else {
                format!("{}x", view.speed)
            };
            (
                format!(
                    "◆ REPLAY of the live run {} · {pace}",
                    show.meta.recorded_at
                ),
                BLUE,
            )
        }
    };
    let model = format!("{} · {}   ", show.meta.provider, show.meta.model);
    let subtitle = "   one stream  vs  a parallel wave";
    let right_width = model.chars().count() + badge.chars().count();
    let room = (inner.width as usize).saturating_sub(21 + right_width + 2);
    let title = Line::from(vec![
        Span::styled("◢◤ ", Style::new().fg(SLIM)),
        Span::styled(
            "SLIMTOKEN SHOWDOWN",
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if room >= subtitle.chars().count() {
                subtitle
            } else {
                ""
            },
            Style::new().fg(DIM),
        ),
    ]);
    let right = Line::from(vec![
        Span::styled(
            if (inner.width as usize) >= 21 + right_width + 2 {
                model
            } else {
                String::new()
            },
            Style::new().fg(DIM),
        ),
        Span::styled(
            badge,
            Style::new().fg(badge_color).add_modifier(Modifier::BOLD),
        ),
    ])
    .alignment(Alignment::Right);
    let objective_width = inner.width.saturating_sub(11) as usize;
    let objective = Line::from(vec![
        Span::styled("objective  ", Style::new().fg(FAINT)),
        Span::styled(
            clip_right(
                &show.meta.objective.replace(['\n', '\r'], " "),
                objective_width,
            ),
            Style::new().fg(DIM),
        ),
    ]);
    let rows = Layout::vertical([Constraint::Length(1); 3]).split(inner);
    frame.render_widget(Paragraph::new(title), rows[0]);
    frame.render_widget(Paragraph::new(right), rows[0]);
    frame.render_widget(Paragraph::new(objective), rows[1]);
}

fn badge(arm: &ArmState, now: f64, frame: usize) -> (String, Color) {
    let color = color_of(arm.arm);
    let spin = SPINNER[frame % SPINNER.len()];
    match arm.phase {
        Phase::Waiting => ("waiting".to_string(), DIM),
        Phase::Planning => (format!("{spin} PLANNING"), BLUE),
        // Before the wave, the engine warms the server's prefix cache with
        // the contracts that every worker prompt starts with.
        Phase::Wave if !arm.streams.values().any(|stream| stream.kind == Kind::File) => {
            (format!("{spin} WARMING PREFIX CACHE"), BLUE)
        }
        Phase::Wave => (
            format!("{spin} WAVE · {} streams", arm.open_streams()),
            color,
        ),
        Phase::Streaming => (format!("{spin} STREAMING · 1 stream"), color),
        Phase::Verify => (format!("{spin} CHECK + REPAIR"), BLUE),
        Phase::Done => (format!("✓ DONE in {:.1}s", arm.elapsed(now)), color),
        Phase::Failed => ("✗ FAILED".to_string(), RED),
    }
}

fn draw_pane(
    frame: &mut Frame,
    area: Rect,
    show: &Show,
    arm: &ArmState,
    view: &View,
    scale: usize,
) {
    let color = color_of(arm.arm);
    let (status, status_color) = badge(arm, show.now, view.frame);
    let border = if arm.started.is_some() {
        dimmed(color)
    } else {
        FAINT
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .style(Style::new().bg(PANEL))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                name_of(arm.arm),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                match arm.arm {
                    Arm::Single => " · 1 request ",
                    Arm::Slim => " · plan + parallel wave ",
                },
                Style::new().fg(DIM),
            ),
        ]))
        .title(
            Line::from(Span::styled(
                format!(" {status} "),
                Style::new().fg(status_color).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tail_height = (inner.height.saturating_sub(9) / 3).clamp(3, 8);
    // The indexio line (SlimToken with `SHOWDOWN_INDEX=indexio` only).
    let tool_line = arm.tools.line().filter(|_| arm.arm == Arm::Slim);
    let [metrics, stats, files_head, files, tail_head, tail] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3 + u16::from(tool_line.is_some())),
        Constraint::Length(1),
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(tail_height),
    ])
    .areas(inner);

    draw_metrics(frame, metrics, show, arm);
    draw_stats(frame, stats, show, arm, view);

    let total = arm.rows.len();
    let head = Line::from(vec![
        Span::styled("FILES ", Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
        Span::styled(
            if total == 0 {
                "none yet".to_string()
            } else {
                format!("{} of {total} written", arm.files_done())
            },
            Style::new().fg(DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(head), files_head);
    // During verify, the last indexio lookups under the files.
    let helper = if arm.arm == Arm::Slim && arm.phase == Phase::Verify {
        arm.tools.rows.len().min(3) as u16
    } else {
        0
    };
    let [files, helpers] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(helper)]).areas(files);
    draw_files(frame, files, show, arm, view, scale);
    if helper > 0 {
        let width = helpers.width as usize;
        let rows: Vec<Line> = arm.tools.rows[arm.tools.rows.len() - helper as usize..]
            .iter()
            .map(|row| {
                Line::styled(
                    clip_right(&format!("⌕ {row}"), width),
                    Style::new().fg(BLUE),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), helpers);
    }
    draw_tail(frame, tail_head, tail, arm, view);
}

fn draw_metrics(frame: &mut Frame, area: Rect, show: &Show, arm: &ArmState) {
    let color = if arm.started.is_some() {
        color_of(arm.arm)
    } else {
        FAINT
    };
    let elapsed = arm.elapsed(show.now);
    let time_text = format!("{elapsed:.1}");
    let (rate, rate_caption) = if arm.finished() {
        let average = if elapsed > 0.0 {
            arm.output_tokens(show.chars_per_token) / elapsed
        } else {
            0.0
        };
        (average, "AVG TOKENS / SEC")
    } else {
        (arm.current_rate(), "TOKENS / SEC")
    };
    let rate_text = format!("{rate:.0}");
    let time_width = big_width(&time_text).max(7);
    let rate_width = big_width(&rate_text).max(16);
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(time_width + 1),
        Constraint::Min(1),
        Constraint::Length(rate_width),
    ])
    .areas(area);

    let style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    let mut time_lines: Vec<Line> = big(&time_text)
        .into_iter()
        .map(|row| Line::styled(row, style))
        .collect();
    time_lines.push(Line::styled("SECONDS", Style::new().fg(DIM)));
    frame.render_widget(Paragraph::new(time_lines), left);

    let mut rate_lines: Vec<Line> = big(&rate_text)
        .into_iter()
        .map(|row| Line::styled(row, style))
        .collect();
    rate_lines.push(Line::styled(rate_caption, Style::new().fg(DIM)));
    frame.render_widget(
        Paragraph::new(rate_lines).alignment(Alignment::Right),
        right,
    );
}

/// Quality line: one mark for each check step, then the app's address.
fn quality_line(arm: &ArmState, view: &View, width: usize) -> Option<Line<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    let mut push = |text: String, color: Color, bold: bool, spans: &mut Vec<Span<'static>>| {
        let room = width.saturating_sub(used);
        let text = clip_right(&text, room);
        used += text.chars().count();
        let style = Style::new().fg(color);
        spans.push(Span::styled(
            text,
            if bold {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        ));
    };
    if let Some(verify) = &arm.verified {
        let text = match (verify.before_failed, verify.after_failed) {
            (0, _) => "checked ✓ clean".to_string(),
            (before, 0) => format!("checked ✓ {before} fixed in {} files", verify.repaired),
            (before, after) => format!("checked: {before} → {after} problems"),
        };
        let color = if verify.after_failed == 0 {
            SLIM
        } else {
            SINGLE
        };
        push(text, color, false, &mut spans);
        push("  ".to_string(), DIM, false, &mut spans);
    }
    for check in &arm.checks {
        let (mark, color) = match check.state {
            CheckState::Running => (SPINNER[view.frame % SPINNER.len()], BLUE),
            CheckState::Pass => ("✓", SLIM),
            CheckState::Fail => ("✗", RED),
        };
        let detail = match (check.name.as_str(), check.state) {
            ("test" | "pages", CheckState::Pass | CheckState::Fail) if !check.tail.is_empty() => {
                let tail = check.tail.trim_start_matches("Tests ").trim();
                // "40 passed (40)": the total in brackets says nothing new.
                let tail = tail.rsplit_once(" (").map_or(tail, |(head, _)| head);
                format!(" {tail}")
            }
            _ => String::new(),
        };
        push(
            format!("{} {mark}{detail}", check.name),
            color,
            false,
            &mut spans,
        );
        push("  ".to_string(), DIM, false, &mut spans);
    }
    if let Some((text, ready)) = &view.launch[arm.arm.index()] {
        push(
            format!("▶ {text}"),
            if *ready { color_of(arm.arm) } else { BLUE },
            true,
            &mut spans,
        );
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

fn draw_stats(frame: &mut Frame, area: Rect, show: &Show, arm: &ArmState, view: &View) {
    let tokens = arm.output_tokens(show.chars_per_token);
    let approx = if arm.usage_exact() { "" } else { "~" };
    let mut parts = vec![format!("{approx}{} tokens out", fmt_int(tokens as u64))];
    parts.push(match arm.requests {
        1 => "1 request".to_string(),
        count => format!("{count} requests"),
    });
    if arm.arm == Arm::Slim {
        parts.push(format!("peak {} streams", arm.peak_streams));
        if let Some(plan) = arm.planning_seconds(show.now) {
            parts.push(format!("plan {plan:.1}s"));
        }
    }
    if arm.failures > 0 {
        parts.push(format!("{} retried", arm.failures));
    }
    if let Some(code) = arm.code_seconds() {
        parts.insert(0, format!("code done {code:.1}s"));
    }
    let mut lines = vec![Line::styled(
        clip_right(&parts.join(" · "), area.width as usize),
        Style::new().fg(TEXT),
    )];
    if let Some(outcome) = &arm.outcome {
        let note = outcome.note.clone().unwrap_or_default();
        let (text, color) = if outcome.ok {
            (
                format!(
                    "{} files · {} lines on disk{}",
                    outcome.files.len(),
                    fmt_int(arm.lines_written() as u64),
                    if note.is_empty() {
                        String::new()
                    } else {
                        format!(" · {note}")
                    }
                ),
                DIM,
            )
        } else {
            (
                format!(
                    "{} files · {}",
                    outcome.files.len(),
                    if note.is_empty() { "failed" } else { &note }
                ),
                RED,
            )
        };
        lines.push(Line::styled(
            clip_right(&text, area.width as usize),
            Style::new().fg(color),
        ));
    }
    if let Some(line) = quality_line(arm, view, area.width as usize) {
        lines.push(line);
    }
    if let Some(tools) = arm.tools.line().filter(|_| arm.arm == Arm::Slim) {
        lines.push(Line::styled(
            clip_right(&tools, area.width as usize),
            Style::new().fg(DIM),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn row_line(
    row: &Row,
    width: usize,
    path_width: usize,
    scale: usize,
    color: Color,
    frame: usize,
    chars_per_token: f64,
) -> Line<'static> {
    let (mark, mark_style) = match row.state {
        RowState::Queued => ("·", Style::new().fg(FAINT)),
        RowState::Rendered => ("◆", Style::new().fg(DIM)),
        RowState::Streaming => (
            SPINNER[frame % SPINNER.len()],
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        RowState::Done => ("✓", Style::new().fg(color)),
        RowState::Failed => ("✗", Style::new().fg(RED)),
    };
    let path_style = match row.state {
        RowState::Queued => Style::new().fg(FAINT),
        RowState::Streaming => Style::new().fg(TEXT),
        RowState::Failed => Style::new().fg(RED),
        _ => Style::new().fg(DIM),
    };
    let info = match row.state {
        RowState::Queued => String::new(),
        RowState::Rendered => "auto".to_string(),
        RowState::Failed => "retry".to_string(),
        _ => match row.tokens {
            Some(tokens) => fmt_tokens(tokens as f64),
            None => fmt_tokens(row.chars as f64 / chars_per_token),
        },
    };
    let info_width = 6;
    let bar_width = width.saturating_sub(2 + path_width + 1 + 1 + info_width);
    let filled = if bar_width == 0 {
        0
    } else {
        let cells = (row.chars as f64 / scale as f64 * bar_width as f64).ceil() as usize;
        match row.state {
            RowState::Queued | RowState::Rendered => 0,
            _ => cells.clamp(usize::from(row.chars > 0), bar_width),
        }
    };
    let bar_color = match row.state {
        RowState::Streaming => color,
        RowState::Failed => RED,
        _ => dimmed(color),
    };
    Line::from(vec![
        Span::styled(format!("{mark} "), mark_style),
        Span::styled(
            format!("{:<path_width$} ", clip_left(&row.path, path_width)),
            path_style,
        ),
        Span::styled("▆".repeat(filled), Style::new().fg(bar_color)),
        Span::raw(" ".repeat(bar_width - filled)),
        Span::styled(format!(" {info:>info_width$}"), Style::new().fg(DIM)),
    ])
}

fn draw_files(
    frame: &mut Frame,
    area: Rect,
    show: &Show,
    arm: &ArmState,
    view: &View,
    scale: usize,
) {
    if area.height == 0 {
        return;
    }
    let color = color_of(arm.arm);
    if arm.rows.is_empty() {
        let hint = match (arm.arm, arm.phase) {
            (_, Phase::Waiting) => "waiting to start",
            (Arm::Slim, Phase::Planning) => {
                "the architect writes the file plan and the interface contracts"
            }
            (Arm::Single, _) => "waiting for the first file in the answer",
            _ => "",
        };
        let mut lines = vec![Line::styled(hint, Style::new().fg(FAINT))];
        // While the architect plans, show its parallel planning calls.
        for stream in arm
            .streams
            .values()
            .filter(|stream| stream.kind == Kind::Plan)
            .take(area.height.saturating_sub(1) as usize)
        {
            let mark = if stream.open {
                Span::styled(
                    format!("{} ", SPINNER[view.frame % SPINNER.len()]),
                    Style::new().fg(BLUE),
                )
            } else {
                Span::styled("✓ ", Style::new().fg(dimmed(BLUE)))
            };
            let cells = (stream.chars / 60).min(area.width.saturating_sub(24) as usize);
            lines.push(Line::from(vec![
                mark,
                Span::styled("contracts  ", Style::new().fg(DIM)),
                Span::styled("▆".repeat(cells), Style::new().fg(dimmed(BLUE))),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let height = area.height as usize;
    let max_columns = if area.width >= 96 {
        3
    } else if area.width >= 56 {
        2
    } else {
        1
    };
    let columns = arm.rows.len().div_ceil(height).clamp(1, max_columns);
    let column_width = (area.width as usize - (columns - 1) * 2) / columns;
    let longest = arm
        .rows
        .iter()
        .map(|row| row.path.chars().count())
        .max()
        .unwrap_or(12);
    let path_width = longest.clamp(10, (column_width * 48 / 100).max(10));
    let capacity = height * columns;
    // When the list does not fit, keep the rows that are active in view.
    let first = if arm.rows.len() > capacity {
        let active = arm
            .rows
            .iter()
            .rposition(|row| row.state == RowState::Streaming)
            .unwrap_or(arm.rows.len() - 1);
        (active + 1).saturating_sub(capacity)
    } else {
        0
    };
    let visible = &arm.rows[first..(first + capacity).min(arm.rows.len())];
    let areas = Layout::horizontal(vec![Constraint::Length(column_width as u16); columns])
        .spacing(2)
        .split(area);
    for (column, chunk) in visible.chunks(height).enumerate() {
        let lines: Vec<Line> = chunk
            .iter()
            .map(|row| {
                row_line(
                    row,
                    column_width,
                    path_width,
                    scale,
                    color,
                    view.frame,
                    show.chars_per_token,
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), areas[column]);
    }
}

fn draw_tail(frame: &mut Frame, head: Rect, body: Rect, arm: &ArmState, view: &View) {
    let color = color_of(arm.arm);
    let stream = arm.focus_stream();
    let label = match stream {
        Some(stream) if stream.kind == Kind::Single => arm
            .rows
            .iter()
            .rfind(|row| row.state == RowState::Streaming)
            .map(|row| row.path.clone())
            .unwrap_or_else(|| stream.label.clone()),
        Some(stream) => stream.label.clone(),
        None => String::new(),
    };
    let rule_width = (head.width as usize).saturating_sub(label.chars().count() + 8);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("LIVE ", Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
            Span::styled(
                if label.is_empty() {
                    String::new()
                } else {
                    format!("▸ {label} ")
                },
                Style::new().fg(dimmed(color)),
            ),
            Span::styled("─".repeat(rule_width), Style::new().fg(FAINT)),
        ])),
        head,
    );
    let Some(stream) = stream else {
        return;
    };
    let height = body.height as usize;
    let width = body.width as usize;
    let mut lines: Vec<String> = stream
        .tail
        .lines()
        .rev()
        .take(height)
        .map(|line| clip_right(&line.replace('\t', "  ").replace('\r', ""), width))
        .collect();
    lines.reverse();
    let count = lines.len();
    let rendered: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index + 1 == count && stream.open && view.frame % 6 < 4 {
                Line::from(vec![
                    Span::styled(line, Style::new().fg(TEXT)),
                    Span::styled("▌", Style::new().fg(color)),
                ])
            } else {
                Line::styled(line, Style::new().fg(CODE))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), body);
}

fn nice_ceiling(value: f64) -> f64 {
    let step = if value <= 500.0 {
        100.0
    } else if value <= 2_000.0 {
        250.0
    } else {
        500.0
    };
    (value / step).ceil().max(1.0) * step
}

fn draw_chart(frame: &mut Frame, area: Rect, show: &Show) {
    let single = show.arm(Arm::Single);
    let slim = show.arm(Arm::Slim);
    let x_max = single
        .elapsed(show.now)
        .max(slim.elapsed(show.now))
        .max(10.0)
        * 1.02;
    let y_max = nice_ceiling(single.peak_rate.max(slim.peak_rate).max(300.0) * 1.1);
    let datasets = vec![
        Dataset::default()
            .name("single stream")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(SINGLE))
            .data(&single.rate),
        Dataset::default()
            .name("slimtoken")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(SLIM))
            .data(&slim.rate),
    ];
    let label = |text: String| Span::styled(text, Style::new().fg(DIM));
    let chart = Chart::new(datasets)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(FAINT))
                .style(Style::new().bg(PANEL))
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "OUTPUT TOKENS PER SECOND",
                        Style::new().fg(DIM).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " · all streams of each arm added together ",
                        Style::new().fg(FAINT),
                    ),
                ])),
        )
        .x_axis(
            Axis::default()
                .style(Style::new().fg(FAINT))
                .bounds([0.0, x_max])
                .labels(vec![
                    label("0s".to_string()),
                    label(format!("{:.0}s", x_max / 2.0)),
                    label(format!("{x_max:.0}s")),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::new().fg(FAINT))
                .bounds([0.0, y_max])
                .labels(vec![
                    label("0".to_string()),
                    label(fmt_int((y_max / 2.0) as u64)),
                    label(fmt_int(y_max as u64)),
                ]),
        )
        .legend_position(Some(LegendPosition::TopRight))
        .hidden_legend_constraints((Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)));
    frame.render_widget(chart, area);
}

/// One sentence that says who is ahead, in plain words.
/// The result of an arm's quality checks: `None` until all of them end.
fn checks_pass(arm: &ArmState) -> Option<bool> {
    if arm.checks.is_empty()
        || arm
            .checks
            .iter()
            .any(|check| check.state == CheckState::Running)
    {
        return None;
    }
    Some(
        arm.checks
            .iter()
            .all(|check| check.state == CheckState::Pass),
    )
}

/// What the quality checks say about the two apps, for the verdict line.
fn quality_note(single: &ArmState, slim: &ArmState) -> Option<Span<'static>> {
    let (text, color) = match (checks_pass(single)?, checks_pass(slim)?) {
        (true, true) => ("both apps build and pass their tests", SLIM),
        (false, true) => ("only the SlimToken app builds and passes its tests", SLIM),
        (true, false) => ("but the SlimToken app fails its checks", RED),
        (false, false) => ("both apps fail their checks", RED),
    };
    Some(Span::styled(
        format!("  ·  {text}"),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn verdict(show: &Show) -> Line<'static> {
    let single = show.arm(Arm::Single);
    let slim = show.arm(Arm::Slim);
    let bold = |text: String, color: Color| {
        Span::styled(text, Style::new().fg(color).add_modifier(Modifier::BOLD))
    };
    let plain = |text: String| Span::styled(text, Style::new().fg(TEXT));
    match (single.finished(), slim.finished()) {
        (true, true) => {
            let (fast, slow) = (slim.elapsed(show.now), single.elapsed(show.now));
            let failed = [single, slim].iter().any(|arm| arm.phase == Phase::Failed);
            if failed {
                Line::from(bold("One arm failed: no speed result.".to_string(), RED))
            } else if fast <= slow {
                let mut spans = vec![
                    bold(
                        format!("SlimToken {:.1}x faster", slow / fast.max(0.001)),
                        SLIM,
                    ),
                    plain(format!("  ·  {fast:.1}s vs {slow:.1}s")),
                ];
                spans.extend(quality_note(single, slim));
                Line::from(spans)
            } else {
                Line::from(vec![
                    bold(
                        format!("Single stream {:.1}x faster", fast / slow.max(0.001)),
                        SINGLE,
                    ),
                    plain(format!(
                        "  ·  {slow:.1}s vs {fast:.1}s (this job is too small for a wave)"
                    )),
                ])
            }
        }
        (false, true) => Line::from(vec![
            bold(
                format!("SlimToken finished in {:.1}s", slim.elapsed(show.now)),
                SLIM,
            ),
            plain("  ·  the single stream still writes  ·  ".to_string()),
            bold(
                format!("{:.1}x and counting", show.speedup().unwrap_or(1.0)),
                SLIM,
            ),
        ]),
        (true, false) => Line::from(vec![
            bold(
                format!("Single stream finished in {:.1}s", single.elapsed(show.now)),
                SINGLE,
            ),
            plain("  ·  the wave still runs".to_string()),
        ]),
        (false, false) => Line::styled(
            "Same objective, same model. The clock stops at each arm's last token.",
            Style::new().fg(DIM),
        ),
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, show: &Show, view: &View) {
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(4),
        height: 1,
        ..area
    };
    let verdict = verdict(show);
    let used = verdict.width() + view.keys.chars().count() + 3;
    frame.render_widget(Paragraph::new(verdict), inner);
    if used <= inner.width as usize {
        frame.render_widget(
            Paragraph::new(Line::styled(view.keys.clone(), Style::new().fg(FAINT)))
                .alignment(Alignment::Right),
            inner,
        );
    }
}

fn check_cell(arm: &ArmState, name: &str, now: f64) -> (String, Color) {
    match arm.checks.iter().find(|check| check.name == name) {
        None => ("not run".to_string(), FAINT),
        Some(check) => match check.state {
            CheckState::Running => (format!("running {:.0}s", now - check.started), BLUE),
            CheckState::Pass if name == "test" && !check.tail.is_empty() => {
                (format!("✓ {}", check.tail), SLIM)
            }
            CheckState::Pass => (format!("✓ pass ({:.0}s)", check.seconds), SLIM),
            CheckState::Fail => (format!("✗ {}", check.tail), RED),
        },
    }
}

fn draw_results(frame: &mut Frame, area: Rect, show: &Show) {
    let single = show.arm(Arm::Single);
    let slim = show.arm(Arm::Slim);
    let mut check_names: Vec<String> = Vec::new();
    for arm in &show.arms {
        for check in &arm.checks {
            if !check_names.contains(&check.name) {
                check_names.push(check.name.clone());
            }
        }
    }
    let wanted_height = 22
        + check_names.len() as u16
        + u16::from(show.is_demo())
        + 2 * u16::from(slim.verified.is_some())
        + u16::from(slim.tools.indexio.is_some() || slim.tools.lock_breaks.is_some());
    let width = area.width.saturating_sub(6).min(92);
    let height = area.height.saturating_sub(2).min(wanted_height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(SLIM))
        .style(Style::new().bg(BG).fg(TEXT))
        .padding(Padding::new(3, 3, 1, 0))
        .title(Line::from(Span::styled(
            " RESULTS ",
            Style::new().fg(SLIM).add_modifier(Modifier::BOLD),
        )))
        .title(
            Line::from(Span::styled(
                " r closes this panel ",
                Style::new().fg(FAINT),
            ))
            .alignment(Alignment::Right),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    let (fast, slow) = (slim.elapsed(show.now), single.elapsed(show.now));
    let both_ok = single.phase == Phase::Done && slim.phase == Phase::Done;
    let (factor, winner, winner_color) = if fast <= slow {
        (slow / fast.max(0.001), "SLIMTOKEN", SLIM)
    } else {
        (fast / slow.max(0.001), "SINGLE STREAM", SINGLE)
    };
    if both_ok {
        let digits = big(&format!("{factor:.1}x"));
        let captions = [
            String::new(),
            format!("   {winner} IS FASTER"),
            format!("   {fast:.1}s  vs  {slow:.1}s, same objective, same model"),
        ];
        for (row, caption) in digits.into_iter().zip(captions) {
            lines.push(Line::from(vec![
                Span::styled(
                    row,
                    Style::new().fg(winner_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(caption, Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
            ]));
        }
    } else {
        lines.push(Line::styled(
            "The run did not finish on both arms. There is no speed result.",
            Style::new().fg(RED).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::default());

    let label_width = 27usize;
    let cell_width = ((inner.width as usize).saturating_sub(label_width) / 2).max(12);
    let header = Line::from(vec![
        Span::raw(" ".repeat(label_width)),
        Span::styled(
            format!("{:<cell_width$}", "SINGLE STREAM"),
            Style::new().fg(SINGLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<cell_width$}", "SLIMTOKEN"),
            Style::new().fg(SLIM).add_modifier(Modifier::BOLD),
        ),
    ]);
    lines.push(header);
    let mut row = |label: &str, left: (String, Color), right: (String, Color)| {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<label_width$}"), Style::new().fg(DIM)),
            Span::styled(
                format!("{:<cell_width$}", clip_right(&left.0, cell_width - 1)),
                Style::new().fg(left.1),
            ),
            Span::styled(
                format!("{:<cell_width$}", clip_right(&right.0, cell_width - 1)),
                Style::new().fg(right.1),
            ),
        ]));
    };
    let cell = |text: String| (text, TEXT);
    let tokens = |arm: &ArmState| {
        format!(
            "{}{}",
            if arm.usage_exact() { "" } else { "~" },
            fmt_int(arm.output_tokens(show.chars_per_token) as u64)
        )
    };
    let average = |arm: &ArmState| {
        let elapsed = arm.elapsed(show.now);
        if elapsed > 0.0 {
            fmt_int((arm.output_tokens(show.chars_per_token) / elapsed) as u64)
        } else {
            "0".to_string()
        }
    };
    let files = |arm: &ArmState| match &arm.outcome {
        Some(outcome) => outcome.files.len().to_string(),
        None => "-".to_string(),
    };
    let finish = |arm: &ArmState| match (&arm.outcome, arm.phase) {
        (_, Phase::Failed) => ("failed".to_string(), RED),
        (Some(outcome), _) if !outcome.ok => (
            outcome
                .note
                .clone()
                .unwrap_or_else(|| "incomplete".to_string()),
            RED,
        ),
        (Some(_), _) => ("complete".to_string(), TEXT),
        (None, _) => ("-".to_string(), DIM),
    };
    row(
        "wall time",
        cell(format!("{:.1} s", single.elapsed(show.now))),
        cell(format!(
            "{:.1} s{}",
            slim.elapsed(show.now),
            slim.planning_seconds(show.now)
                .map(|plan| format!("  (plan {plan:.1} s)"))
                .unwrap_or_default()
        )),
    );
    if let (Some(code), Some(verify)) = (slim.code_seconds(), &slim.verified) {
        row(
            "  code written",
            cell(format!("{:.1} s", single.elapsed(show.now))),
            cell(format!("{code:.1} s")),
        );
        row(
            "  self-check + repair",
            ("none".to_string(), DIM),
            (
                match (verify.before_failed, verify.after_failed) {
                    (0, _) => format!("{:.1} s  passed first try", verify.seconds),
                    (before, 0) => format!(
                        "{:.1} s  {before} → 0, {} tests ok",
                        verify.seconds, verify.tests_passed
                    ),
                    (before, after) => {
                        format!("{:.1} s  {before} → {after} problems", verify.seconds)
                    }
                },
                if verify.after_failed == 0 {
                    SLIM
                } else {
                    SINGLE
                },
            ),
        );
    }
    row("output tokens", cell(tokens(single)), cell(tokens(slim)));
    row(
        "output tokens / second",
        cell(average(single)),
        cell(average(slim)),
    );
    row(
        "peak tokens / second",
        cell(fmt_int(single.peak_rate as u64)),
        cell(fmt_int(slim.peak_rate as u64)),
    );
    row(
        "input tokens",
        cell(fmt_int(single.input_tokens())),
        cell(fmt_int(slim.input_tokens())),
    );
    let cached = |state: &ArmState| {
        let (hits, input) = (state.cached_input_tokens(), state.input_tokens());
        if input == 0 {
            "-".to_string()
        } else {
            format!(
                "{} ({:.0}%)",
                fmt_int(hits),
                hits as f64 * 100.0 / input as f64
            )
        }
    };
    row(
        "  of them cache hits",
        cell(cached(single)),
        cell(cached(slim)),
    );
    row(
        "model requests",
        cell(single.requests.to_string()),
        cell(slim.requests.to_string()),
    );
    row(
        "streams at the same time",
        cell(single.peak_streams.to_string()),
        cell(slim.peak_streams.to_string()),
    );
    row("files written", cell(files(single)), cell(files(slim)));
    row(
        "lines written",
        cell(fmt_int(single.lines_written() as u64)),
        cell(fmt_int(slim.lines_written() as u64)),
    );
    row("answer", finish(single), finish(slim));
    // The repair guards' helper: indexio, or the text index.
    if slim.tools.indexio.is_some() || slim.tools.lock_breaks.is_some() {
        let tools = &slim.tools;
        let breaks = tools
            .lock_breaks
            .map(|breaks| format!(" · {breaks} lock breaks"))
            .unwrap_or_default();
        let text = match &tools.indexio {
            Some(_) => format!(
                "{} calls · {} ms · {} ms on clock{breaks}{}",
                tools.calls(),
                tools.total_ms(),
                tools.critical_ms,
                if tools.fallbacks > 0 {
                    format!(" · {} fallbacks", tools.fallbacks)
                } else {
                    String::new()
                }
            ),
            None => format!("text index{breaks}"),
        };
        row(
            if tools.indexio.is_some() {
                "indexio"
            } else {
                "repair guards"
            },
            ("none".to_string(), DIM),
            cell(text),
        );
    }
    for name in &check_names {
        row(
            &format!("check: {name}"),
            check_cell(single, name, show.now),
            check_cell(slim, name, show.now),
        );
    }
    lines.push(Line::default());
    for arm in [single, slim] {
        if let Some(outcome) = &arm.outcome {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<label_width$}", format!("{} app", arm.arm.dir_name())),
                    Style::new().fg(DIM),
                ),
                Span::styled(
                    clip_left(
                        &outcome.out_dir,
                        (inner.width as usize).saturating_sub(label_width),
                    ),
                    Style::new().fg(color_of(arm.arm)),
                ),
            ]));
        }
    }
    if show.is_demo() {
        lines.push(Line::styled(
            "SIMULATED DATA. This is not a measurement. Do not publish these numbers.",
            Style::new().fg(RED).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod intro_tests {
    use super::*;

    #[test]
    fn the_opening_screen_shows_the_whole_prompt_and_the_countdown() {
        let objective = include_str!("../objectives/taskflow.txt");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).expect("terminal");
        terminal
            .draw(|frame| {
                draw_intro(
                    frame,
                    objective,
                    "deepseek-flash",
                    "deepseek",
                    false,
                    Some(7.2),
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let screen: Vec<String> = (0..36u16)
            .map(|y| (0..120u16).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = screen.join("\n");
        println!("{text}");
        assert!(text.contains("THE PROMPT"));
        assert!(text.contains("starts in 8"));
        assert!(text.contains("deepseek-flash (deepseek)"));
        // The first and the last words of the prompt are on the screen.
        let words: Vec<&str> = objective.split_whitespace().collect();
        assert!(text.contains(words[0]), "{}", words[0]);
        assert!(
            text.contains(words[words.len() - 1]),
            "{}",
            words[words.len() - 1]
        );
    }
}
