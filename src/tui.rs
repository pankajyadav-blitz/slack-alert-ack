//! The full-screen terminal dashboard for the running bot.
//!
//! Modeled on the `sts` tool's live views: a rounded, cyan-accented title bar
//! with a ticking clock, body panels, and a footer of keybindings. Built on
//! `ratatui` + `crossterm`.
//!
//! It is only ever entered when stdout is a real terminal (see `main.rs`). In the
//! piped / redirected / service path the bot keeps printing its ordinary stderr
//! log, which a TUI cannot do — it needs raw mode and the alternate screen, and
//! would otherwise scribble control codes into a log file. One-shot commands
//! (`paths`, `--list-channels`, `--test`, `--click`) stay plain for the same
//! reason.
//!
//! The bot's poll / socket loop runs on another thread and only *appends* to the
//! shared [`Activity`] store; this loop only *reads* a cheap snapshot each redraw.
//! A key press (q / Esc / Ctrl-C) sets the shared stop flag so the bot thread
//! winds down, then the terminal is restored.

use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::activity::{Activity, Outcome, Snapshot};

/// How often the dashboard redraws so the clock and relative ages tick smoothly.
const REDRAW_TICK: Duration = Duration::from_millis(1000);

/// The cyan the title bar, panel titles and keys share — the `sts` accent.
const ACCENT: Color = Color::Cyan;

/// Run the dashboard until the user quits or `is_done()` reports the bot has
/// stopped. `is_done` is polled once per redraw (e.g. the shared stop flag);
/// `quit` is called when the user presses q / Esc / Ctrl-C so the bot thread can
/// wind down. The terminal is always restored before returning, even on error.
pub fn run<D, Q>(activity: &Activity, mut is_done: D, mut quit: Q) -> std::io::Result<()>
where
    D: FnMut() -> bool,
    Q: FnMut(),
{
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(&mut terminal, activity, &mut is_done, &mut quit);
    ratatui::try_restore()?;
    result
}

fn run_loop<D, Q>(
    terminal: &mut ratatui::DefaultTerminal,
    activity: &Activity,
    is_done: &mut D,
    quit: &mut Q,
) -> std::io::Result<()>
where
    D: FnMut() -> bool,
    Q: FnMut(),
{
    loop {
        let snap = activity.snapshot();
        terminal.draw(|f| draw(f, &snap))?;

        // The bot thread stopped on its own (fatal error, signal) — leave.
        if is_done() {
            return Ok(());
        }

        // Wake for the next redraw, or sooner on a key press.
        if event::poll(REDRAW_TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && should_quit(key.code, key.modifiers)
        {
            quit();
            return Ok(());
        }
    }
}

/// q / Esc / Ctrl-C all quit the dashboard.
fn should_quit(code: KeyCode, mods: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
}

// ---- rendering --------------------------------------------------------------

fn draw(f: &mut Frame, snap: &Snapshot) {
    f.render_widget(Clear, f.area());

    // title bar | body | footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_title(f, chunks[0], snap);
    draw_body(f, chunks[1], snap);
    draw_footer(f, chunks[2], snap);
}

fn draw_title(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let mode = match snap.mode {
        "user" => "user mode",
        _ => "bot mode",
    };
    let dry = if snap.dry_run { " · dry-run" } else { "" };
    let left = Span::styled(
        format!(" ack-bot · {mode}{dry} "),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    // Recompute every redraw so it ticks once a second.
    let right = Span::styled(
        format!("up {}  {}  ", fmt_age(snap.uptime_secs), clock()),
        Style::default().fg(Color::DarkGray),
    );
    let spacer_len = (area.width as usize).saturating_sub(left.width() + right.width() + 2);
    let line = Line::from(vec![left, Span::raw(" ".repeat(spacer_len)), right]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_body(f: &mut Frame, area: Rect, snap: &Snapshot) {
    // Watching on top (short, grows with the channel list), the Activity feed
    // taking the rest.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(watching_height(snap)), Constraint::Min(3)])
        .split(area);
    draw_watching(f, rows[0], snap);
    draw_activity(f, rows[1], snap);
}

/// Enough rows for every channel plus the borders, capped so a huge allowlist
/// never squeezes the activity feed off the screen.
fn watching_height(snap: &Snapshot) -> u16 {
    let lines = snap.channels.len().clamp(1, 6) as u16;
    lines + 2
}

fn panel<'a>(title: &'a str, count: Option<usize>) -> Block<'a> {
    let title = match count {
        Some(n) => format!(" {title} ({n}) "),
        None => format!(" {title} "),
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn draw_watching(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = panel("Watching", Some(snap.channels.len()));
    if snap.channels.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "resolving channels…",
                Style::default().fg(Color::DarkGray),
            )))
            .block(block),
            area,
        );
        return;
    }
    let lines: Vec<Line> = snap
        .channels
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::styled("● ", Style::default().fg(Color::Green)),
                Span::raw(c.clone()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_activity(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = panel("Activity", Some(snap.events.len()));
    if snap.events.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no alerts handled yet — waiting for messages.",
                Style::default().fg(Color::DarkGray),
            )))
            .block(block)
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows = snap.events.iter().map(|e| {
        let (glyph, label, colour) = look(e.outcome);
        // Line 1: glyph + outcome + channel/ts (left).
        let line1 = Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(colour)),
            Span::styled(label, Style::default().fg(colour).bold()),
            Span::raw("  "),
            Span::raw(format!("{}/{}", e.channel, e.ts)),
        ]);
        // Line 2: the detail (method or reason) · age, right-aligned so long
        // reasons are shown in full instead of being clipped.
        let detail = format!("{} · {}", e.detail, fmt_age(snap.uptime_secs.saturating_sub(e.at_secs)));
        let line2 = Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray)))
            .right_aligned();
        Row::new(vec![Cell::from(Text::from(vec![line1, line2]))]).height(2)
    });

    let table = Table::new(rows, [Constraint::Percentage(100)])
        .column_spacing(0)
        .block(block);
    f.render_widget(table, area);
}

/// (glyph, label, colour) for an outcome — the `sts` convention: ● busy/accent,
/// ✓ good/green, ✗ bad/red.
fn look(outcome: Outcome) -> (&'static str, &'static str, Color) {
    match outcome {
        Outcome::Acked => ("✓", "acked", Color::Green),
        Outcome::NotAcked => ("●", "not acked", Color::Yellow),
        Outcome::Failed => ("✗", "failed", Color::Red),
    }
}

fn draw_footer(f: &mut Frame, area: Rect, snap: &Snapshot) {
    let line = Line::from(vec![
        Span::styled(" q", Style::default().fg(ACCENT).bold()),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(format!("✓ {} acked", snap.acked), Style::default().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(
            format!("● {} not acked", snap.not_acked),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(format!("✗ {} failed", snap.failed), Style::default().fg(Color::Red)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---- small helpers ----------------------------------------------------------

/// Local wall clock, matching the log's timestamp style but to the second.
fn clock() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Compact relative age, e.g. `12s`, `3m`, `1h4m`.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn age_formats_compactly() {
        assert_eq!(fmt_age(5), "5s");
        assert_eq!(fmt_age(90), "1m");
        assert_eq!(fmt_age(3_900), "1h5m");
    }

    #[test]
    fn renders_panels_and_rows_to_a_buffer() {
        let activity = Activity::new("user", true);
        activity.set_channels(vec!["#alerts".to_string(), "#ops".to_string()]);
        activity.record(Outcome::Acked, "#alerts", "1.1", "url_click:host:200");
        activity.record(Outcome::NotAcked, "#alerts", "2.2", "no ack path");
        activity.record(Outcome::Failed, "#ops", "3.3", "session expired");
        let snap = activity.snapshot();

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| draw(f, &snap)).unwrap();
        let rendered = terminal.backend().to_string();

        // Title bar, both panels, the seeded rows and the counters must all show.
        assert!(rendered.contains("ack-bot"), "title missing");
        assert!(rendered.contains("user mode"), "mode missing");
        assert!(rendered.contains("dry-run"), "dry-run flag missing");
        assert!(rendered.contains("Watching"), "watching panel missing");
        assert!(rendered.contains("#alerts"), "channel missing");
        assert!(rendered.contains("Activity"), "activity panel missing");
        assert!(rendered.contains("acked"), "acked outcome missing");
        assert!(rendered.contains("not acked"), "not-acked outcome missing");
        assert!(rendered.contains("failed"), "failed outcome missing");
        assert!(rendered.contains('q') && rendered.contains("quit"), "footer missing");
        // Rounded borders mean the box-drawing corner should appear.
        assert!(rendered.contains('╭'), "panel border missing");
    }
}
