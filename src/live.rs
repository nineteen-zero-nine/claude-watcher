//! What is happening now: the sessions registered on this machine, a
//! session's subagents, and a snapshot of one session's context.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::SystemTime;

use anyhow::Result;
use serde_json::{json, Value};

use crate::context::{self, Report};
use crate::paths::{self, Chosen};
use crate::session::Session;

/// Modification time and size of a file: changes whenever it is appended to.
pub type Stamp = (SystemTime, u64);

pub fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// How much context a transcript's requests used.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub requests: usize,
    /// Context of the latest request that has a response.
    pub last: Option<u64>,
    /// Largest context any request reached.
    pub peak: Option<u64>,
}

/// Totals of transcripts, reread only when a transcript has changed. Reading
/// one means parsing all of it, and a busy machine has dozens.
#[derive(Default)]
pub struct TotalsCache {
    known: HashMap<PathBuf, (Stamp, Totals)>,
}

impl TotalsCache {
    /// Totals of each transcript, `None` where it cannot be read.
    pub fn of(&mut self, transcripts: &[PathBuf]) -> Vec<Option<Totals>> {
        let stamps: Vec<Option<Stamp>> = transcripts.iter().map(|path| stamp(path)).collect();
        let stale: Vec<(&PathBuf, Stamp)> = transcripts
            .iter()
            .zip(&stamps)
            .filter_map(|(path, stamp)| Some((path, (*stamp)?)))
            .filter(|(path, stamp)| self.known.get(*path).is_none_or(|(known, _)| known != stamp))
            .collect();

        let workers = thread::available_parallelism().map_or(4, |n| n.get());
        let per_worker = stale.len().div_ceil(workers).max(1);
        let read: Vec<(PathBuf, Stamp, Totals)> = thread::scope(|scope| {
            let workers: Vec<_> = stale
                .chunks(per_worker)
                .map(|chunk| {
                    scope.spawn(move || {
                        let read = |(path, stamp): &(&PathBuf, Stamp)| {
                            let session = Session::load_usage(path).ok()?;
                            let totals = Totals {
                                requests: session.requests.len(),
                                last: session.latest_context_tokens(),
                                peak: session.peak_context_tokens(),
                            };
                            Some(((*path).clone(), *stamp, totals))
                        };
                        chunk.iter().filter_map(read).collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|worker| worker.join().unwrap_or_default()).collect()
        });
        for (path, stamp, totals) in read {
            self.known.insert(path, (stamp, totals));
        }

        transcripts
            .iter()
            .zip(&stamps)
            .map(|(path, stamp)| {
                let (known, totals) = self.known.get(path)?;
                (Some(known) == stamp.as_ref()).then_some(*totals)
            })
            .collect()
    }
}

/// A session registered under `sessions/<pid>.json`.
#[derive(Clone, Debug)]
pub struct LiveSession {
    pub pid: u64,
    pub id: String,
    pub cwd: PathBuf,
    pub name: String,
    /// `busy`, `idle`, `waiting`, ... as the session reports it.
    pub status: String,
    /// When the session last updated its registration, in ms since the epoch.
    pub updated_ms: u64,
    /// Whether its process still exists.
    pub running: bool,
    pub transcript: Option<PathBuf>,
    /// Context of its latest answered request.
    pub context: Option<u64>,
}

impl LiveSession {
    pub fn to_json(&self) -> Value {
        json!({ "pid": self.pid, "id": self.id, "name": self.name, "cwd": self.cwd, "status": self.status,
                "running": self.running, "updated_ms": self.updated_ms, "context_tokens": self.context,
                "transcript": self.transcript })
    }
}

/// Registered sessions, most recently updated first.
pub fn sessions(cache: &mut TotalsCache) -> Vec<LiveSession> {
    let dir = paths::config_dir().join("sessions");
    let mut sessions: Vec<LiveSession> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|entry| read_json(&entry.path()))
        .filter_map(|record| {
            let id = record["sessionId"].as_str()?.to_string();
            let cwd = PathBuf::from(record["cwd"].as_str().unwrap_or(""));
            let pid = record["pid"].as_u64().unwrap_or(0);
            Some(LiveSession {
                pid,
                transcript: paths::transcript_of(&id, &cwd),
                id,
                cwd,
                name: record["name"].as_str().unwrap_or("-").to_string(),
                status: record["status"].as_str().unwrap_or("?").to_string(),
                updated_ms: record["updatedAt"].as_u64().unwrap_or(0),
                running: is_running(pid),
                context: None,
            })
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));

    let (owners, transcripts): (Vec<usize>, Vec<PathBuf>) = sessions
        .iter()
        .enumerate()
        .filter_map(|(i, s)| Some((i, s.transcript.clone()?)))
        .unzip();
    for (owner, totals) in owners.into_iter().zip(cache.of(&transcripts)) {
        sessions[owner].context = totals.and_then(|t| t.last);
    }
    sessions
}

fn is_running(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// An agent a session delegated to. It has a context window of its own.
#[derive(Clone, Debug)]
pub struct Subagent {
    pub transcript: PathBuf,
    /// `general-purpose`, `Explore`, `fork`, ...
    pub kind: String,
    pub description: String,
    pub totals: Totals,
}

impl Subagent {
    /// What names it among its session's subagents.
    pub fn id(&self) -> String {
        let stem = self.transcript.file_stem().unwrap_or_default().to_string_lossy();
        stem.strip_prefix("agent-").unwrap_or(&stem).to_string()
    }

    pub fn to_json(&self) -> Value {
        json!({ "id": self.id(), "type": self.kind, "description": self.description,
                "requests": self.totals.requests, "last_context_tokens": self.totals.last,
                "peak_context_tokens": self.totals.peak, "transcript": self.transcript })
    }
}

/// The subagent meant by `selector`: its position in the list, from 1, or
/// the start of its id.
pub fn find_subagent<'a>(agents: &'a [Subagent], selector: &str) -> Result<&'a Subagent, String> {
    if let Ok(position) = selector.parse::<usize>() {
        let found = position.checked_sub(1).and_then(|index| agents.get(index));
        return found.ok_or_else(|| format!("there are {} subagents, so no subagent {position}", agents.len()));
    }
    let start = selector.strip_prefix("agent-").unwrap_or(selector);
    let matching: Vec<&Subagent> = agents.iter().filter(|agent| agent.id().starts_with(start)).collect();
    match matching[..] {
        [only] => Ok(only),
        [] => Err(format!("no subagent has an id starting with '{start}'")),
        _ => Err(format!("{} subagents have an id starting with '{start}'; give more of it", matching.len())),
    }
}

/// A session's subagents, largest peak context first.
pub fn subagents(transcript: &Path, cache: &mut TotalsCache) -> Vec<Subagent> {
    let transcripts = paths::subagent_transcripts(transcript);
    let totals = cache.of(&transcripts);
    let mut agents: Vec<Subagent> = transcripts
        .into_iter()
        .zip(totals)
        .filter_map(|(transcript, totals)| {
            let meta = read_json(&transcript.with_extension("meta.json")).unwrap_or_default();
            Some(Subagent {
                kind: meta["agentType"].as_str().unwrap_or("agent").to_string(),
                description: meta["description"].as_str().unwrap_or("").to_string(),
                totals: totals?,
                transcript,
            })
        })
        .collect();
    agents.sort_by_key(|agent| std::cmp::Reverse(agent.totals.peak));
    agents
}

/// Something that, when it differs, means a session or its subagents moved on.
pub fn fingerprint(transcript: &Path) -> Option<(Stamp, Vec<Stamp>)> {
    let own = stamp(transcript)?;
    let agents = paths::subagent_transcripts(transcript).iter().filter_map(|path| stamp(path)).collect();
    Some((own, agents))
}

/// One session's context at a moment in time.
pub struct Snapshot {
    pub transcript: PathBuf,
    /// How the transcript was chosen, when that needs saying.
    pub note: Option<String>,
    /// Absent when the transcript records no API requests.
    pub report: Option<Report>,
    pub subagents: Vec<Subagent>,
}

impl Snapshot {
    pub fn take(chosen: Chosen, cache: &mut TotalsCache) -> Result<Snapshot> {
        let session = Session::load(&chosen.path)?;
        Ok(Snapshot {
            report: context::analyze(&session),
            subagents: subagents(&chosen.path, cache),
            transcript: chosen.path,
            note: chosen.note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::testing::usage_json;
    use std::io::Write;

    #[test]
    fn subagents_are_found_by_position_or_by_the_start_of_their_id() {
        let agent = |id: &str| Subagent {
            transcript: PathBuf::from(format!("/s/subagents/agent-{id}.jsonl")),
            kind: "Explore".into(),
            description: String::new(),
            totals: Totals::default(),
        };
        let agents = [agent("a1b2"), agent("a1c3"), agent("a9")];
        let found = |selector: &str| find_subagent(&agents, selector).map(Subagent::id);
        assert_eq!(found("2").unwrap(), "a1c3");
        assert_eq!(found("a9").unwrap(), "a9");
        assert_eq!(found("agent-a1b").unwrap(), "a1b2");
        assert_eq!(found("0").unwrap_err(), "there are 3 subagents, so no subagent 0");
        assert_eq!(found("4").unwrap_err(), "there are 3 subagents, so no subagent 4");
        assert_eq!(found("a1").unwrap_err(), "2 subagents have an id starting with 'a1'; give more of it");
        assert_eq!(found("b").unwrap_err(), "no subagent has an id starting with 'b'");
        assert_eq!(agents[0].to_json()["id"], "a1b2");
    }

    fn request_and_reply(id: &str, context: u64) -> [Value; 2] {
        [
            json!({ "type": "api-request", "id": id, "keep": 0, "tail": [], "shapeHash": "s" }),
            json!({ "type": "assistant", "requestRef": id,
                    "message": { "id": id, "content": [], "usage": usage_json(context, 1, 0) } }),
        ]
    }

    fn append(path: &Path, records: &[Value]) {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }

    #[test]
    fn totals_are_reread_only_when_a_transcript_changes() {
        let dir = std::env::temp_dir().join(format!("claude-watcher-live-{}", std::process::id()));
        fs::create_dir_all(dir.join("main/subagents")).unwrap();
        let main = dir.join("main.jsonl");
        let agent = dir.join("main/subagents/agent-1.jsonl");
        append(&main, &request_and_reply("r1", 100));
        append(&agent, &request_and_reply("a1", 900));
        append(&agent, &request_and_reply("a2", 400));
        fs::write(agent.with_extension("meta.json"), r#"{"agentType":"Explore","description":"look"}"#).unwrap();

        let mut cache = TotalsCache::default();
        let missing = dir.join("missing.jsonl");
        let totals = cache.of(&[main.clone(), missing]);
        assert_eq!(totals, [Some(Totals { requests: 1, last: Some(100), peak: Some(100) }), None]);

        let agents = subagents(&main, &mut cache);
        assert_eq!(agents.len(), 1);
        assert_eq!((agents[0].kind.as_str(), agents[0].description.as_str()), ("Explore", "look"));
        assert_eq!(agents[0].totals, Totals { requests: 2, last: Some(400), peak: Some(900) });

        // Unchanged files are answered from memory: poison the cached value to prove it.
        cache.known.get_mut(&main).unwrap().1.last = Some(7);
        assert_eq!(cache.of(std::slice::from_ref(&main))[0].unwrap().last, Some(7));

        let before = fingerprint(&main);
        append(&main, &request_and_reply("r2", 250));
        assert_eq!(cache.of(std::slice::from_ref(&main))[0].unwrap().last, Some(250));
        assert_ne!(fingerprint(&main), before);

        let snapshot = Snapshot::take(Chosen { path: main, note: None }, &mut cache).unwrap();
        assert_eq!(snapshot.report.unwrap().total, 250.0);
        assert_eq!(snapshot.subagents.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
