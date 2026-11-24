---
name: claude-watcher
description: Inspect a Claude Code session with the claude-watcher CLI - what fills its context window, part by part down to the text itself, what its subagents used, and which hooks are configured. Use when asked why a session's context is large, which tool results, files, skills or MCP servers cost the most tokens, what a subagent consumed, or which hooks run in a project.
---

# Inspecting a session with claude-watcher

`claude-watcher` reads the transcripts Claude Code writes under `~/.claude/projects` and the settings files hooks come from. It only reads. Every command and flag is in `claude-watcher <command> --help`; this skill is the route through them, and what the help does not say.

Not installed? `cargo install --git https://github.com/nineteen-zero-nine/claude-watcher --locked`.

## 1. Pick the session

Name the session in every command: its id, a unique start of its id, or the path of its transcript.

- **Your own session**: `"$CLAUDE_CODE_SESSION_ID"`, which Claude Code sets in your shell.
- **Another session**: `claude-watcher sessions` lists the sessions registered on this machine with the start of their id, their directory and their context size. A session it does not list still opens by its id, when its transcript is under `~/.claude/projects`.

With no session named, you get the latest session of the current directory (`--project DIR` for another one), and when that directory has none, the latest session of any project, under a `Note` line. That is rarely the session you were asked about.

Done when the report's `Session` line shows the id and the directory you meant.

## 2. Read the breakdown

```
claude-watcher context SESSION --depth 1 --top 0     # the header and the top-level parts, about 16 lines
claude-watcher context SESSION                       # three levels and the ten largest single items, about 100 lines
```

Start shallow, then go deeper only where the tokens are. The header says how far to trust the numbers:

- `Context`: the total. `as reported by the API` is exact. `estimated` means the latest request has no response recorded yet, which is common for a session in the middle of a turn: run it again later for the exact figure.
- `% of a 1,000,000 window (assumed)`: the window's size is not in the transcript. Pass `--window 200000` when you know the session's real window, or leave the percentage out of your answer.
- `Split`: the share of the total placed from API usage. The rest, and the shares inside one message, are split by text size: treat those rows as close, not exact.
- A `Dropped by the API` row, with a negative count: a request sent after the prompt cache expired reported a smaller context. It is a correction, not a part, and nothing says what was dropped.
- Any other `Note` line: relay it. The transcript format is internal to Claude Code, and the tool reports what it could not match rather than hiding it.

## 3. Drill into a part

A part is addressed by the rows that lead to it, separated by `/`. Case does not matter, and each step can be cut down to an unambiguous start.

```
claude-watcher context SESSION --part "tool r"                  # Tool results, per tool and MCP server
claude-watcher context SESSION --part "tool r/bash" --pieces    # each Bash result, numbered, largest first
claude-watcher context SESSION --part "tool r/bash" --piece 1   # the text of the largest, as the model received it
```

- `tool` alone is ambiguous (`Tool results`, `Tool definitions`): write `tool r` or `tool d`.
- A step that matches nothing, or several rows, fails with the list of names that exist there. Read that list instead of guessing again.
- Go `--pieces`, then `--piece N`. `--content` prints every piece of the part, which can be hundreds of kilobytes; the `TOKENS` column of `--pieces` tells you what a piece will cost you before you print it.
- Piece text is the session's raw content. Quote only what the question needs.

Done when you can name the pieces that make up the part you were asked about, each with its tokens.

## 4. Subagents

Each subagent has a context window of its own, outside the session's total.

```
claude-watcher subagents SESSION                     # numbered, by peak context
claude-watcher context SESSION --subagent 1          # that subagent's breakdown; --part, --pieces and --piece work the same
```

## 5. Hooks

```
claude-watcher hooks --project DIR
```

Hooks belong to a project, not to a session: for a session's hooks, pass the directory on its `Session` line. The report lists every source, highest precedence first, with its state (`absent`, `no hooks`, `2 hooks`), then the hooks by event, then the status line command. Hooks passed on the command line (`--settings`) cannot be seen, and hooks in a skill's or an agent's frontmatter run only while it does.

## Computing and following

- To sum, filter or compare, add `--json` (on `context`, `subagents`, `sessions` and `hooks`) and compute from it; the text report is for reading.
- To follow a session over time, run `context` again. `watch`, `tui` and the bare `claude-watcher` are for a person at a terminal: `watch` never returns.

## Answering

Give tokens and share for every part you name, say whether the total was exact or estimated, and name pieces by the title `--pieces` gives them, so the reader can open the same piece.
