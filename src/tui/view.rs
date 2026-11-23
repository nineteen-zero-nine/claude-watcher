//! Drawing the interface from its state.

use std::fmt::Write as _;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, LineGauge, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::Frame;

use super::app::{App, Tab};
use super::worker::Target;
use crate::hooks::{HookReport, State};
use crate::paths::tilde;
use crate::util::{age, now_ms, one_line, thousands};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const WARN: Style = Style::new().fg(Color::Yellow);
const BAD: Style = Style::new().fg(Color::Red);
const GOOD: Style = Style::new().fg(Color::Green);
const ACCENT: Style = Style::new().fg(Color::Cyan);
const CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Width of the share bars in the breakdown, in cells.
const BAR_CELLS: usize = 10;
/// The tree never needs more room than this; the rest goes to content.
const TREE_MAX_WIDTH: u16 = 84;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [top, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    draw_tabs(frame, top, app);
    match app.tab {
        Tab::Context => draw_context(frame, body, app),
        Tab::Subagents => draw_subagents(frame, body, app),
        Tab::Sessions => draw_sessions(frame, body, app),
        Tab::Hooks => draw_hooks(frame, body, app),
    }
    draw_footer(frame, bottom, app);
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let subagents = app.snapshot.as_ref().map_or(0, |snapshot| snapshot.subagents.len());
    let hooks = app.hooks.as_ref().map_or(0, |report| report.hooks.len());
    let titles = [
        "1 Context".to_string(),
        format!("2 Subagents {subagents}"),
        format!("3 Sessions {}", app.sessions.len()),
        format!("4 Hooks {hooks}"),
    ];
    let selected = Tab::ALL.iter().position(|&tab| tab == app.tab);
    let [name, tabs] = Layout::horizontal([Constraint::Length(16), Constraint::Min(0)]).areas(area);
    frame.render_widget(Line::styled(" claude-watcher", BOLD), name);
    frame.render_widget(Tabs::new(titles).select(selected).highlight_style(BOLD.patch(CURSOR)), tabs);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys = match app.tab {
        Tab::Context if app.reading => "↑↓ scroll  n/p next/previous piece  Esc back to the tree  ",
        Tab::Context => "↑↓ move  ←→ fold  e/c all  Enter read  ",
        Tab::Subagents => "↑↓ move  Enter open  ",
        Tab::Sessions => "↑↓ move  Enter open  f follow latest  ",
        Tab::Hooks => "↑↓ scroll  ",
    };
    let back = if app.can_go_back() { "Esc back  " } else { "" };
    let status = match (app.loading, &app.target) {
        (true, _) => "reading… ",
        (false, Target::Latest) => "following the latest session ",
        (false, Target::Transcript(_)) => "following this session ",
    };
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(status.chars().count() as u16)]).areas(area);
    frame.render_widget(Line::styled(format!(" {keys}{back}w window  r reload  Tab view  q quit"), DIM), left);
    frame.render_widget(Line::styled(status, if app.loading { WARN } else { DIM }), right);
}

fn placeholder(frame: &mut Frame, area: Rect, message: &str) {
    let text = Paragraph::new(message.to_string()).style(DIM).wrap(Wrap { trim: true }).centered();
    frame.render_widget(text.block(Block::bordered()), area);
}

fn right(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).right_aligned())
}

fn tokens(count: Option<u64>) -> String {
    count.map_or("-".to_string(), |count| thousands(count as i64))
}

fn share_of_window(count: Option<u64>, window: u64) -> String {
    count.map_or(String::new(), |count| format!("{:.1}%", 100.0 * count as f64 / window as f64))
}

fn bar(percent: f64) -> String {
    let cells = (percent.clamp(0.0, 100.0) / 100.0 * BAR_CELLS as f64).round() as usize;
    "█".repeat(cells)
}

fn draw_context(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(snapshot) = &app.snapshot else {
        let message = app.problem.clone().unwrap_or_else(|| "Reading the session…".to_string());
        return placeholder(frame, area, &message);
    };
    let Some(report) = &snapshot.report else {
        let message = format!("{} records no API requests yet.", tilde(&snapshot.transcript));
        return placeholder(frame, area, &message);
    };
    let total = report.total;
    let window = app.window as f64;
    let percent = |tokens: f64| if total > 0.0 { 100.0 * tokens / total } else { 0.0 };

    // Who and what.
    let (kind, name) = match &app.subagent {
        Some(task) => (" Subagent ", one_line(task, 70)),
        None => (" Session ", report.session_id.chars().take(8).collect()),
    };
    let mut identity = vec![
        Span::styled(kind, DIM),
        Span::styled(name, BOLD),
        Span::raw(format!("  {}", report.cwd.as_deref().map_or(String::new(), |cwd| tilde(cwd.as_ref())))),
    ];
    if let Some(title) = &report.title {
        identity.push(Span::styled(format!("  {}", one_line(title, 70)), ACCENT));
    }
    let request = format!(
        " Request #{} · {} · {} messages · {} · {}",
        report.request_number, report.model, report.message_count, report.timestamp, report.query_source
    );

    // How full.
    let ratio = (total / window).clamp(0.0, 1.0);
    let fill = if ratio >= 0.8 { BAD } else if ratio >= 0.6 { WARN } else { GOOD };
    let gauge = LineGauge::default()
        .ratio(ratio)
        .label(format!(
            " {} / {} tokens · {:.1}%{}",
            thousands(total.round() as i64),
            thousands(app.window as i64),
            100.0 * total / window,
            if report.usage.is_some() { "" } else { " (estimated)" }
        ))
        .filled_symbol("█")
        .unfilled_symbol("░")
        .filled_style(fill)
        .unfilled_style(DIM);
    let usage = match report.usage {
        Some(u) => format!(
            " cache read {} · write {} · uncached {} · last reply {} output tokens ({} thinking) · {:.0}% placed from API usage",
            thousands(u.cache_read as i64),
            thousands(u.cache_creation as i64),
            thousands(u.input as i64),
            thousands(u.output as i64),
            thousands(u.thinking as i64),
            100.0 * report.measured_share
        ),
        None => format!(" {:.0}% placed from API usage", 100.0 * report.measured_share),
    };
    let notes: Vec<Line> = snapshot
        .note
        .iter()
        .chain(&report.notes)
        .map(|note| Line::styled(format!(" Note: {note}"), WARN))
        .collect();
    // Wrapped height, estimated generously so that no note is cut short.
    let wrap_at = usize::from(area.width).saturating_sub(8).max(1);
    let notes_height: usize = notes.iter().map(|note| note.width().div_ceil(wrap_at)).sum();

    let [identity_area, gauge_area, usage_area, notes_area, main] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(notes_height.min(6) as u16),
        Constraint::Min(0),
    ])
    .areas(area);
    // Reading gives the whole width to the content.
    let tree_width = if app.reading { 0 } else { (main.width / 2).min(TREE_MAX_WIDTH) };
    let [left, details_area] = Layout::horizontal([Constraint::Length(tree_width), Constraint::Min(0)]).areas(main);

    // The tree.
    let rows = app.rows();
    let cursor = app.cursor();
    let lines: Vec<Row> = rows
        .iter()
        .map(|row| {
            let depth = row.path.len() - 1;
            let marker = match (row.node.children.is_empty(), row.open) {
                (true, _) => "  ",
                (false, true) => "▾ ",
                (false, false) => "▸ ",
            };
            let style = match depth {
                _ if row.node.tokens < 0.0 => BAD,
                0 => BOLD,
                _ => Style::new(),
            };
            Row::new([
                Cell::from(format!(" {}{marker}{}", "  ".repeat(depth), row.node.name)),
                right(thousands(row.node.tokens.round() as i64)),
                right(format!("{:.1}%", percent(row.node.tokens))),
                Cell::from(bar(percent(row.node.tokens))).style(ACCENT),
            ])
            .style(style)
        })
        .collect();
    let tree = Table::new(
        lines,
        [Constraint::Min(24), Constraint::Length(10), Constraint::Length(7), Constraint::Length(BAR_CELLS as u16)],
    )
    .header(Row::new([Cell::from(" Part"), right("Tokens".into()), right("%".into()), Cell::from("")]).style(DIM))
    .row_highlight_style(CURSOR)
    .block(Block::bordered().title(" What fills the context "));

    // The selected part: what it weighs, then what it says.
    let mut facts: Vec<Line> = Vec::new();
    let mut heading = String::new();
    if let Some(row) = rows.get(cursor) {
        heading = format!(" {} ", row.path.join(" › "));
        let node = row.node;
        let mut weight = format!("{:.1}% of this context", percent(node.tokens));
        let parent = rows.iter().find(|other| other.path[..] == row.path[..row.path.len() - 1]);
        if let Some(parent) = parent.filter(|parent| parent.node.tokens > 0.0) {
            let _ = write!(weight, " · {:.1}% of {}", 100.0 * node.tokens / parent.node.tokens, parent.node.name);
        }
        let _ = write!(weight, " · {:.1}% of the window", 100.0 * node.tokens / window);
        facts.push(Line::styled(format!(" {} tokens", thousands(node.tokens.round() as i64)), BOLD));
        facts.push(Line::styled(format!(" {weight}"), DIM));
        facts.push(Line::raw(""));
    }
    drop(rows);

    let border = if app.reading { ACCENT } else { Style::new() };
    let pane = Block::bordered().border_style(border).title(heading);
    let inside = pane.inner(details_area);
    let room = usize::from(inside.height).saturating_sub(facts.len());
    let content = app.content_window(usize::from(inside.width).saturating_sub(2), room);
    let pane = match content.total {
        0 => pane,
        total => pane.title_bottom(
            Line::styled(
                format!(
                    " piece {} of {} · lines {}–{} of {} ",
                    content.piece,
                    content.pieces,
                    thousands(content.first as i64 + 1),
                    thousands((content.first + content.lines.len()) as i64),
                    thousands(total as i64)
                ),
                DIM,
            )
            .right_aligned(),
        ),
    };
    if content.total == 0 && !facts.is_empty() {
        facts.push(Line::styled(" Nothing to show for this part.", DIM));
    }
    facts.extend(content.lines.into_iter().map(|line| match line.heading {
        true => Line::styled(format!(" {}", line.text), BOLD.patch(ACCENT)),
        false => Line::raw(format!(" {}", line.text)),
    }));

    frame.render_widget(Paragraph::new(vec![Line::from(identity), Line::styled(request, DIM)]), identity_area);
    frame.render_widget(gauge, gauge_area);
    frame.render_widget(Line::styled(usage, DIM), usage_area);
    frame.render_widget(Paragraph::new(notes).wrap(Wrap { trim: false }), notes_area);
    frame.render_widget(Paragraph::new(facts).block(pane), details_area);
    if !app.reading {
        app.tree_table.select(Some(cursor));
        frame.render_stateful_widget(tree, left, &mut app.tree_table);
    }
}

fn draw_subagents(frame: &mut Frame, area: Rect, app: &mut App) {
    let agents = app.snapshot.as_ref().map_or(&[][..], |snapshot| &snapshot.subagents);
    if agents.is_empty() {
        return placeholder(frame, area, "This session has not started any subagents.");
    }
    let rows: Vec<Row> = agents
        .iter()
        .map(|agent| {
            Row::new([
                right(tokens(agent.totals.peak)),
                right(share_of_window(agent.totals.peak, app.window)),
                right(tokens(agent.totals.last)),
                right(agent.totals.requests.to_string()),
                Cell::from(format!("  {}", agent.kind)),
                Cell::from(agent.description.clone()),
            ])
        })
        .collect();
    let header = Row::new([
        right("Peak".into()),
        right("Window".into()),
        right("Latest".into()),
        right("Requests".into()),
        Cell::from("  Type"),
        Cell::from("Task"),
    ]);
    let widths = [
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(18),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header.style(DIM))
        .row_highlight_style(CURSOR)
        .block(Block::bordered().title(" Subagents · each has a context window of its own "));
    frame.render_stateful_widget(table, area, &mut app.subagent_table);
}

fn draw_sessions(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.sessions.is_empty() {
        return placeholder(frame, area, "No sessions are registered on this machine.");
    }
    let now = now_ms();
    let shown = app.snapshot.as_ref().map(|snapshot| &snapshot.transcript);
    let rows: Vec<Row> = app
        .sessions
        .iter()
        .map(|session| {
            let process = if session.running { Cell::from(" ●").style(GOOD) } else { Cell::from(" ○").style(DIM) };
            let row = Row::new([
                process,
                Cell::from(session.status.clone()),
                right(age(now.saturating_sub(session.updated_ms))),
                right(tokens(session.context)),
                right(share_of_window(session.context, app.window)),
                Cell::from(format!("  {}", session.name)),
                Cell::from(tilde(&session.cwd)),
                Cell::from(session.id.chars().take(8).collect::<String>()),
            ]);
            // The session whose context the first tab shows.
            if session.transcript.as_ref() == shown { row.style(BOLD) } else { row }
        })
        .collect();
    let header = Row::new([
        Cell::from(""),
        Cell::from("Status"),
        right("Updated".into()),
        right("Context".into()),
        right("Window".into()),
        Cell::from("  Name"),
        Cell::from("Directory"),
        Cell::from("Session"),
    ]);
    let widths = [
        Constraint::Length(2),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(30),
        Constraint::Min(20),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths)
        .header(header.style(DIM))
        .row_highlight_style(CURSOR)
        .block(Block::bordered().title(" Sessions on this machine · ● process running "));
    frame.render_stateful_widget(table, area, &mut app.session_table);
}

fn draw_hooks(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(report) = &app.hooks else {
        return placeholder(frame, area, "Reading the hook configuration…");
    };
    let lines = hook_lines(report);
    // Let the last line scroll up to the top, no further.
    app.hooks_scroll = app.hooks_scroll.min(lines.len().saturating_sub(1) as u16);
    let title = format!(" Hooks for {} ", tilde(&report.project));
    let text = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((app.hooks_scroll, 0));
    frame.render_widget(text.block(Block::bordered().title(title)), area);
}

fn hook_lines(report: &HookReport) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(scope) = &report.disabled_by {
        lines.push(Line::styled(format!(" disableAllHooks is set in {scope} settings: no hook below will run."), BAD));
        lines.push(Line::raw(""));
    }
    if let Some(scope) = &report.managed_only_by {
        lines.push(Line::styled(format!(" allowManagedHooksOnly is set in {scope} settings: only managed hooks run."), WARN));
        lines.push(Line::raw(""));
    }

    if report.hooks.is_empty() {
        lines.push(Line::styled(" No hooks are configured in any settings file or enabled plugin.", BOLD));
    }
    let mut events: Vec<&str> = report.hooks.iter().map(|hook| hook.event.as_str()).collect();
    events.sort();
    events.dedup();
    for event in events {
        lines.push(Line::styled(format!(" {event}"), BOLD.patch(ACCENT)));
        for hook in report.hooks.iter().filter(|hook| hook.event == event) {
            let matcher = if hook.matcher.is_empty() { "*" } else { &hook.matcher };
            lines.push(Line::from(vec![
                Span::raw(format!("   on {matcher}")),
                Span::styled(format!(" · {} · from {}", hook.kind, hook.scope), DIM),
            ]));
            lines.push(Line::raw(format!("     {}", one_line(&hook.action, 400))));
            if !hook.options.is_empty() {
                let options: Vec<String> = hook.options.iter().map(|(k, v)| format!("{k}={}", one_line(v, 60))).collect();
                lines.push(Line::styled(format!("     {}", options.join("  ")), DIM));
            }
        }
    }

    if !report.scoped.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(" Declared by skills and agents, active only while they run", BOLD));
        for scoped in &report.scoped {
            lines.push(Line::raw(format!("   {}", tilde(&scoped.file))));
            lines.extend(scoped.declaration.lines().map(|line| Line::styled(format!("     {line}"), DIM)));
        }
    }
    if let Some((scope, command)) = &report.status_line {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!(" Status line command, run on every update ({scope} settings)"), BOLD));
        lines.push(Line::raw(format!("   {command}")));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(" Sources, highest precedence first", BOLD));
    for source in &report.sources {
        let (state, style) = match &source.state {
            State::Absent => ("absent".to_string(), DIM),
            State::Unreadable(error) => (format!("UNREADABLE: {error}"), BAD),
            State::Read { hooks: 0 } => ("no hooks".to_string(), Style::new()),
            State::Read { hooks: 1 } => ("1 hook".to_string(), GOOD),
            State::Read { hooks } => (format!("{hooks} hooks"), GOOD),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("   {state:<9} "), style),
            Span::raw(source.scope.clone()),
            Span::styled(format!("  {}", tilde(&source.path)), DIM),
        ]));
        if let Some(target) = &source.link_target {
            lines.push(Line::styled(format!("             symlink to {}", tilde(target)), DIM));
        }
        if let Some(caveat) = source.caveat {
            lines.push(Line::styled(format!("             {caveat}"), WARN));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{Hook, Source};
    use crate::live::LiveSession;
    use crate::tui::app::testing::{app, press};
    use crate::tui::worker::Update;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    fn assert_shows(screen: &str, expected: &[&str]) {
        for text in expected {
            assert!(screen.contains(text), "missing {text:?} in:\n{screen}");
        }
    }

    #[test]
    fn context_tab_shows_the_session_the_tree_and_the_selected_part() {
        let mut app = app();
        let screen = screen(&mut app, 140, 30);
        assert_shows(
            &screen,
            &[
                "1 Context",
                "2 Subagents 1",
                "Session 0123abcd  /work/demo  Demo session",
                "Request #7 · claude-test · 12 messages",
                "1,000 / 1,000,000 tokens · 0.1%",
                "cache read 900 · write 98 · uncached 2",
                "Note: a note worth reading",
                "▾ Tool results",
                "600",
                "60.0%",
                "██████",
                "    Bash",
                "▾ Assistant output",
                // The selected row, Tool results: its weight, then what is in it.
                " Tool results ",
                "600 tokens",
                "60.0% of this context · 0.1% of the window",
                "result of Bash: cargo test · 300 tokens",
                "running 37 tests",
                "result of Read: /work/demo/a.rs · 100 tokens",
                "fn main() {}",
                "piece 1 of 3 · lines 1–10 of 10",
                "Enter read",
                "following the latest session",
            ],
        );

        // Moving onto Bash narrows the content to Bash results.
        press(&mut app, KeyCode::Down);
        let screen = self::screen(&mut app, 140, 30);
        assert_shows(&screen, &[" Tool results › Bash ", "50.0% of this context · 83.3% of Tool results · 0.1% of the window", "Cargo.toml", "piece 1 of 2"]);
        assert!(!screen.contains("result of Read"), "{screen}");

        press(&mut app, KeyCode::Char('w'));
        assert_shows(&self::screen(&mut app, 140, 30), &["1,000 / 200,000 tokens · 0.5%"]);
    }

    #[test]
    fn reading_gives_the_content_the_whole_width_and_scrolls_it() {
        let mut app = app();
        // Too short for the content of Tool results: 4 lines of it fit.
        let screen = self::screen(&mut app, 100, 16);
        assert_shows(&screen, &["▾ Tool results", "test result: ok", "piece 1 of 3 · lines 1–4 of 10"]);
        assert!(!screen.contains("Cargo.toml"), "{screen}");

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        let screen = self::screen(&mut app, 100, 16);
        assert_shows(&screen, &["result of Bash: ls · 200 tokens", "Cargo.toml", "piece 2 of 3 · lines 5–8 of 10", "Esc back to the tree"]);
        assert!(!screen.contains("▾ Tool results") && !screen.contains("running 37 tests"), "{screen}");

        press(&mut app, KeyCode::Esc);
        assert_shows(&self::screen(&mut app, 100, 16), &["▾ Tool results", "Enter read"]);
    }

    #[test]
    fn the_tree_scrolls_to_keep_the_selection_visible() {
        let mut app = app();
        press(&mut app, KeyCode::End);
        // Room for three tree rows only.
        let screen = screen(&mut app, 120, 13);
        assert_shows(&screen, &["block 1: You are", " System prompt › block 1: You are "]);
        assert!(!screen.contains("Tool results"), "{screen}");
    }

    #[test]
    fn other_tabs_show_subagents_sessions_and_hooks() {
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        assert_shows(&screen(&mut app, 120, 12), &["50,000", "5.0%", "40,000", "Explore", "Find the call sites", "Enter open"]);
        // Opened, a subagent is named by its type and task, and Esc leads back.
        press(&mut app, KeyCode::Enter);
        assert_shows(&screen(&mut app, 120, 12), &["Subagent Explore: Find the call sites  /work/demo", "Esc back", "reading…"]);
        press(&mut app, KeyCode::Esc);

        app.apply(Update::Sessions(vec![LiveSession {
            pid: 42,
            id: "feedfacecafe".into(),
            cwd: PathBuf::from("/work/demo"),
            name: "demo-1".into(),
            status: "busy".into(),
            updated_ms: now_ms() - 125_000,
            running: true,
            transcript: None,
            context: Some(250_000),
        }]));
        press(&mut app, KeyCode::Char('3'));
        assert_shows(&screen(&mut app, 120, 12), &["3 Sessions 1", "●", "busy", "2m", "250,000", "25.0%", "demo-1", "/work/demo", "feedface"]);

        app.apply(Update::Hooks(Box::new(HookReport {
            project: PathBuf::from("/work/demo"),
            sources: vec![
                Source { scope: "project".into(), path: PathBuf::from("/work/demo/.claude/settings.json"), link_target: None, state: State::Absent, caveat: None },
                Source { scope: "user".into(), path: PathBuf::from("/home/.claude/settings.json"), link_target: Some(PathBuf::from("/nix/store/x/settings.json")), state: State::Read { hooks: 1 }, caveat: None },
            ],
            hooks: vec![Hook {
                scope: "user".into(),
                file: PathBuf::from("/home/.claude/settings.json"),
                event: "PreToolUse".into(),
                matcher: "Bash".into(),
                kind: "command".into(),
                action: "guard.sh --strict".into(),
                options: vec![("timeout".into(), "10".into())],
            }],
            scoped: Vec::new(),
            disabled_by: Some("project".into()),
            managed_only_by: None,
            status_line: Some(("user".into(), "line.sh".into())),
        })));
        press(&mut app, KeyCode::Char('4'));
        let screen = screen(&mut app, 120, 24);
        assert_shows(
            &screen,
            &[
                "4 Hooks 1",
                "Hooks for /work/demo",
                "disableAllHooks is set in project settings",
                "PreToolUse",
                "on Bash · command · from user",
                "guard.sh --strict",
                "timeout=10",
                "Status line command",
                "line.sh",
                "absent    project",
                "1 hook    user",
                "symlink to /nix/store/x/settings.json",
            ],
        );

        // Scrolling stops with the last line at the top.
        press(&mut app, KeyCode::End);
        let screen = self::screen(&mut app, 120, 24);
        assert!(screen.contains("symlink to") && !screen.contains("PreToolUse"), "{screen}");
        press(&mut app, KeyCode::Home);
        assert_shows(&self::screen(&mut app, 120, 24), &["PreToolUse"]);
    }

    #[test]
    fn empty_and_failed_states_say_what_is_going_on() {
        let mut app = App::new(Target::Latest, 1_000_000);
        assert_shows(&screen(&mut app, 80, 10), &["Reading the session…", "reading…"]);
        app.apply(Update::Context(Err("no session transcripts found under ~/.claude/projects".into())));
        assert_shows(&screen(&mut app, 80, 10), &["no session transcripts found"]);
        for (key, text) in [('2', "has not started any subagents"), ('3', "No sessions are registered"), ('4', "Reading the hook configuration")] {
            press(&mut app, KeyCode::Char(key));
            assert_shows(&screen(&mut app, 80, 10), &[text]);
        }
    }

    #[test]
    fn any_terminal_size_can_be_drawn() {
        let mut app = app();
        for tab in ['1', '2', '3', '4'] {
            press(&mut app, KeyCode::Char(tab));
            for (width, height) in [(1, 1), (10, 3), (30, 8), (300, 80)] {
                screen(&mut app, width, height);
            }
        }
    }
}
