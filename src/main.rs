//! claude-watcher: see which hooks Claude Code is configured to run, and what
//! is filling a session's context window.

mod context;
mod hooks;
mod live;
mod paths;
mod session;
mod tui;
mod util;

use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use context::{RenderOptions, DEFAULT_WINDOW};
use live::{Snapshot, TotalsCache};
use paths::Chosen;
use tui::Target;
use util::{age, now_ms, one_line, thousands};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Project whose configuration and sessions to inspect [default: current directory]
    #[arg(long, global = true, value_name = "DIR")]
    project: Option<PathBuf>,

    /// With no command: the interactive view on a terminal, a text report otherwise
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Browse context, subagents, sessions and hooks interactively
    Tui {
        /// Session id (or unique prefix), or path to a transcript [default: follow the project's latest session]
        session: Option<String>,
        /// Context window size, in tokens, to measure usage against
        #[arg(long, default_value_t = DEFAULT_WINDOW)]
        window: u64,
    },
    /// Show the hooks configured in settings files, plugins, skills and agents
    Hooks {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Break down what fills a session's context window, or print what is in it
    Context(ContextArgs),
    /// List a session's subagents; `context --subagent` opens one
    Subagents {
        /// Session id (or unique prefix), or path to a transcript [default: the project's latest session]
        session: Option<String>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// List the Claude Code sessions registered on this machine
    Sessions {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Reprint the context breakdown whenever the session's transcript changes
    Watch {
        #[command(flatten)]
        args: ContextArgs,
        /// Seconds between checks
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
}

#[derive(Args, Clone)]
struct ContextArgs {
    /// Session id (or unique prefix), or path to a transcript [default: the project's latest session]
    session: Option<String>,
    /// Levels of the breakdown to show
    #[arg(long, default_value_t = 3)]
    depth: usize,
    /// Number of largest single items, and of subagents, to list
    #[arg(long, default_value_t = 10)]
    top: usize,
    /// Context window size, in tokens, to measure usage against
    #[arg(long, default_value_t = DEFAULT_WINDOW)]
    window: u64,
    /// Print JSON instead of text
    #[arg(long)]
    json: bool,
    /// Look at one part only: a path such as "tool results/bash", where each
    /// step is a row's name or an unambiguous start of it ("tool/ba")
    #[arg(long, value_name = "PATH")]
    part: Option<String>,
    /// Show every level and every row of the breakdown
    #[arg(long)]
    all: bool,
    /// List the pieces of text the part is made of, largest first
    #[arg(long, conflicts_with_all = ["content", "piece"])]
    pieces: bool,
    /// Print the text of the part, as the model receives it, piece by piece
    #[arg(long)]
    content: bool,
    /// Print the text of one piece only, by its number in --pieces
    #[arg(long, value_name = "N")]
    piece: Option<usize>,
    /// Look at one of the session's subagents instead: its number or id in `subagents`
    #[arg(long, value_name = "ID")]
    subagent: Option<String>,
}

impl ContextArgs {
    /// The short report printed when no command is given.
    fn overview() -> ContextArgs {
        ContextArgs {
            session: None,
            depth: 2,
            top: 5,
            window: DEFAULT_WINDOW,
            json: false,
            part: None,
            all: false,
            pieces: false,
            content: false,
            piece: None,
            subagent: None,
        }
    }

    fn options(&self, part: Vec<String>) -> RenderOptions {
        RenderOptions { depth: self.depth, top: self.top, window: self.window.max(1), part, all: self.all }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project = match &cli.project {
        Some(dir) => fs::canonicalize(dir).with_context(|| format!("no such project directory: {}", dir.display()))?,
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };

    match cli.command {
        None if std::io::stdout().is_terminal() => tui::run(project, Target::Latest, DEFAULT_WINDOW)?,
        None => {
            let args = ContextArgs::overview();
            let hooks = hooks::collect(&project).render();
            emit(&format!("{hooks}\n{}\n{}", "─".repeat(78), context_text(&args, &project)?))?;
        }
        Some(Cmd::Tui { session, window }) => {
            let target = match session {
                Some(selector) => Target::Transcript(paths::resolve_session(Some(&selector), &project)?.path),
                None => Target::Latest,
            };
            tui::run(project, target, window.max(1))?;
        }
        Some(Cmd::Hooks { json }) => {
            let report = hooks::collect(&project);
            if json {
                emit(&format!("{}\n", serde_json::to_string_pretty(&report.to_json())?))?;
            } else {
                emit(&report.render())?;
            }
        }
        Some(Cmd::Context(args)) => emit(&context_text(&args, &project)?)?,
        Some(Cmd::Subagents { session, json }) => emit(&subagents_text(session.as_deref(), &project, json)?)?,
        Some(Cmd::Sessions { json }) => emit(&sessions_text(json)?)?,
        Some(Cmd::Watch { args, interval }) => watch(&args, &project, interval)?,
    }
    Ok(())
}

/// Write to standard output. A reader that stops early (`| head`) is not an
/// error: the rest is simply not wanted.
fn emit(text: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error.into()),
        _ => Ok(()),
    }
}

/// The first characters of a transcript's session or agent id.
fn short_id(transcript: &Path) -> String {
    transcript.file_stem().unwrap_or_default().to_string_lossy().chars().take(8).collect()
}

/// The context report for the selected session, as text or JSON: the
/// breakdown, or the pieces of text behind one part of it.
fn context_text(args: &ContextArgs, project: &Path) -> Result<String> {
    let mut cache = TotalsCache::default();
    let mut chosen = paths::resolve_session(args.session.as_deref(), project)?;
    if let Some(selector) = &args.subagent {
        let session = short_id(&chosen.path);
        let agents = live::subagents(&chosen.path, &mut cache);
        let agent = live::find_subagent(&agents, selector)
            .map_err(|problem| anyhow!("session {session}: {problem} (`claude-watcher subagents` lists them)"))?;
        let what = format!("{}: {}", agent.kind, one_line(&agent.description, 60));
        chosen = Chosen {
            note: Some(format!("subagent {} of session {session} ({what})", agent.id())),
            path: agent.transcript.clone(),
        };
    }
    let snapshot = Snapshot::take(chosen, &mut cache)?;
    let Some(report) = &snapshot.report else {
        let problem = "the transcript records no API requests (it is empty, or predates request logging)";
        return Ok(if args.json {
            format!("{}\n", json!({ "transcript": snapshot.transcript, "error": problem }))
        } else {
            format!("{}: {problem}.\n", paths::tilde(&snapshot.transcript))
        });
    };
    let part = match &args.part {
        Some(selector) => report.tree().find(selector).map_err(|problem| anyhow!("--part: {problem}"))?,
        None => Vec::new(),
    };

    // What is in the part, rather than how much.
    if args.pieces || args.content || args.piece.is_some() {
        return if args.json {
            let document = report.pieces_json(&part, args.piece, !args.pieces).map_err(anyhow::Error::msg)?;
            Ok(format!("{}\n", serde_json::to_string_pretty(&document)?))
        } else if args.pieces {
            Ok(report.render_pieces(&part))
        } else {
            report.render_content(&part, args.piece).map_err(anyhow::Error::msg)
        };
    }

    let whole = part.is_empty();
    let options = args.options(part);
    if args.json {
        let mut document = report.to_json(&options);
        if let Some(note) = &snapshot.note {
            document["note"] = json!(note);
        }
        if whole {
            document["subagents"] = snapshot.subagents.iter().map(live::Subagent::to_json).collect();
        }
        return Ok(format!("{}\n", serde_json::to_string_pretty(&document)?));
    }

    let mut out = String::new();
    if let Some(note) = &snapshot.note {
        let _ = writeln!(out, "Note     {note}");
    }
    out.push_str(&report.render(&options));
    let listed = if args.all { usize::MAX } else { args.top };
    if whole && !snapshot.subagents.is_empty() {
        let _ = writeln!(out, "\nSubagents (each has a context window of its own)");
        let _ = writeln!(out, "     LAST      PEAK  REQUESTS");
        for agent in snapshot.subagents.iter().take(listed) {
            let tokens = |t: Option<u64>| t.map_or("?".to_string(), |t| thousands(t as i64));
            let _ = writeln!(
                out,
                "{:>9} {:>9}  {:>8}  {}: {}",
                tokens(agent.totals.last),
                tokens(agent.totals.peak),
                agent.totals.requests,
                agent.kind,
                one_line(&agent.description, 70)
            );
        }
        if snapshot.subagents.len() > listed {
            let more = snapshot.subagents.len() - listed;
            let _ = writeln!(out, "{:>30}… {more} smaller ones (`claude-watcher subagents` lists them all)", "");
        }
    }
    Ok(out)
}

/// A session's subagents, each with what is needed to open it.
fn subagents_text(session: Option<&str>, project: &Path, as_json: bool) -> Result<String> {
    let chosen = paths::resolve_session(session, project)?;
    let agents = live::subagents(&chosen.path, &mut TotalsCache::default());
    if as_json {
        let document = json!({ "transcript": chosen.path, "note": chosen.note,
                               "subagents": agents.iter().map(live::Subagent::to_json).collect::<Vec<_>>() });
        return Ok(format!("{}\n", serde_json::to_string_pretty(&document)?));
    }

    let session = short_id(&chosen.path);
    let mut out = String::new();
    if let Some(note) = &chosen.note {
        let _ = writeln!(out, "Note     {note}");
    }
    if agents.is_empty() {
        let _ = writeln!(out, "Session {session} has no subagents.");
        return Ok(out);
    }
    let _ = writeln!(out, "Subagents of session {session} (each has a context window of its own)\n");
    let _ = writeln!(out, "  #  {:<17}  {:>9} {:>9}  REQUESTS  TYPE: DESCRIPTION", "ID", "LAST", "PEAK");
    for (index, agent) in agents.iter().enumerate() {
        let tokens = |t: Option<u64>| t.map_or("?".to_string(), |t| thousands(t as i64));
        let _ = writeln!(
            out,
            "{:>3}  {:<17}  {:>9} {:>9}  {:>8}  {}: {}",
            index + 1,
            agent.id(),
            tokens(agent.totals.last),
            tokens(agent.totals.peak),
            agent.totals.requests,
            agent.kind,
            one_line(&agent.description, 70)
        );
    }
    let _ = writeln!(out, "\nOpen one with: claude-watcher context {session} --subagent <# or ID>");
    Ok(out)
}

/// Registered sessions, each with the context size of its latest answered request.
fn sessions_text(as_json: bool) -> Result<String> {
    let sessions = live::sessions(&mut TotalsCache::default());
    if as_json {
        let document: Vec<_> = sessions.iter().map(live::LiveSession::to_json).collect();
        return Ok(format!("{}\n", serde_json::to_string_pretty(&document)?));
    }
    if sessions.is_empty() {
        return Ok(format!("No sessions registered under {}.\n", paths::tilde(&paths::config_dir().join("sessions"))));
    }
    let now = now_ms();
    let mut out = format!(
        "{:<8} {:>7}  {:<8} {:>7}  {:>9}  SESSION   NAME / DIRECTORY\n",
        "PID", "PROCESS", "STATUS", "UPDATED", "CONTEXT"
    );
    for s in &sessions {
        let _ = writeln!(
            out,
            "{:<8} {:>7}  {:<8} {:>7}  {:>9}  {:<8}  {} · {}",
            s.pid,
            if s.running { "running" } else { "gone" },
            s.status,
            age(now.saturating_sub(s.updated_ms)),
            s.context.map_or("-".to_string(), |t| thousands(t as i64)),
            s.id.chars().take(8).collect::<String>(),
            s.name,
            paths::tilde(&s.cwd),
        );
    }
    Ok(out)
}

/// Reprint the report each time the selected transcript changes. With no
/// explicit session this follows the project's latest one as new ones start.
fn watch(args: &ContextArgs, project: &Path, interval: u64) -> Result<()> {
    let mut args = args.clone();
    args.json = false;
    let mut seen = None;
    loop {
        let chosen = paths::resolve_session(args.session.as_deref(), project)?;
        let now = live::stamp(&chosen.path).map(|stamp| (chosen.path.clone(), stamp));
        if now.is_some() && now != seen {
            let text = context_text(&args, project)?;
            // Clear the screen and scrollback, then home the cursor.
            let watching = paths::tilde(&chosen.path);
            emit(&format!("\x1b[2J\x1b[3J\x1b[H{text}\nWatching {watching} · Ctrl-C to stop\n"))?;
            seen = now;
        }
        sleep(Duration::from_secs(interval.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
