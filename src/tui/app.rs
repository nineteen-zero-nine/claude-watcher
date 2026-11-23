//! What the interface is showing, and how keys change it. Nothing here
//! touches the terminal, so all of it can be driven from tests.

use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

use super::worker::{Command, Target, Update};
use crate::context::{Node, Report};
use crate::hooks::HookReport;
use crate::live::{LiveSession, Snapshot};
use crate::util::{brief, thousands, wrap};

/// Tree rows moved by Page Up and Page Down.
const PAGE: usize = 10;
/// Window sizes the `w` key cycles through, besides the one given at start.
const COMMON_WINDOWS: [u64; 2] = [200_000, 1_000_000];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tab {
    Context,
    Subagents,
    Sessions,
    Hooks,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Context, Tab::Subagents, Tab::Sessions, Tab::Hooks];

    fn index(self) -> usize {
        Tab::ALL.iter().position(|&tab| tab == self).unwrap_or(0)
    }
}

/// A visible line of the breakdown tree.
pub struct Row<'a> {
    /// Names from the top of the tree down to this node.
    pub path: Vec<String>,
    pub node: &'a Node,
    /// Whether its children are shown.
    pub open: bool,
}

/// A line of the selected part's content.
#[derive(Clone, Debug, PartialEq)]
pub struct ContentLine {
    pub text: String,
    /// Whether it introduces a piece rather than being part of one.
    pub heading: bool,
}

/// The lines of the content pane that are on screen, and where they sit.
pub struct ContentWindow {
    pub lines: Vec<ContentLine>,
    /// Index of the first line shown, among `total`.
    pub first: usize,
    pub total: usize,
    /// Which piece the first line belongs to, counting from 1, among `pieces`.
    pub piece: usize,
    pub pieces: usize,
}

/// The selected part's content laid out for one pane width, kept until the
/// selection, the data or the width changes: laying it out means wrapping
/// what can be megabytes of text.
struct Content {
    path: Vec<String>,
    generation: u64,
    width: usize,
    lines: Vec<ContentLine>,
}

pub struct App {
    pub tab: Tab,
    /// Context window size usage is measured against.
    pub window: u64,
    windows: Vec<u64>,
    pub target: Target,
    /// Type and task of the subagent whose context is shown, if it is one.
    pub subagent: Option<String>,
    /// Where to return to from that subagent, innermost last.
    back: Vec<(Target, Option<String>)>,

    pub snapshot: Option<Snapshot>,
    /// The breakdown of `snapshot`, when it has one.
    pub tree: Option<Node>,
    /// Why there is no snapshot.
    pub problem: Option<String>,
    /// Whether a requested reload has not arrived yet.
    pub loading: bool,
    expanded: HashSet<Vec<String>>,
    selected: Vec<String>,
    /// Whether keys scroll the selected part's content instead of the tree.
    pub reading: bool,
    content: Option<Content>,
    content_scroll: usize,
    /// Content lines that fit on screen, as of the last draw.
    content_page: usize,
    /// Counts the snapshots received, to tell when content is out of date.
    generation: u64,

    pub sessions: Vec<LiveSession>,
    pub hooks: Option<HookReport>,

    pub tree_table: TableState,
    pub subagent_table: TableState,
    pub session_table: TableState,
    pub hooks_scroll: u16,
    pub quit: bool,
}

impl App {
    pub fn new(target: Target, window: u64) -> App {
        let mut windows = COMMON_WINDOWS.to_vec();
        if !windows.contains(&window) {
            windows.push(window);
            windows.sort();
        }
        App {
            tab: Tab::Context,
            window,
            windows,
            target,
            subagent: None,
            back: Vec::new(),
            snapshot: None,
            tree: None,
            problem: None,
            loading: true,
            expanded: HashSet::new(),
            selected: Vec::new(),
            reading: false,
            content: None,
            content_scroll: 0,
            content_page: 1,
            generation: 0,
            sessions: Vec::new(),
            hooks: None,
            tree_table: TableState::default(),
            subagent_table: TableState::default().with_selected(0),
            session_table: TableState::default().with_selected(0),
            hooks_scroll: 0,
            quit: false,
        }
    }

    /// Take in fresh data from the worker.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Context(Ok(snapshot)) => {
                let first = self.tree.is_none();
                self.tree = snapshot.report.as_ref().map(|report| report.tree());
                self.snapshot = Some(*snapshot);
                self.problem = None;
                self.loading = false;
                self.generation += 1;
                if let (true, Some(tree)) = (first, &self.tree) {
                    // Start with one level open: an overview that fits a screen.
                    self.expanded = tree.children.iter().map(|node| vec![node.name.clone()]).collect();
                }
                self.settle();
            }
            Update::Context(Err(problem)) => {
                (self.snapshot, self.tree) = (None, None);
                self.problem = Some(problem);
                self.loading = false;
            }
            Update::Sessions(sessions) => self.sessions = sessions,
            Update::Hooks(hooks) => self.hooks = Some(*hooks),
        }
        let subagents = self.subagents();
        clamp(&mut self.subagent_table, subagents);
        clamp(&mut self.session_table, self.sessions.len());
    }

    fn subagents(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |snapshot| snapshot.subagents.len())
    }

    /// Whether there is a session to go back to.
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// The breakdown as currently unfolded, top to bottom.
    pub fn rows(&self) -> Vec<Row<'_>> {
        fn walk<'a>(node: &'a Node, path: &mut Vec<String>, expanded: &HashSet<Vec<String>>, rows: &mut Vec<Row<'a>>) {
            for child in &node.children {
                path.push(child.name.clone());
                let open = !child.children.is_empty() && expanded.contains(path);
                rows.push(Row { path: path.clone(), node: child, open });
                if open {
                    walk(child, path, expanded, rows);
                }
                path.pop();
            }
        }
        let mut rows = Vec::new();
        if let Some(tree) = &self.tree {
            walk(tree, &mut Vec::new(), &self.expanded, &mut rows);
        }
        rows
    }

    /// Index of the selected row among `rows()`.
    pub fn cursor(&self) -> usize {
        self.rows().iter().position(|row| row.path == self.selected).unwrap_or(0)
    }

    /// Keep the selection on a visible row. A row can vanish when the session
    /// moves on or an ancestor is folded; its nearest visible ancestor takes over.
    fn settle(&mut self) {
        let rows = self.rows();
        let mut wanted = self.selected.clone();
        while !wanted.is_empty() && !rows.iter().any(|row| row.path == wanted) {
            wanted.pop();
        }
        if wanted.is_empty() {
            wanted = rows.first().map(|row| row.path.clone()).unwrap_or_default();
        }
        self.selected = wanted;
    }

    /// The part of the selected part's content that fits a pane `width`
    /// characters wide and `height` lines tall, at the current scroll position.
    pub fn content_window(&mut self, width: usize, height: usize) -> ContentWindow {
        let current = self.content.as_ref().is_some_and(|content| {
            content.path == self.selected && content.generation == self.generation && content.width == width
        });
        if !current {
            if self.content.as_ref().is_none_or(|content| content.path != self.selected) {
                self.content_scroll = 0;
            }
            let report = self.snapshot.as_ref().and_then(|snapshot| snapshot.report.as_ref());
            let lines = report.map_or_else(Vec::new, |report| content_lines(report, &self.selected, width));
            self.content = Some(Content { path: self.selected.clone(), generation: self.generation, width, lines });
        }
        let lines = self.content.as_ref().map_or(&[][..], |content| &content.lines);
        self.content_page = height.max(1);
        self.content_scroll = self.content_scroll.min(lines.len().saturating_sub(1));
        let first = self.content_scroll;
        let headings = |lines: &[ContentLine]| lines.iter().filter(|line| line.heading).count();
        ContentWindow {
            lines: lines.iter().skip(first).take(height).cloned().collect(),
            first,
            total: lines.len(),
            piece: headings(&lines[..(first + 1).min(lines.len())]),
            pieces: headings(lines),
        }
    }

    /// Scroll the content, or leave it. Returns whether the key was used.
    fn reading_key(&mut self, code: KeyCode) -> bool {
        let lines = self.content.as_ref().map_or(&[][..], |content| &content.lines);
        let at = self.content_scroll;
        let next = match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                self.reading = false;
                return true;
            }
            // To the next piece, or the previous one.
            KeyCode::Char('n') => lines.iter().skip(at + 1).position(|line| line.heading).map(|skip| at + 1 + skip),
            KeyCode::Char('p') => lines[..at.min(lines.len())].iter().rposition(|line| line.heading),
            code => moved(code, at, lines.len(), self.content_page),
        };
        let used = next.is_some() || matches!(code, KeyCode::Char('n' | 'p'));
        self.content_scroll = next.unwrap_or(at);
        used
    }

    /// Show another context, as a subagent's when `subagent` describes one.
    fn follow(&mut self, target: Target, subagent: Option<String>) -> Option<Command> {
        self.target = target.clone();
        self.subagent = subagent;
        self.tab = Tab::Context;
        self.loading = true;
        // A different context is read from the top.
        self.selected.clear();
        self.reading = false;
        Some(Command::Follow(target))
    }

    /// React to a key. Returns what the worker should do about it, if anything.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if self.reading && self.tab == Tab::Context && self.reading_key(key.code) {
            return None;
        }
        let tabs = Tab::ALL.len();
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % tabs],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + tabs - 1) % tabs],
            KeyCode::Char(digit @ '1'..='4') => self.tab = Tab::ALL[digit as usize - '1' as usize],
            KeyCode::Char('w') => {
                let at = self.windows.iter().position(|&w| w == self.window).unwrap_or(0);
                self.window = self.windows[(at + 1) % self.windows.len()];
            }
            KeyCode::Char('r') => {
                self.loading = true;
                return Some(Command::Refresh);
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some((target, subagent)) = self.back.pop() {
                    return self.follow(target, subagent);
                }
            }
            code => {
                return match self.tab {
                    Tab::Context => {
                        self.tree_key(code);
                        None
                    }
                    Tab::Subagents => self.subagent_key(code),
                    Tab::Sessions => self.session_key(code),
                    Tab::Hooks => {
                        // The view caps the offset at the end of the text.
                        let lines = usize::from(u16::MAX);
                        self.hooks_scroll = moved(code, self.hooks_scroll.into(), lines, PAGE).unwrap_or(self.hooks_scroll.into()) as u16;
                        None
                    }
                };
            }
        }
        None
    }

    fn tree_key(&mut self, code: KeyCode) {
        let rows = self.rows();
        let cursor = rows.iter().position(|row| row.path == self.selected).unwrap_or(0);
        let Some(row) = rows.get(cursor) else { return };
        let (path, foldable, open) = (row.path.clone(), !row.node.children.is_empty(), row.open);
        let path_at = |index: usize| rows.get(index).map(|row| row.path.clone());
        let after_move = moved(code, cursor, rows.len(), PAGE).and_then(path_at);
        let first_child = path_at(cursor + 1).filter(|_| open);

        if let Some(next) = after_move {
            self.selected = next;
            return;
        }
        match (code, first_child) {
            (KeyCode::Enter, _) => self.reading = true,
            (KeyCode::Char(' '), _) if foldable && open => drop(self.expanded.remove(&path)),
            (KeyCode::Char(' ') | KeyCode::Right | KeyCode::Char('l'), _) if foldable && !open => {
                self.expanded.insert(path);
            }
            // On an open node, right steps into it.
            (KeyCode::Right | KeyCode::Char('l'), Some(child)) => self.selected = child,
            (KeyCode::Left | KeyCode::Char('h'), _) if open => drop(self.expanded.remove(&path)),
            // On a folded node or a leaf, left goes up to its parent.
            (KeyCode::Left | KeyCode::Char('h'), _) if path.len() > 1 => self.selected = path[..path.len() - 1].to_vec(),
            (KeyCode::Char('e'), _) => {
                fn open_all(node: &Node, path: &mut Vec<String>, expanded: &mut HashSet<Vec<String>>) {
                    for child in node.children.iter().filter(|child| !child.children.is_empty()) {
                        path.push(child.name.clone());
                        expanded.insert(path.clone());
                        open_all(child, path, expanded);
                        path.pop();
                    }
                }
                if let Some(tree) = &self.tree {
                    open_all(tree, &mut Vec::new(), &mut self.expanded);
                }
            }
            (KeyCode::Char('c'), _) => {
                self.expanded.clear();
                self.settle();
            }
            _ => {}
        }
    }

    fn subagent_key(&mut self, code: KeyCode) -> Option<Command> {
        let cursor = self.subagent_table.selected().unwrap_or(0);
        if let Some(next) = moved(code, cursor, self.subagents(), PAGE) {
            self.subagent_table.select(Some(next));
        } else if code == KeyCode::Enter {
            let agent = self.snapshot.as_ref()?.subagents.get(cursor)?;
            let (transcript, label) = (agent.transcript.clone(), format!("{}: {}", agent.kind, agent.description));
            self.back.push((self.target.clone(), self.subagent.take()));
            return self.follow(Target::Transcript(transcript), Some(label));
        }
        None
    }

    fn session_key(&mut self, code: KeyCode) -> Option<Command> {
        let cursor = self.session_table.selected().unwrap_or(0);
        if let Some(next) = moved(code, cursor, self.sessions.len(), PAGE) {
            self.session_table.select(Some(next));
            return None;
        }
        let target = match code {
            KeyCode::Enter => Target::Transcript(self.sessions.get(cursor)?.transcript.clone()?),
            KeyCode::Char('f') => Target::Latest,
            _ => return None,
        };
        self.back.clear();
        self.follow(target, None)
    }
}

/// The text of everything under `path`, largest piece first, each introduced
/// by a heading that says what it is and how many tokens it takes.
fn content_lines(report: &Report, path: &[String], width: usize) -> Vec<ContentLine> {
    let mut lines = Vec::new();
    for piece in report.pieces(path) {
        let size = format!(" · {} tokens", thousands(piece.tokens.round() as i64));
        let title = brief(&piece.title, width.saturating_sub(size.chars().count()));
        if !lines.is_empty() {
            lines.push(ContentLine { text: String::new(), heading: false });
        }
        lines.push(ContentLine { text: format!("{title}{size}"), heading: true });
        let text = wrap(piece.content.trim_end(), width);
        lines.extend(text.into_iter().map(|text| ContentLine { text, heading: false }));
    }
    lines
}

/// Where a movement key takes a cursor in a list of `len` rows, `page` of
/// which fit on screen.
fn moved(code: KeyCode, cursor: usize, len: usize, page: usize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let by = |delta: isize| cursor.saturating_add_signed(delta).min(last);
    let page = page as isize;
    Some(match code {
        KeyCode::Down | KeyCode::Char('j') => by(1),
        KeyCode::Up | KeyCode::Char('k') => by(-1),
        KeyCode::PageDown => by(page),
        KeyCode::PageUp => by(-page),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    })
}

/// Keep a table's selection inside a list that may have shrunk.
fn clamp(table: &mut TableState, len: usize) {
    let cursor = table.selected().unwrap_or(0);
    table.select(Some(cursor.min(len.saturating_sub(1))));
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::context::{Leaf, Report};
    use crate::live::{Subagent, Totals};
    use crate::session::Usage;
    use std::path::PathBuf;

    pub fn leaf(path: &[&str], tokens: f64, item: Option<&str>, content: &str) -> Leaf {
        Leaf {
            path: path.iter().map(|s| s.to_string()).collect(),
            tokens,
            measured: true,
            item: item.map(str::to_owned),
            content: content.to_string(),
        }
    }

    /// A 1,000-token context: 600 of tool results (Bash 500, Read 100), 300
    /// of assistant output and 100 of system prompt.
    pub fn snapshot(leaves: Vec<Leaf>) -> Snapshot {
        let total = leaves.iter().map(|l| l.tokens).sum();
        Snapshot {
            transcript: PathBuf::from("/projects/demo/0123abcd-session.jsonl"),
            note: None,
            report: Some(Report {
                session_id: "0123abcd-session".into(),
                transcript: PathBuf::from("/projects/demo/0123abcd-session.jsonl"),
                cwd: Some("/work/demo".into()),
                title: Some("Demo session".into()),
                model: "claude-test".into(),
                query_source: "repl_main_thread".into(),
                timestamp: "2026-11-23T10:00:00Z".into(),
                request_number: 7,
                message_count: 12,
                usage: Some(Usage { input: 2, cache_read: 900, cache_creation: 98, output: 40, thinking: 10 }),
                total,
                measured_share: 1.0,
                leaves,
                notes: vec!["a note worth reading".into()],
            }),
            subagents: vec![Subagent {
                transcript: PathBuf::from("/projects/demo/0123abcd-session/subagents/agent-1.jsonl"),
                kind: "Explore".into(),
                description: "Find the call sites".into(),
                totals: Totals { requests: 9, last: Some(40_000), peak: Some(50_000) },
            }],
        }
    }

    pub fn leaves() -> Vec<Leaf> {
        vec![
            leaf(&["Tool results", "Bash"], 300.0, Some("result of Bash: cargo test"), "running 37 tests\ntest result: ok"),
            leaf(&["Tool results", "Bash"], 200.0, Some("result of Bash: ls"), "Cargo.toml\nsrc"),
            leaf(&["Tool results", "Read"], 100.0, Some("result of Read: /work/demo/a.rs"), "fn main() {}"),
            leaf(&["Assistant output", "thinking"], 200.0, None, "Considering the layout of the module"),
            leaf(&["Assistant output", "text"], 100.0, None, "Done."),
            leaf(&["System prompt", "block 1: You are"], 100.0, None, "You are a test agent."),
        ]
    }

    pub fn app() -> App {
        let mut app = App::new(Target::Latest, 1_000_000);
        app.apply(Update::Context(Ok(Box::new(snapshot(leaves())))));
        app
    }

    pub fn press(app: &mut App, code: KeyCode) -> Option<Command> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::path::{Path, PathBuf};

    fn names(app: &App) -> Vec<String> {
        app.rows().iter().map(|row| format!("{}{}", "  ".repeat(row.path.len() - 1), row.node.name)).collect()
    }

    fn selected(app: &App) -> String {
        app.rows()[app.cursor()].path.join(" > ")
    }

    #[test]
    fn starts_one_level_open_with_the_largest_part_selected() {
        let app = app();
        assert_eq!(
            names(&app),
            ["Tool results", "  Bash", "  Read", "Assistant output", "  thinking", "  text", "System prompt", "  block 1: You are"]
        );
        assert_eq!(selected(&app), "Tool results");
        assert!(!app.loading);
    }

    #[test]
    fn arrows_move_fold_and_unfold() {
        let mut app = app();
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), "Tool results > Bash");
        // Left on a leaf goes to its parent; left again folds it.
        press(&mut app, KeyCode::Left);
        assert_eq!(selected(&app), "Tool results");
        press(&mut app, KeyCode::Left);
        assert_eq!(names(&app)[..2], ["Tool results", "Assistant output"]);
        // Right unfolds, then steps inside.
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        assert_eq!(selected(&app), "Tool results > Bash");

        press(&mut app, KeyCode::Char('c'));
        assert_eq!(names(&app), ["Tool results", "Assistant output", "System prompt"]);
        assert_eq!(selected(&app), "Tool results", "selection falls back to the visible ancestor");
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(names(&app).len(), 8);
        press(&mut app, KeyCode::End);
        assert_eq!(selected(&app), "System prompt > block 1: You are");
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), "System prompt > block 1: You are", "stops at the last row");
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(names(&app).len(), 8, "a leaf has nothing to fold");
    }

    #[test]
    fn selection_and_folding_survive_a_refresh() {
        let mut app = app();
        press(&mut app, KeyCode::Char(' ')); // fold Tool results
        for _ in 0..2 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(selected(&app), "Assistant output > thinking");

        // The session moves on: thinking grows past everything, Read results disappear.
        let mut leaves = leaves();
        leaves.retain(|leaf| leaf.path[1] != "Read");
        leaves.push(leaf(&["Assistant output", "thinking"], 5000.0, None, "More thinking"));
        app.apply(Update::Context(Ok(Box::new(snapshot(leaves)))));

        assert_eq!(names(&app)[..4], ["Assistant output", "  thinking", "  text", "Tool results"]);
        assert_eq!(selected(&app), "Assistant output > thinking");
        assert_eq!(app.cursor(), 1);
    }

    fn texts(window: &ContentWindow) -> Vec<&str> {
        window.lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn the_selected_part_shows_its_content_largest_piece_first() {
        let mut app = app();
        let window = app.content_window(60, 100);
        assert_eq!(
            texts(&window),
            [
                "result of Bash: cargo test · 300 tokens",
                "running 37 tests",
                "test result: ok",
                "",
                "result of Bash: ls · 200 tokens",
                "Cargo.toml",
                "src",
                "",
                "result of Read: /work/demo/a.rs · 100 tokens",
                "fn main() {}",
            ]
        );
        assert!(window.lines[0].heading && !window.lines[1].heading);
        assert_eq!((window.first, window.total, window.piece, window.pieces), (0, 10, 1, 3));

        // Parts without an item of their own are named by where they sit.
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), "Assistant output");
        assert_eq!(
            texts(&app.content_window(24, 100)),
            ["thinking · 200 tokens", "Considering the layout", "of the module", "", "text · 100 tokens", "Done."]
        );
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        assert_eq!(texts(&app.content_window(60, 100))[0], "thinking · 200 tokens");
    }

    #[test]
    fn reading_scrolls_the_content_and_esc_returns_to_the_tree() {
        let mut app = app();
        app.content_window(60, 3);
        press(&mut app, KeyCode::Enter);
        assert!(app.reading);

        // Arrows now scroll instead of moving the selection.
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), "Tool results");
        let window = app.content_window(60, 3);
        assert_eq!(texts(&window), ["running 37 tests", "test result: ok", ""]);
        assert_eq!((window.first, window.piece), (1, 1));

        press(&mut app, KeyCode::Char('n'));
        assert_eq!(texts(&app.content_window(60, 3))[0], "result of Bash: ls · 200 tokens");
        press(&mut app, KeyCode::Char('n'));
        let window = app.content_window(60, 3);
        assert_eq!((texts(&window)[0], window.piece), ("result of Read: /work/demo/a.rs · 100 tokens", 3));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.content_window(60, 3).first, 8, "there is no piece after the last");
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.content_window(60, 3).first, 4);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.content_window(60, 3).first, 7, "a page is what fits on screen");
        press(&mut app, KeyCode::End);
        assert_eq!(app.content_window(60, 3).first, 9);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.content_window(60, 3).first, 0);

        // A refresh keeps the place; other keys still work while reading.
        press(&mut app, KeyCode::Down);
        app.apply(Update::Context(Ok(Box::new(snapshot(leaves())))));
        assert_eq!(app.content_window(60, 3).first, 1);
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.window, 200_000);

        assert!(press(&mut app, KeyCode::Esc).is_none());
        assert!(!app.reading);
        press(&mut app, KeyCode::Down);
        assert_eq!(selected(&app), "Tool results > Bash");
        assert_eq!(app.content_window(60, 3).first, 0, "another part is read from its start");
    }

    #[test]
    fn tabs_window_and_quit() {
        let mut app = app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Subagents);
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.tab, Tab::Hooks);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.tab, Tab::Sessions);

        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.window, 200_000);
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.window, 1_000_000);

        assert!(matches!(press(&mut app, KeyCode::Char('r')), Some(Command::Refresh)));
        assert!(app.loading);

        let release = KeyEvent { kind: KeyEventKind::Release, ..KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE) };
        app.on_key(release);
        assert!(!app.quit, "key releases are not presses");
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn opening_a_subagent_and_coming_back() {
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        let command = press(&mut app, KeyCode::Enter);
        let agent = PathBuf::from("/projects/demo/0123abcd-session/subagents/agent-1.jsonl");
        assert!(matches!(command, Some(Command::Follow(Target::Transcript(path))) if path == agent));
        assert_eq!(app.tab, Tab::Context);
        assert!(app.can_go_back() && app.loading);
        assert_eq!(app.subagent.as_deref(), Some("Explore: Find the call sites"));

        let command = press(&mut app, KeyCode::Esc);
        assert!(matches!(command, Some(Command::Follow(Target::Latest))));
        assert!(!app.can_go_back() && app.subagent.is_none());
        assert!(press(&mut app, KeyCode::Esc).is_none(), "nothing further to go back to");
    }

    #[test]
    fn choosing_a_session_follows_its_transcript() {
        let mut app = app();
        let session = |name: &str, transcript: Option<&str>| LiveSession {
            pid: 1,
            id: name.into(),
            cwd: PathBuf::from("/work"),
            name: name.into(),
            status: "idle".into(),
            updated_ms: 0,
            running: true,
            transcript: transcript.map(PathBuf::from),
            context: Some(10),
        };
        app.apply(Update::Sessions(vec![session("one", None), session("two", Some("/t/two.jsonl"))]));
        press(&mut app, KeyCode::End);
        assert_eq!(selected(&app), "System prompt > block 1: You are");
        press(&mut app, KeyCode::Char('3'));
        assert!(press(&mut app, KeyCode::Enter).is_none(), "a session without a transcript cannot be opened");
        press(&mut app, KeyCode::Down);
        let command = press(&mut app, KeyCode::Enter);
        assert!(matches!(command, Some(Command::Follow(Target::Transcript(path))) if path == Path::new("/t/two.jsonl")));
        // Its context arrives and is read from the top, whatever was selected before.
        app.apply(Update::Context(Ok(Box::new(snapshot(leaves())))));
        assert_eq!(selected(&app), "Tool results");

        press(&mut app, KeyCode::Char('3'));
        assert!(matches!(press(&mut app, KeyCode::Char('f')), Some(Command::Follow(Target::Latest))));

        // The list shrinking keeps the cursor on a row.
        app.apply(Update::Sessions(vec![session("one", None)]));
        assert_eq!(app.session_table.selected(), Some(0));
    }

    #[test]
    fn a_failed_load_replaces_the_stale_context() {
        let mut app = app();
        app.apply(Update::Context(Err("cannot open transcript".into())));
        assert!(app.snapshot.is_none() && app.rows().is_empty());
        assert_eq!(app.problem.as_deref(), Some("cannot open transcript"));
        press(&mut app, KeyCode::Down); // nothing to move in: must not panic
    }
}
