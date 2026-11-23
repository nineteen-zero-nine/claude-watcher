//! Where Claude Code keeps its configuration and transcripts.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Result};

pub fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// `~/.claude`, or `$CLAUDE_CONFIG_DIR` when set.
pub fn config_dir() -> PathBuf {
    match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".claude"),
    }
}

pub fn projects_dir() -> PathBuf {
    config_dir().join("projects")
}

/// Managed (enterprise) settings, which take precedence over everything else.
pub fn managed_settings() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

/// Transcripts live in a directory named after the project path, with every
/// character outside `[A-Za-z0-9]` replaced by `-`.
pub fn encode_project_dir(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Shorten a path for display by replacing the home directory with `~`.
pub fn tilde(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn transcripts_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect()
}

fn all_transcripts() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(projects_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|e| transcripts_in(&e.path()))
        .collect()
}

fn newest(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().max_by_key(|p| modified(p))
}

/// A transcript chosen for inspection, and how it was chosen.
pub struct Chosen {
    pub path: PathBuf,
    pub note: Option<String>,
}

/// Resolve the session to inspect: an explicit transcript path, a session id
/// (or unique prefix), or by default the project's most recent session.
pub fn resolve_session(selector: Option<&str>, project: &Path) -> Result<Chosen> {
    if let Some(sel) = selector {
        let as_path = PathBuf::from(sel);
        if as_path.is_file() {
            return Ok(Chosen { path: as_path, note: None });
        }
        let mut hits: Vec<PathBuf> = all_transcripts()
            .into_iter()
            .filter(|p| {
                p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with(sel))
            })
            .collect();
        hits.sort();
        return match hits.len() {
            0 => bail!("no session transcript matches '{sel}' under {}", tilde(&projects_dir())),
            1 => Ok(Chosen { path: hits.remove(0), note: None }),
            n => bail!("'{sel}' matches {n} sessions; give a longer prefix"),
        };
    }

    let project_dir = projects_dir().join(encode_project_dir(project));
    if let Some(path) = newest(transcripts_in(&project_dir)) {
        return Ok(Chosen { path, note: None });
    }
    match newest(all_transcripts()) {
        Some(path) => Ok(Chosen {
            path,
            note: Some(format!(
                "no sessions recorded for {}; showing the most recent session of any project",
                tilde(project)
            )),
        }),
        None => bail!("no session transcripts found under {}", tilde(&projects_dir())),
    }
}

/// The transcript of a session started in `cwd`. A session can outlive its
/// starting directory's name (worktrees, renames), so fall back to its id.
pub fn transcript_of(session_id: &str, cwd: &Path) -> Option<PathBuf> {
    let name = format!("{session_id}.jsonl");
    let expected = projects_dir().join(encode_project_dir(cwd)).join(&name);
    if expected.is_file() {
        return Some(expected);
    }
    fs::read_dir(projects_dir())
        .ok()?
        .flatten()
        .map(|project| project.path().join(&name))
        .find(|candidate| candidate.is_file())
}

/// Subagent transcripts recorded for a session: `<session>/subagents/agent-*.jsonl`.
pub fn subagent_transcripts(session: &Path) -> Vec<PathBuf> {
    let dir = session.with_extension("").join("subagents");
    let mut found = transcripts_in(&dir);
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dirs_replace_every_non_alphanumeric_character() {
        assert_eq!(
            encode_project_dir(Path::new("/Users/me/19h09/claude-watcher")),
            "-Users-me-19h09-claude-watcher"
        );
        assert_eq!(
            encode_project_dir(Path::new("/tmp/work.hRLmNd/ops/.missions/run_1")),
            "-tmp-work-hRLmNd-ops--missions-run-1"
        );
    }
}
