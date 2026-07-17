//! The ratatui view: a narrow side-pane layout — header, LIVE TURN, FILES
//! THIS TURN, SESSION, HISTORY, HEATMAP (files × days intensity grid),
//! SESSIONS, ATTENTION — plus the full-screen REPORT view.

use chrono::{DateTime, Duration, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::state::{fmt_duration, fmt_tokens};
use crate::{App, View};

const PURPLE: Color = Color::Rgb(0xb5, 0x8f, 0xf2);
const CYAN: Color = Color::Rgb(0x56, 0xc9, 0xdf);
const GREEN: Color = Color::Rgb(0x7b, 0xd8, 0x8f);
const YELLOW: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
const RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
const FG: Color = Color::Rgb(0xc8, 0xd0, 0xdc);
const DIM: Color = Color::Rgb(0x56, 0x60, 0x73);
const FAINT: Color = Color::Rgb(0x39, 0x41, 0x4f);
const BORDER: Color = Color::Rgb(0x27, 0x30, 0x44);
const BORDER_ALERT: Color = Color::Rgb(0x5c, 0x3a, 0x41);

const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Single-hue brightness ramp for the heatmap cells: dim slot marker for
/// zero, then darker→brighter purple as intensity rises (monotonic
/// brightness reads as intensity; mixed hues do not).
const HEAT: [Color; 5] = [
    Color::Rgb(0x2a, 0x30, 0x3e),
    Color::Rgb(0x48, 0x37, 0x72),
    Color::Rgb(0x6f, 0x4b, 0xa8),
    Color::Rgb(0x96, 0x6e, 0xd8),
    Color::Rgb(0xc9, 0xac, 0xff),
];
/// Days shown per heatmap row; each cell is two characters wide.
const HEAT_DAYS: i64 = 14;
const HEAT_FILES: usize = 4;

pub(crate) fn render(frame: &mut Frame, app: &App) {
    match app.view {
        View::Watch => render_watch(frame, app),
        View::Report => render_report_view(frame, app),
    }
}

fn render_watch(frame: &mut Frame, app: &App) {
    let now = Utc::now();
    let [
        header,
        sub,
        live,
        files,
        session,
        history,
        heat,
        sessions,
        attention,
        _rest,
        footer,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, app, header, sub);
    render_live(frame, app, now, live);
    render_files(frame, app, files);
    render_session(frame, app, session);
    render_history(frame, app, history);
    render_heat(frame, app, heat);
    render_sessions(frame, app, sessions);
    render_attention(frame, app, now, attention);
    render_footer(frame, app, footer);
}

fn render_header(frame: &mut Frame, app: &App, header: Rect, sub: Rect) {
    frame.render_widget(
        Paragraph::new(two_columns(
            header.width,
            vec![
                Span::styled("▚ benclaude ", Style::new().fg(PURPLE).bold()),
                Span::styled("watch", Style::new().fg(DIM)),
            ],
            vec![Span::styled(app.project_name.clone(), Style::new().fg(DIM))],
        )),
        header,
    );
    let (status_dot, status) = if app.session.is_some() {
        (Span::styled("● ", Style::new().fg(GREEN)), "live")
    } else {
        (Span::styled("○ ", Style::new().fg(DIM)), "no session")
    };
    let started = app
        .session
        .as_ref()
        .and_then(|s| s.started)
        .map_or_else(|| "--:--".to_owned(), local_hm);
    let id = app.session.as_ref().map_or("", |s| &s.session_id);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("session {} · started {started} · ", short_id(id)),
                Style::new().fg(DIM),
            ),
            status_dot,
            Span::styled(status, Style::new().fg(GREEN)),
        ])),
        sub,
    );
}

fn render_live(frame: &mut Frame, app: &App, now: DateTime<Utc>, area: Rect) {
    let mut lines = Vec::new();
    if let Some(session) = &app.session {
        let elapsed = session
            .last_prompt_at
            .map_or_else(|| "0s".to_owned(), |t| fmt_duration(now - t));
        lines.push(two_columns(
            area.width.saturating_sub(2),
            vec![
                Span::styled("turn ", Style::new().fg(FG)),
                Span::styled(session.prompts.to_string(), Style::new().fg(FG).bold()),
                Span::styled("  ·  ", Style::new().fg(DIM)),
                Span::styled(elapsed, Style::new().fg(YELLOW)),
            ],
            vec![Span::styled(
                format!("{} tok", fmt_tokens(session.turn_output_tokens)),
                Style::new().fg(DIM),
            )],
        ));
        if let Some(run) = session.running_tool() {
            lines.push(Line::from(vec![
                Span::styled("└ ", Style::new().fg(DIM)),
                Span::styled(format!("{}  ", run.name), Style::new().fg(FG)),
                Span::styled(run.label.clone(), Style::new().fg(FG)),
                Span::styled(
                    format!(" — running {}", fmt_duration(now - run.at)),
                    Style::new().fg(FAINT),
                ),
            ]));
        }
        for run in session.recent_finished(5 - usize::from(session.running_tool().is_some())) {
            let duration = run.done_at.map_or_else(String::new, |done| {
                format!(" {}", fmt_duration(done - run.at))
            });
            lines.push(Line::from(vec![
                Span::styled(local_hms(run.at), Style::new().fg(FAINT)),
                Span::raw("  "),
                Span::styled(format!("{:<5}", run.name), Style::new().fg(GREEN)),
                Span::raw(" "),
                Span::styled(run.label.clone(), Style::new().fg(FG)),
                Span::styled(duration, Style::new().fg(DIM)),
            ]));
        }
    } else {
        lines.push(Line::styled(
            "waiting for a session to start…",
            Style::new().fg(DIM),
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(block("LIVE TURN", false)), area);
}

fn render_files(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(session) = &app.session {
        for file in session.files_this_turn.iter().rev().take(3) {
            let churn = app.history.churn(file);
            let (mark, mark_style, note) = if churn >= 4 {
                (
                    "⚠ ",
                    Style::new().fg(YELLOW),
                    format!("— {churn} edits /30d"),
                )
            } else {
                ("✓ ", Style::new().fg(GREEN), "— quiet file".to_owned())
            };
            lines.push(Line::from(vec![
                Span::styled(mark, mark_style),
                Span::styled(file.clone(), Style::new().fg(FG)),
                Span::styled(format!(" {note}"), Style::new().fg(DIM)),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("no edits yet this turn", Style::new().fg(DIM)));
    }
    frame.render_widget(
        Paragraph::new(lines).block(block("FILES THIS TURN", false)),
        area,
    );
}

fn render_session(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2);
    let mut lines = Vec::new();
    if let Some(session) = &app.session {
        lines.push(kv_row(
            width,
            "tokens",
            vec![
                Span::styled(
                    fmt_tokens(session.input_tokens + session.output_tokens),
                    Style::new().fg(FG),
                ),
                Span::styled(
                    format!(" ({} out)", fmt_tokens(session.output_tokens)),
                    Style::new().fg(DIM),
                ),
            ],
        ));
        lines.push(kv_row(
            width,
            "follow-ups",
            vec![
                Span::styled(
                    session.prompts.saturating_sub(1).to_string(),
                    Style::new().fg(YELLOW),
                ),
                Span::styled(
                    format!(" (avg {:.1})", app.history.followups_avg),
                    Style::new().fg(DIM),
                ),
            ],
        ));
        lines.push(kv_row(
            width,
            "interruptions",
            vec![Span::styled(
                session.interruptions.to_string(),
                Style::new().fg(FG),
            )],
        ));
        lines.push(kv_row(
            width,
            "babysit gap",
            vec![
                Span::styled(fmt_duration(session.babysit), Style::new().fg(FG)),
                Span::styled(
                    format!(" today {}", fmt_duration(app.history.babysit_today)),
                    Style::new().fg(DIM),
                ),
            ],
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(block("SESSION", false)), area);
}

fn render_history(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2);
    let history = &app.history;
    let mut lines = vec![kv_row(
        width,
        "sessions",
        vec![Span::styled(
            history.sessions.to_string(),
            Style::new().fg(GREEN).bold(),
        )],
    )];
    let peak = history
        .tokens_by_day
        .iter()
        .map(|(_, tokens)| *tokens)
        .max()
        .unwrap_or(0);
    let spark: String = history
        .tokens_by_day
        .iter()
        .map(|(_, tokens)| spark_char(*tokens, peak))
        .collect();
    lines.push(Line::from(vec![
        Span::styled(spark, Style::new().fg(PURPLE)),
        Span::styled("  tok/day · 8d", Style::new().fg(DIM)),
    ]));
    lines.push(kv_row(
        width,
        "follow-ups/session",
        vec![Span::styled(
            format!("{:.1}", history.followups_avg),
            Style::new().fg(FG),
        )],
    ));
    frame.render_widget(
        Paragraph::new(lines).block(block("HISTORY · 30D", false)),
        area,
    );
}

/// Files × days edit grid: each cell one local day showing the edit COUNT
/// as a digit ('+' for 10+, '·' for none) — digits discriminate where five
/// similar purples cannot. Colors use absolute thresholds, so a shade means
/// the same number of edits on every day and every run.
fn render_heat(frame: &mut Frame, app: &App, area: Rect) {
    let today = Local::now().date_naive();
    let hottest = app.history.hottest_files(HEAT_FILES);
    let cells = usize::try_from(HEAT_DAYS).unwrap_or(0);
    let label_width = usize::from(area.width.saturating_sub(2))
        .saturating_sub(cells * 2 + 1)
        .max(8);

    let mut lines = Vec::new();
    for (file, _) in &hottest {
        let days = app.history.edits_by_file_day.get(*file);
        let mut spans = vec![Span::styled(
            format!("{:<label_width$} ", truncate(file, label_width)),
            Style::new().fg(FG),
        )];
        for days_ago in (0..HEAT_DAYS).rev() {
            let day = today - Duration::days(days_ago);
            let edits = days.and_then(|d| d.get(&day)).copied().unwrap_or(0);
            spans.push(heat_cell(edits));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no edits in 30 days", Style::new().fg(DIM)));
    } else {
        let axis_width = (cells * 2).saturating_sub(5);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(label_width + 1)),
            Span::styled(
                format!("{:<axis_width$}today", "14d ago"),
                Style::new().fg(FAINT),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(block("HEATMAP · EDITS × 14D", false)),
        area,
    );
}

/// The most recent sessions with their linked-commit count from the report.
fn render_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2);
    let mut lines = Vec::new();
    for summary in app.history.summaries.iter().rev().take(2) {
        let commits = app.report.as_ref().map(|data| {
            data.sessions
                .iter()
                .find(|row| row.id == summary.id)
                .map_or(0, |row| row.commits)
        });
        let mut right = vec![Span::styled(
            format!(
                "{}t  {}",
                summary.prompts,
                fmt_tokens(summary.output_tokens)
            ),
            Style::new().fg(FG),
        )];
        match commits {
            Some(count) => right.push(Span::styled(
                format!("  ⚑{count}"),
                Style::new().fg(if count > 0 { GREEN } else { DIM }),
            )),
            None => right.push(Span::styled("  ⚑—", Style::new().fg(FAINT))),
        }
        lines.push(two_columns(
            width,
            vec![
                Span::styled(
                    format!("{} ", short_id(&summary.id)),
                    Style::new().fg(PURPLE),
                ),
                Span::styled(
                    summary
                        .start
                        .with_timezone(&Local)
                        .format("%m-%d %H:%M")
                        .to_string(),
                    Style::new().fg(FAINT),
                ),
            ],
            right,
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no sessions yet", Style::new().fg(DIM)));
    }
    frame.render_widget(Paragraph::new(lines).block(block("SESSIONS", false)), area);
}

fn render_attention(frame: &mut Frame, app: &App, now: DateTime<Utc>, area: Rect) {
    let waiting = app.session.as_ref().and_then(|s| s.waiting_since);
    let mut lines = Vec::new();
    if let Some(since) = waiting {
        lines.push(Line::from(Span::styled(
            format!("⏳ waiting on you — {}", fmt_duration(now - since)),
            Style::new().fg(RED).add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "● agent working",
            Style::new().fg(GREEN),
        )));
    }
    if let Some(text) = app
        .session
        .as_ref()
        .and_then(|s| s.last_assistant_text.as_deref())
    {
        let first = text.lines().next().unwrap_or_default();
        lines.push(Line::styled(
            format!(
                "last: “{}”",
                truncate(first, usize::from(area.width).saturating_sub(12))
            ),
            Style::new().fg(DIM),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(block("ATTENTION", waiting.is_some())),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(toast) = &app.toast {
        Line::styled(toast.clone(), Style::new().fg(YELLOW))
    } else if app.view == View::Watch {
        keys_line(&[("q", "uit  "), ("r", "eport  ")])
    } else {
        keys_line(&[("b", "ack  "), ("r", "efresh  ")])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn keys_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, rest) in keys {
        spans.push(Span::styled(format!("[{key}]"), Style::new().fg(FAINT)));
        spans.push(Span::styled((*rest).to_owned(), Style::new().fg(DIM)));
    }
    spans.push(Span::styled("· ro-mode", Style::new().fg(FAINT)));
    Line::from(spans)
}

// ---- full-screen report view ------------------------------------------------

fn render_report_view(frame: &mut Frame, app: &App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    frame.render_widget(
        Paragraph::new(report_lines(app)).block(block("REPORT · 30D", false)),
        body,
    );
    render_footer(frame, app, footer);
}

fn report_lines(app: &App) -> Vec<Line<'static>> {
    let Some(data) = &app.report else {
        return vec![Line::styled("no data — press r", Style::new().fg(DIM))];
    };
    let mut lines = Vec::new();
    lines.push(kv_line("AI commits", data.commits.len().to_string(), GREEN));
    let added: u64 = data.commits.iter().map(|c| c.added).sum();
    lines.push(kv_line("lines added", added.to_string(), FG));
    match data.survival_pct() {
        Some(pct) => lines.push(kv_line(
            "line survival 7d+",
            format!(
                "{pct:.0}%  ({}/{} mature)",
                data.surviving_mature, data.added_mature
            ),
            if pct >= 80.0 { GREEN } else { YELLOW },
        )),
        None => lines.push(kv_line("line survival 7d+", "— (<7d old)".to_owned(), DIM)),
    }
    if let Some(cost) = data.tokens_per_surviving_line() {
        lines.push(kv_line("tok/surviving line", format!("{cost:.0}"), FG));
    }
    lines.push(Line::raw(""));
    for row in data.commits.iter().take(12) {
        let age = if row.mature { "" } else { " <7d" };
        lines.push(Line::from(vec![
            Span::styled(short_id(&row.sha), Style::new().fg(PURPLE)),
            Span::styled(
                format!("  {}", row.at.format("%m-%d")),
                Style::new().fg(FAINT),
            ),
            Span::styled(
                format!("  {}/{}{age}", row.surviving, row.added),
                Style::new().fg(if row.surviving * 2 >= row.added {
                    GREEN
                } else {
                    YELLOW
                }),
            ),
            Span::styled(
                format!("  {}", truncate(&row.subject, 24)),
                Style::new().fg(FG),
            ),
            Span::styled(
                row.session_id
                    .as_deref()
                    .map_or_else(String::new, |id| format!("  ⇠{}", short_id(id))),
                Style::new().fg(DIM),
            ),
        ]));
    }
    lines
}

fn kv_line(label: &str, value: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<19}"), Style::new().fg(DIM)),
        Span::styled(value, Style::new().fg(color)),
    ])
}

// ---- helpers ----------------------------------------------------------------

fn block(title: &str, alert: bool) -> Block<'_> {
    let (border, title_color) = if alert {
        (BORDER_ALERT, RED)
    } else {
        (BORDER, CYAN)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(title_color).bold(),
        ))
}

/// A `label ........ value` row with the value right-aligned.
fn kv_row(width: u16, label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    two_columns(
        width,
        vec![Span::styled(label.to_owned(), Style::new().fg(DIM))],
        value,
    )
}

/// Lays two span groups out on one line with the second group right-aligned.
fn two_columns(width: u16, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let used: usize = left.iter().chain(&right).map(Span::width).sum();
    let pad = usize::from(width).saturating_sub(used).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(spans)
}

/// One day cell: the edit count as a digit, colored by fixed thresholds
/// (1 / 2–3 / 4–6 / 7+), `·` for an empty day.
fn heat_cell(edits: u64) -> Span<'static> {
    let (glyph, color) = match edits {
        0 => ('·', FAINT),
        1 => ('1', HEAT[1]),
        2..=3 => (
            char::from_digit(u32::try_from(edits).unwrap_or(2), 10).unwrap_or('2'),
            HEAT[2],
        ),
        4..=6 => (
            char::from_digit(u32::try_from(edits).unwrap_or(4), 10).unwrap_or('4'),
            HEAT[3],
        ),
        7..=9 => (
            char::from_digit(u32::try_from(edits).unwrap_or(7), 10).unwrap_or('7'),
            HEAT[4],
        ),
        _ => ('+', HEAT[4]),
    };
    Span::styled(format!("{glyph} "), Style::new().fg(color).bold())
}

fn spark_char(value: u64, peak: u64) -> char {
    if peak == 0 || value == 0 {
        return SPARK[0];
    }
    let bucket = (value * (SPARK.len() as u64 - 1)).div_ceil(peak);
    SPARK[usize::try_from(bucket)
        .unwrap_or(SPARK.len() - 1)
        .min(SPARK.len() - 1)]
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn local_hm(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

fn local_hms(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M:%S").to_string()
}
