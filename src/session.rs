//! Reading a Claude Code session transcript and rebuilding its API requests.
//!
//! A transcript (`~/.claude/projects/<project>/<session>.jsonl`) logs every
//! API request as a delta against an earlier one:
//!
//! * `api-request-shape` holds the system prompt and tool definitions, keyed
//!   by `shapeHash`.
//! * `api-request` names its `shapeHash`, a `base` request, how many of the
//!   base's messages to `keep`, and a `tail` of message hashes to append.
//! * A message hash is the SHA-256 of the message's `{content, role}` JSON
//!   with sorted keys. The message itself is either an `api-request-blob`
//!   record or an ordinary `user` / `assistant` transcript record. On the
//!   wire, an assistant turn's per-block records are merged into one message,
//!   and so are adjacent user records (parallel tool results, a tool result
//!   followed by an injected reminder).
//! * `assistant` records carry the API `usage` of the request named by their
//!   `requestRef`.
//!
//! This format is internal to Claude Code and undocumented; everything here
//! is read leniently so that unknown or missing fields degrade the report
//! instead of failing it.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Longest run of adjacent user records tried as a single wire message.
const MAX_MERGED_USER_RECORDS: usize = 8;

/// Token counts reported by the API for one request.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub thinking: u64,
}

impl Usage {
    /// Tokens the request put in the context window.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation
    }

    fn from_json(v: &Value) -> Option<Usage> {
        let field = |key: &str| v.get(key).and_then(Value::as_u64);
        Some(Usage {
            input: field("input_tokens")?,
            cache_read: field("cache_read_input_tokens").unwrap_or(0),
            cache_creation: field("cache_creation_input_tokens").unwrap_or(0),
            output: field("output_tokens").unwrap_or(0),
            thinking: v
                .pointer("/output_tokens_details/thinking_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }
}

#[derive(Debug)]
pub struct Request {
    pub id: String,
    pub base: Option<String>,
    pub keep: usize,
    pub tail: Vec<String>,
    pub shape_hash: String,
    pub model: String,
    pub query_source: String,
    pub timestamp: String,
}

#[derive(Default)]
pub struct Session {
    pub path: PathBuf,
    pub id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub version: Option<String>,
    /// API requests in the order they were made.
    pub requests: Vec<Request>,
    by_id: HashMap<String, usize>,
    /// `shapeHash` -> `{system, tools}`.
    pub shapes: HashMap<String, Value>,
    /// Message hash -> `{role, content}`.
    pub messages: HashMap<String, Value>,
    /// Request id -> usage reported by its response.
    pub usage: HashMap<String, Usage>,
    /// Lines that were not valid JSON (a line still being written counts).
    pub skipped_lines: usize,
}

/// One conversational record, kept only until message hashes are resolved.
enum Turn {
    User(Value),
    Assistant { message_id: String, content: Value },
}

impl Session {
    pub fn load(path: &Path) -> Result<Session> {
        let (mut session, turns) = Session::read(path)?;
        session.resolve_messages(turns);
        Ok(session)
    }

    /// Read only the requests and their usage, leaving messages unresolved.
    /// Enough for context totals, and much cheaper on long transcripts.
    pub fn load_usage(path: &Path) -> Result<Session> {
        Ok(Session::read(path)?.0)
    }

    fn read(path: &Path) -> Result<(Session, Vec<Turn>)> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut session = Session {
            path: path.to_path_buf(),
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..Session::default()
        };
        let mut turns = Vec::new();
        for line in BufReader::new(file).split(b'\n') {
            let line = line.with_context(|| format!("cannot read {}", path.display()))?;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(record) => session.ingest(record, &mut turns),
                Err(_) => session.skipped_lines += 1,
            }
        }
        Ok((session, turns))
    }

    fn ingest(&mut self, mut record: Value, turns: &mut Vec<Turn>) {
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        if self.cwd.is_none() {
            self.cwd = text(&record, "cwd");
        }
        if let Some(version) = text(&record, "version") {
            self.version = Some(version);
        }
        match record.get("type").and_then(Value::as_str).unwrap_or("") {
            "api-request-shape" => {
                if let Some(hash) = text(&record, "shapeHash") {
                    self.shapes.insert(hash, record["shape"].take());
                }
            }
            "api-request-blob" => {
                if let Some(hash) = text(&record, "hash") {
                    self.messages.insert(hash, record["message"].take());
                }
            }
            "api-request" => {
                let Some(id) = text(&record, "id") else { return };
                let tail = record["tail"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                self.by_id.insert(id.clone(), self.requests.len());
                self.requests.push(Request {
                    id,
                    base: text(&record, "base"),
                    keep: record["keep"].as_u64().unwrap_or(0) as usize,
                    tail,
                    shape_hash: text(&record, "shapeHash").unwrap_or_default(),
                    model: text(&record["params"], "model").unwrap_or_default(),
                    query_source: text(&record, "querySource").unwrap_or_default(),
                    timestamp: text(&record, "timestamp").unwrap_or_default(),
                });
            }
            "assistant" => {
                let usage = Usage::from_json(&record["message"]["usage"]);
                if let (Some(request), Some(usage)) = (text(&record, "requestRef"), usage) {
                    // Every block of a response repeats its usage; keep the fullest.
                    let slot = self.usage.entry(request).or_insert(usage);
                    if usage.output >= slot.output {
                        *slot = usage;
                    }
                }
                turns.push(Turn::Assistant {
                    message_id: text(&record["message"], "id").unwrap_or_default(),
                    content: record["message"]["content"].take(),
                });
            }
            "user" => turns.push(Turn::User(record["message"]["content"].take())),
            "ai-title" => self.title = text(&record, "aiTitle"),
            _ => {}
        }
    }

    /// Find the transcript records behind every message hash that has no
    /// blob, by hashing the wire messages those records could have produced.
    fn resolve_messages(&mut self, turns: Vec<Turn>) {
        let mut wanted: HashSet<&str> = HashSet::new();
        for request in &self.requests {
            for hash in &request.tail {
                if !self.messages.contains_key(hash) {
                    wanted.insert(hash);
                }
            }
        }
        if wanted.is_empty() {
            return;
        }

        let mut found: Vec<(String, Value)> = Vec::new();
        let mut offer = |role: &str, content: &Value| {
            let hash = message_hash(role, content);
            if wanted.contains(hash.as_str()) {
                found.push((hash, json!({ "role": role, "content": content })));
            }
        };

        // Assistant: each record alone, and all records of one API message merged.
        let mut merged: Vec<(&str, Vec<Value>)> = Vec::new();
        let mut merged_at: HashMap<&str, usize> = HashMap::new();
        // User: every contiguous stretch of records between assistant turns.
        let mut run: Vec<&Value> = Vec::new();
        let flush = |run: &mut Vec<&Value>, offer: &mut dyn FnMut(&str, &Value)| {
            for start in 0..run.len() {
                let mut blocks: Vec<Value> = Vec::new();
                let end = run.len().min(start + MAX_MERGED_USER_RECORDS);
                for (offset, content) in run[start..end].iter().enumerate() {
                    match content {
                        Value::String(text) => blocks.push(json!({ "type": "text", "text": text })),
                        Value::Array(items) => blocks.extend(items.iter().cloned()),
                        _ => {}
                    }
                    if offset == 0 {
                        offer("user", content);
                    }
                    offer("user", &Value::Array(blocks.clone()));
                }
            }
            run.clear();
        };

        for turn in &turns {
            match turn {
                Turn::User(content) => run.push(content),
                Turn::Assistant { message_id, content } => {
                    flush(&mut run, &mut offer);
                    offer("assistant", content);
                    let at = *merged_at.entry(message_id).or_insert_with(|| {
                        merged.push((message_id, Vec::new()));
                        merged.len() - 1
                    });
                    if let Value::Array(blocks) = content {
                        merged[at].1.extend(blocks.iter().cloned());
                    }
                }
            }
        }
        flush(&mut run, &mut offer);
        for (_, blocks) in merged {
            offer("assistant", &Value::Array(blocks));
        }

        for (hash, message) in found {
            self.messages.entry(hash).or_insert(message);
        }
    }

    pub fn request(&self, id: &str) -> Option<&Request> {
        self.by_id.get(id).map(|&i| &self.requests[i])
    }

    /// The requests a request is built from, oldest first, ending with the
    /// request itself. The flag is false when the oldest one names a base
    /// that is not in this transcript (the session continues another file).
    pub fn chain(&self, index: usize) -> (Vec<&Request>, bool) {
        let mut chain = vec![&self.requests[index]];
        let mut seen: HashSet<&str> = HashSet::from([self.requests[index].id.as_str()]);
        let mut complete = true;
        while let Some(base) = chain.last().and_then(|r| r.base.as_deref()) {
            match self.request(base) {
                Some(request) if seen.insert(&request.id) => chain.push(request),
                _ => {
                    complete = false;
                    break;
                }
            }
        }
        chain.reverse();
        (chain, complete)
    }

    /// Context tokens of the most recent request that has a response.
    pub fn latest_context_tokens(&self) -> Option<u64> {
        self.requests
            .iter()
            .rev()
            .find_map(|r| self.usage.get(&r.id))
            .map(Usage::context)
    }

    /// Largest context any request of this session reached.
    pub fn peak_context_tokens(&self) -> Option<u64> {
        self.requests
            .iter()
            .filter_map(|r| self.usage.get(&r.id))
            .map(Usage::context)
            .max()
    }
}

/// Serialize JSON the way the transcript hashes it: compact, keys sorted.
pub fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            // JavaScript prints integral floats without a fraction.
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => {
                let _ = write!(out, "{}", f as i64);
            }
            _ => {
                let _ = write!(out, "{n}");
            }
        },
        Value::String(s) => push_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(key, out);
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

fn push_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Length of a value's canonical JSON, used to size things that are not text.
pub fn canonical_len(value: &Value) -> usize {
    let mut out = String::new();
    canonical(value, &mut out);
    out.len()
}

pub fn message_hash(role: &str, content: &Value) -> String {
    let mut json = String::from("{\"content\":");
    canonical(content, &mut json);
    json.push_str(",\"role\":");
    push_json_string(role, &mut json);
    json.push('}');
    let digest = Sha256::digest(json.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::io::Write;

    /// Write records to a temporary transcript and load it back.
    pub fn session_from(records: &[Value]) -> Session {
        let lines: Vec<String> = records.iter().map(Value::to_string).collect();
        session_from_lines(&lines)
    }

    pub fn session_from_lines(lines: &[String]) -> Session {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "claude-watcher-test-{}-{n}.jsonl",
            std::process::id()
        ));
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        drop(file);
        let session = Session::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        session
    }

    pub fn usage_json(context: u64, output: u64, thinking: u64) -> Value {
        json!({
            "input_tokens": 10,
            "cache_read_input_tokens": context - 10,
            "cache_creation_input_tokens": 0,
            "output_tokens": output,
            "output_tokens_details": { "thinking_tokens": thinking },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_keeps_unicode() {
        let value = json!({ "b": [1, 2.0, 0.5, null, true], "a": "é\n\"\u{1}" });
        let mut out = String::new();
        canonical(&value, &mut out);
        assert_eq!(out, "{\"a\":\"é\\n\\\"\\u0001\",\"b\":[1,2,0.5,null,true]}");
    }

    #[test]
    fn message_hash_is_sha256_of_the_sorted_compact_message() {
        // Reference from `shasum -a 256` over
        // {"content":[{"text":"hi","type":"text"}],"role":"user"}
        let content = json!([{ "type": "text", "text": "hi" }]);
        assert_eq!(
            message_hash("user", &content),
            "da3ac0b4d79dbe28c3c6684e1e4d037992df0e519e410c56436a23ecadf5c3c1"
        );
        // Stable across key order.
        let reordered = json!([{ "text": "hi", "type": "text" }]);
        assert_eq!(message_hash("user", &content), message_hash("user", &reordered));
        assert_ne!(message_hash("user", &content), message_hash("assistant", &content));
    }

    #[test]
    fn resolves_messages_from_blobs_and_transcript_records() {
        let prompt = json!([{ "type": "text", "text": "list files" }]);
        let thinking = json!({ "type": "thinking", "thinking": "hmm" });
        let call = json!({ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } });
        let result = json!({ "type": "tool_result", "tool_use_id": "t1", "content": "a.txt" });
        let reminder = "<system-reminder>be brief</system-reminder>";

        let h_prompt = message_hash("user", &prompt);
        let h_assistant = message_hash("assistant", &json!([thinking, call]));
        let h_user = message_hash(
            "user",
            &json!([result, { "type": "text", "text": reminder }]),
        );

        let session = session_from(&[
            json!({ "type": "api-request-shape", "shapeHash": "s", "shape": { "system": [], "tools": [] } }),
            json!({ "type": "api-request-blob", "hash": h_prompt, "message": { "role": "user", "content": prompt } }),
            json!({ "type": "api-request", "id": "r1", "keep": 0, "tail": [h_prompt], "shapeHash": "s",
                    "params": { "model": "m" }, "querySource": "repl_main_thread", "cwd": "/work" }),
            // One API message, logged as one record per block.
            json!({ "type": "assistant", "requestRef": "r1",
                    "message": { "id": "msg_1", "content": [thinking], "usage": usage_json(100, 5, 3) } }),
            json!({ "type": "assistant", "requestRef": "r1",
                    "message": { "id": "msg_1", "content": [call], "usage": usage_json(100, 20, 3) } }),
            // A tool result and an injected reminder, sent as one user message.
            json!({ "type": "user", "message": { "role": "user", "content": [result] } }),
            json!({ "type": "user", "isMeta": true, "message": { "role": "user", "content": reminder } }),
            json!({ "type": "api-request", "id": "r2", "base": "r1", "keep": 1, "tail": [h_assistant, h_user],
                    "shapeHash": "s", "params": { "model": "m" }, "querySource": "repl_main_thread" }),
        ]);

        assert_eq!(session.requests.len(), 2);
        assert_eq!(session.cwd.as_deref(), Some("/work"));
        assert_eq!(session.usage["r1"].output, 20, "keeps the fullest usage of a response");
        assert_eq!(session.usage["r1"].context(), 100);
        assert_eq!(session.messages[&h_assistant]["role"], "assistant");
        assert_eq!(session.messages[&h_assistant]["content"].as_array().unwrap().len(), 2);
        assert_eq!(session.messages[&h_user]["content"][1]["text"], reminder);

        let (chain, complete) = session.chain(1);
        assert!(complete);
        assert_eq!(chain.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r1", "r2"]);
    }

    #[test]
    fn a_missing_base_is_reported_and_bad_lines_are_skipped() {
        let session = session_from_lines(&[
            json!({ "type": "api-request", "id": "r9", "base": "gone", "keep": 4, "tail": [], "shapeHash": "s" })
                .to_string(),
            String::new(),
            // A line still being written when the file was read.
            "{\"type\":\"api-request\",\"id\":\"r10\",\"ta".to_string(),
        ]);
        assert_eq!(session.requests.len(), 1);
        assert_eq!(session.skipped_lines, 1);
        let (chain, complete) = session.chain(0);
        assert_eq!(chain.len(), 1);
        assert!(!complete);
    }
}
