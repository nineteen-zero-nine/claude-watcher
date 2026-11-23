<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="claude-watcher" width="560">
  </picture>
</h1>

Shows two things about a Claude Code setup: which hooks are configured, and
what is filling a session's context window.

https://github.com/user-attachments/assets/7c5041ea-2c48-451a-ad1c-e88b64547252

<p align="center">
  <sub>The film: 78 seconds, with sound · <a href="assets/claude-watcher.en.srt">subtitles</a> · <a href="assets/claude-watcher.mp4">file</a></sub>
</p>

```
cargo install --path .
```

## Commands

```
claude-watcher                    interactive view (a text report when output is piped)
claude-watcher tui [SESSION]      interactive view of one session
claude-watcher hooks              hook configuration from every source
claude-watcher context [SESSION]  context breakdown of one session, or what is in it
claude-watcher watch [SESSION]    the same, reprinted whenever the transcript changes
claude-watcher subagents [SESSION] the session's subagents, by peak context
claude-watcher sessions           sessions registered on this machine, with context size
```

`SESSION` is a session id, a unique prefix of one (as listed by `sessions`), or
a path to a transcript. Without it, the latest session of the project is used.
`--project DIR` selects another project than the current directory. `hooks`,
`context`, `subagents` and `sessions` take `--json`. `tui`, `context` and
`watch` take `--window`.

`context` and `watch` take:

| Option | Effect |
|---|---|
| `--depth N` | levels of the breakdown to print (3) |
| `--all` | every level and every row |
| `--top N` | largest single items, and subagents, to list (10) |
| `--part PATH` | look at one part only |
| `--pieces` | list the pieces of text the part is made of, largest first |
| `--content` | print that text, as the model receives it, piece by piece |
| `--piece N` | print one piece only, by its number in `--pieces` |
| `--subagent ID` | look at a subagent instead: its number or id in `subagents` |

A part is written as the rows leading to it, separated by `/`, in any case.
Each step can be cut down to an unambiguous start:

```
claude-watcher context --part "tool results/bash"      breakdown of Bash results
claude-watcher context --part tool/bash --pieces       which results, and how large
claude-watcher context --part tool/bash --piece 2      the second largest, in full
claude-watcher context --part inj/notes --content      every injected note
claude-watcher context --subagent 1 --part ass/think --content
```

## Interactive view

Four tabs, kept current in the background as transcripts and settings change:

- **Context**: the breakdown as a tree to fold and unfold. Beside it, the
  selected part's share and its content: the text the model actually receives
  for it, piece by piece, largest first. Enter gives the content the whole
  width to read and scroll it.
- **Subagents**: the session's subagents by peak context. Enter opens one's
  own breakdown; Esc returns.
- **Sessions**: every registered session with its context size. Enter opens
  one; `f` goes back to following the project's latest session.
- **Hooks**: the hook configuration described below.

| Key | Action |
|---|---|
| `Tab`, `1`–`4` | switch tab |
| `↑` `↓` `j` `k`, `PgUp` `PgDn`, `g` `G` | move |
| `→` `←` `l` `h`, `Space` | unfold, fold |
| `e`, `c` | unfold all, fold all |
| `Enter` | read the selected part's content; then `↑` `↓` `PgUp` `PgDn` scroll, `n` `p` jump to the next or previous piece, `Esc` returns to the tree |
| `w` | switch the assumed context window (200,000 or 1,000,000) |
| `Esc` | back from reading; back from a subagent to its session |
| `r` | reload now |
| `q` | quit |

Everything the interactive view does is also a command:

| Interactive | Command |
|---|---|
| unfold the tree, `e` | `context --depth N`, `context --all` |
| select a row | `context --part PATH` |
| the content pane | `context --part PATH --pieces`, `--content` |
| `n` `p` between pieces | `context --part PATH --piece N` |
| Subagents tab, Enter | `subagents`, `context --subagent ID` |
| Sessions tab, Enter | `sessions`, `context SESSION` |
| Hooks tab | `hooks` |
| `w` | `--window N` |
| live updates, `r` | `watch` |

## Hooks

`hooks` reads every place a hook can come from, highest precedence first:

- managed settings, `<project>/.claude/settings.local.json`,
  `<project>/.claude/settings.json`, `~/.claude/settings.json`
- `hooks/hooks.json` and the manifest of each enabled plugin
- the frontmatter of skills, agents and commands (shown verbatim; those hooks
  are active only while the skill or agent runs)

It also reports `disableAllHooks`, `allowManagedHooksOnly`, files that fail to
parse, symlinked settings, and the status line command. `CLAUDE_CONFIG_DIR` is
honoured. Hooks passed on the command line (`--settings`) cannot be seen.

## Context

The total is the usage the API reported for the session's latest request, so
it is exact. The split comes from the same numbers:

- Claude Code logs each request as a delta on an earlier one. The growth in
  reported context between two requests is the size of the messages appended
  in between. Of that, the assistant's reply is the output it was billed for,
  thinking included, and the rest is what came back (tool results, prompts,
  reminders).
- What usage cannot separate is split by size in bytes: the blocks within one
  message, messages sent in the same request, and the system prompt, tool
  definitions and opening messages that all arrive in the first request.

The `Split` line of the report says how much of the total was placed from
usage. Rows are named after their cause: tool definitions and tool results per
tool and MCP server, skill and agent listings per entry, CLAUDE.md per file,
reminders (hook output included) by how they open.

Limits:

- The context window size is not in the transcript. 1,000,000 is assumed;
  pass `--window` for anything else.
- When a request is sent after the prompt cache has expired, the API can
  report a smaller context although no message was removed. Usage does not say
  what was dropped, so the difference appears as a `Dropped by the API` row
  rather than being taken from a guess.
- The transcript format is internal to Claude Code and undocumented. It was
  worked out from transcripts written by versions 2.1.323 to 2.1.330, and may
  change. Messages that cannot be matched are reported, not hidden.
