//! Attributing the tokens in a session's context window to what put them there.
//!
//! The total is exact: it is the usage the API reported for the request. The
//! split is derived from the same numbers wherever possible. Each request
//! extends an earlier one, so the growth in reported context between two
//! requests is exactly the size of the messages appended in between; of that
//! growth, the assistant's reply is the output the API billed for it and the
//! rest is the new user-side content (tool results, prompts, reminders).
//!
//! What usage cannot separate is split by size in bytes: the blocks inside one
//! message, messages that arrive in the same request, and the system prompt,
//! tool definitions and opening messages that all arrive in the first request.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::session::{canonical_len, Request, Session, Usage};
use crate::paths;
use crate::util::{brief, one_line, thousands};

/// Context window assumed when none is given. Transcripts do not record it.
pub const DEFAULT_WINDOW: u64 = 1_000_000;

/// Used only when a transcript has no usage to calibrate against.
const FALLBACK_BYTES_PER_TOKEN: f64 = 3.0;
/// Images and documents are base64 in the transcript; their byte length says
/// nothing about tokens, so they are sized as this many bytes of text.
const MEDIA_BYTES: f64 = 4500.0;
/// Characters of a tool call's input or a prompt kept to describe an item.
const ITEM_SUMMARY: usize = 200;
/// Children shown per node below the top level before the rest is summarized.
const MAX_CHILDREN: usize = 12;

const SYSTEM_PROMPT: &str = "System prompt";
const TOOL_DEFINITIONS: &str = "Tool definitions";
const INJECTED: &str = "Injected context";
const REMINDERS: &str = "System reminders";
/// Section of injected context holding mid-conversation notes.
const NOTES: &str = "Notes";
const USER_PROMPTS: &str = "User prompts";
const ASSISTANT: &str = "Assistant output";
const TOOL_RESULTS: &str = "Tool results";
const UNRESOLVED: &str = "Unresolved messages";
const UNATTRIBUTED: &str = "Unattributed";
const DROPPED: &str = "Dropped by the API";

/// A share of the context with a single cause.
#[derive(Debug)]
pub struct Leaf {
    /// Where it sits in the breakdown, outermost group first.
    pub path: Vec<String>,
    pub tokens: f64,
    /// Whether the tokens come from API usage rather than a byte-size estimate.
    pub measured: bool,
    /// Set for individual pieces of conversation worth listing on their own.
    pub item: Option<String>,
    /// The text this share consists of, as the model receives it. Empty when
    /// there is nothing to show.
    pub content: String,
}

/// One stretch of text in the context: a message block, a tool definition,
/// a section of injected text.
pub struct Piece<'a> {
    /// What it is: the call it belongs to, or where it sits in the breakdown.
    pub title: String,
    pub path: &'a [String],
    pub tokens: f64,
    pub content: &'a str,
}

pub struct Report {
    pub session_id: String,
    pub transcript: PathBuf,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: String,
    pub query_source: String,
    pub timestamp: String,
    pub request_number: usize,
    pub message_count: usize,
    /// Usage of the analyzed request, absent while its response is pending.
    pub usage: Option<Usage>,
    pub total: f64,
    /// Fraction of the total placed using API usage.
    pub measured_share: f64,
    pub leaves: Vec<Leaf>,
    pub notes: Vec<String>,
}

/// One message of the request being rebuilt.
struct Slot<'a> {
    message: Option<&'a Value>,
    tokens: Option<f64>,
    measured: bool,
    /// Usage of the response that produced this assistant message.
    produced_by: Option<Usage>,
}

impl<'a> Slot<'a> {
    fn new(message: Option<&'a Value>) -> Self {
        Slot { message, tokens: None, measured: false, produced_by: None }
    }

    fn weight(&self) -> f64 {
        self.message.map_or(0.0, message_weight)
    }

    fn is_assistant(&self) -> bool {
        self.message.is_some_and(|m| m["role"] == "assistant")
    }
}

/// Break down the context of the session's most recent API request.
pub fn analyze(session: &Session) -> Option<Report> {
    let index = session.requests.len().checked_sub(1)?;
    let (chain, complete) = session.chain(index);
    let request = *chain.last()?;
    let shape_of = |r: &Request| session.shapes.get(&r.shape_hash);

    let mut slots: Vec<Slot> = Vec::new();
    // Shape (system prompt + tools) most recently sized from usage, with the
    // bytes-per-token it was sized at.
    let mut calibration: Option<(&str, f64)> = None;
    // The last request whose usage is known, how many messages it had, and
    // whether every request since has only appended to it under the same shape.
    let mut anchor: Option<(&Request, Usage, usize)> = None;
    let mut only_appended = true;
    // Tokens that left the context without any message being removed.
    let mut dropped = 0.0;
    let mut drops = 0;
    let mut previous: Option<&Request> = None;

    for &r in &chain {
        let appended = r.keep == slots.len();
        if r.keep <= slots.len() {
            slots.truncate(r.keep);
        } else {
            // Messages kept from a request this transcript does not contain.
            slots.resize_with(r.keep, || Slot::new(None));
        }
        let first_new = slots.len();
        slots.extend(r.tail.iter().map(|h| Slot::new(session.messages.get(h))));
        if appended {
            if let Some(reply) = slots.get_mut(first_new).filter(|s| s.is_assistant()) {
                reply.produced_by = previous.and_then(|p| session.usage.get(&p.id)).copied();
            }
        }
        if !appended || anchor.is_some_and(|(a, _, _)| a.shape_hash != r.shape_hash) {
            only_appended = false;
        }
        if r.keep == 0 {
            dropped = 0.0;
        }

        // A request without usage (interrupted, or retried) is sized together
        // with whatever follows it, at the next request that has usage.
        if let Some(&now) = session.usage.get(&r.id) {
            match anchor {
                Some((_, before, length)) if only_appended && now.context() >= before.context() => {
                    let new = &mut slots[length..];
                    let reply = new.first().and_then(|s| s.produced_by).map(|u| u.output as f64);
                    distribute(new, (now.context() - before.context()) as f64, reply);
                }
                Some((_, before, length)) if only_appended => {
                    // Messages were only added, yet the context got smaller:
                    // the API dropped something it had been sent before. Usage
                    // cannot say what, so the new messages are sized by bytes
                    // and the shortfall is reported as its own row.
                    let rate = calibration.map_or(FALLBACK_BYTES_PER_TOKEN, |(_, rate)| rate);
                    let mut added = 0.0;
                    for slot in &mut slots[length..] {
                        let tokens = slot.weight() / rate;
                        slot.tokens = Some(tokens);
                        added += tokens;
                    }
                    dropped += before.context() as f64 + added - now.context() as f64;
                    drops += 1;
                }
                _ => {
                    // No clean delta (first request, rewritten history, new
                    // tools): what the already-sized messages do not account
                    // for is the shape plus the unsized messages.
                    let sized: f64 = slots.iter().filter_map(|s| s.tokens).sum();
                    let open: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].tokens.is_none()).collect();
                    let readable = open.iter().all(|&i| slots[i].message.is_some());
                    let open_bytes: f64 = open.iter().map(|&i| slots[i].weight()).sum();
                    let shape_bytes = shape_weight(shape_of(r));
                    let known_shape = calibration
                        .filter(|(hash, _)| *hash == r.shape_hash)
                        .map(|(_, rate)| shape_bytes / rate);
                    let rest = now.context() as f64 - (sized - dropped) - known_shape.unwrap_or(0.0);
                    let bytes = open_bytes + if known_shape.is_some() { 0.0 } else { shape_bytes };
                    if readable && rest > 0.0 && bytes > 0.0 {
                        let bytes_per_token = bytes / rest;
                        for i in open {
                            slots[i].tokens = Some(slots[i].weight() / bytes_per_token);
                            slots[i].measured = true;
                        }
                        if known_shape.is_none() {
                            calibration = Some((&r.shape_hash, bytes_per_token));
                        }
                    }
                }
            }
            anchor = Some((r, now, slots.len()));
            only_appended = true;
        }
        previous = Some(r);
    }

    let bytes_per_token = calibration
        .map(|(_, rate)| rate)
        .or_else(|| observed_bytes_per_token(&slots))
        .unwrap_or(FALLBACK_BYTES_PER_TOKEN);

    let mut leaves = Vec::new();
    match calibration {
        Some((hash, rate)) if hash == request.shape_hash => {
            shape_leaves(shape_of(request), rate, true, &mut leaves)
        }
        _ => shape_leaves(shape_of(request), bytes_per_token, false, &mut leaves),
    }

    let calls = tool_calls(&slots);
    let mut unresolved = 0;
    for slot in &slots {
        let tokens = slot.tokens.unwrap_or(slot.weight() / bytes_per_token);
        match slot.message {
            Some(message) => message_leaves(message, tokens, slot, &calls, &mut leaves),
            None => {
                unresolved += 1;
                let why = "This message is part of the request, but no transcript record matches it, \
                           so its text cannot be shown.";
                leaves.push(leaf(&[UNRESOLVED], tokens, slot.measured, why));
            }
        }
    }

    if dropped >= 1.0 {
        let why = "The API reported a smaller context than the messages sent add up to, although no \
                   message was removed. This happens on requests sent after the prompt cache has \
                   expired. Usage does not say what was dropped (most likely thinking from earlier \
                   turns), so the other rows still count it and this row takes it back out.";
        leaves.push(leaf(&[DROPPED], -dropped, false, why));
    }
    let attributed: f64 = leaves.iter().map(|l| l.tokens).sum();
    let usage = session.usage.get(&request.id).copied();
    let total = match usage {
        Some(usage) => {
            let exact = usage.context() as f64;
            if (exact - attributed).abs() >= 1.0 {
                let why = "The difference between the context the API reported and the sum of the other \
                           rows: parts of the request that could not be sized from usage.";
                leaves.push(leaf(&[UNATTRIBUTED], exact - attributed, true, why));
            }
            exact
        }
        None => attributed,
    };
    let measured: f64 = leaves
        .iter()
        .filter(|l| l.measured && l.path[0] != UNATTRIBUTED)
        .map(|l| l.tokens)
        .sum();

    let mut notes = Vec::new();
    if dropped >= 1.0 {
        notes.push(format!(
            "on {drops} request(s) the context shrank although no message was removed, by {} tokens in all; \
             the API dropped earlier content (likely old thinking) that the rows below still count, \
             so the difference is shown as \"{DROPPED}\"",
            thousands(dropped.round() as i64)
        ));
    }
    if usage.is_none() {
        notes.push("the latest request has no response recorded yet, so its total is an estimate".into());
    }
    if !complete {
        notes.push("this session continues one whose transcript is elsewhere; its earlier messages cannot be read".into());
    }
    if unresolved > 0 {
        notes.push(format!(
            "{unresolved} of {} messages could not be matched to transcript records",
            slots.len()
        ));
    }
    if shape_of(request).is_none() {
        notes.push("the system prompt and tool definitions of this request are not in the transcript".into());
    }
    if session.skipped_lines > 0 {
        notes.push(format!("{} unreadable transcript lines were skipped", session.skipped_lines));
    }

    Some(Report {
        session_id: session.id.clone(),
        transcript: session.path.clone(),
        cwd: session.cwd.clone(),
        title: session.title.clone(),
        model: request.model.clone(),
        query_source: request.query_source.clone(),
        timestamp: request.timestamp.clone(),
        request_number: session.requests.len(),
        message_count: slots.len(),
        usage,
        total,
        measured_share: if total > 0.0 && measured > 0.0 { (measured / total).min(1.0) } else { 0.0 },
        leaves,
        notes,
    })
}

/// Give `growth` tokens to the messages appended since the previous request.
/// The reply the previous request produced takes what the API billed for it.
fn distribute(tail: &mut [Slot], mut growth: f64, reply: Option<f64>) {
    let mut rest = tail;
    if let Some(reply) = reply.filter(|&r| r <= growth && rest.len() > 1) {
        let (first, others) = rest.split_at_mut(1);
        first[0].tokens = Some(reply);
        first[0].measured = true;
        growth -= reply;
        rest = others;
    }
    let bytes: f64 = rest.iter().map(Slot::weight).sum();
    let count = rest.len() as f64;
    for slot in rest {
        slot.tokens = Some(if bytes > 0.0 { growth * slot.weight() / bytes } else { growth / count });
        slot.measured = true;
    }
}

fn observed_bytes_per_token(slots: &[Slot]) -> Option<f64> {
    let (bytes, tokens) = slots
        .iter()
        .filter(|s| s.measured)
        .fold((0.0, 0.0), |(b, t), s| (b + s.weight(), t + s.tokens.unwrap_or(0.0)));
    (bytes > 0.0 && tokens > 0.0).then(|| bytes / tokens)
}

fn leaf(path: &[&str], tokens: f64, measured: bool, content: &str) -> Leaf {
    let path = path.iter().map(|s| s.to_string()).collect();
    Leaf { path, tokens, measured, item: None, content: content.to_string() }
}

fn block_type(block: &Value) -> &str {
    block.get("type").and_then(Value::as_str).unwrap_or("")
}

fn is_tool_call(kind: &str) -> bool {
    kind.ends_with("tool_use")
}

fn is_tool_result(kind: &str) -> bool {
    kind.ends_with("tool_result")
}

fn is_thinking(kind: &str) -> bool {
    kind == "thinking" || kind == "redacted_thinking"
}

/// Message content as a list of blocks; a bare string is one text block.
fn blocks(content: &Value) -> Vec<Cow<'_, Value>> {
    match content {
        Value::String(text) => vec![Cow::Owned(json!({ "type": "text", "text": text }))],
        Value::Array(items) => items.iter().map(Cow::Borrowed).collect(),
        _ => Vec::new(),
    }
}

/// Size of a block in bytes of model-visible text, the proxy for its tokens.
fn block_weight(block: &Value) -> f64 {
    let text = |key: &str| block.get(key).and_then(Value::as_str).map_or(0, str::len) as f64;
    match block_type(block) {
        "text" => text("text"),
        "thinking" => text("thinking"),
        // Encrypted; its length is unrelated to what the model reads.
        "redacted_thinking" => 1.0,
        "image" | "document" => MEDIA_BYTES,
        kind if is_tool_call(kind) => text("name") + canonical_len(&block["input"]) as f64,
        kind if is_tool_result(kind) => content_weight(&block["content"]),
        _ => canonical_len(block) as f64,
    }
}

fn content_weight(content: &Value) -> f64 {
    match content {
        Value::Null => 0.0,
        Value::String(text) => text.len() as f64,
        Value::Array(items) => items.iter().map(block_weight).sum(),
        other => canonical_len(other) as f64,
    }
}

fn message_weight(message: &Value) -> f64 {
    content_weight(&message["content"]).max(1.0)
}

fn system_blocks(shape: Option<&Value>) -> &[Value] {
    shape
        .and_then(|s| s["system"].as_array())
        .map_or(&[], Vec::as_slice)
}

fn tools(shape: Option<&Value>) -> &[Value] {
    shape
        .and_then(|s| s["tools"].as_array())
        .map_or(&[], Vec::as_slice)
}

fn system_block_weight(block: &Value) -> f64 {
    block["text"].as_str().map_or(0, str::len) as f64
}

fn shape_weight(shape: Option<&Value>) -> f64 {
    let system: f64 = system_blocks(shape).iter().map(system_block_weight).sum();
    let tools: f64 = tools(shape).iter().map(|t| canonical_len(t) as f64).sum();
    system + tools
}

fn shape_leaves(shape: Option<&Value>, bytes_per_token: f64, measured: bool, out: &mut Vec<Leaf>) {
    for (i, block) in system_blocks(shape).iter().enumerate() {
        let label = format!("block {}: {}", i + 1, one_line(block["text"].as_str().unwrap_or(""), 44));
        out.push(Leaf {
            path: vec![SYSTEM_PROMPT.into(), label],
            tokens: system_block_weight(block) / bytes_per_token,
            measured,
            item: None,
            content: block["text"].as_str().unwrap_or("").to_string(),
        });
    }
    for tool in tools(shape) {
        let mut path = vec![TOOL_DEFINITIONS.to_string()];
        path.extend(tool_path(tool["name"].as_str().unwrap_or("(unnamed)")));
        let tokens = canonical_len(tool) as f64 / bytes_per_token;
        out.push(Leaf { path, tokens, measured, item: None, content: tool_text(tool) });
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// A tool definition as prose: its description, then its input schema.
fn tool_text(tool: &Value) -> String {
    let description = tool["description"].as_str().unwrap_or("");
    match tool.get("input_schema") {
        Some(schema) => format!("{description}\n\nInput schema:\n{}", pretty(schema)),
        None => description.to_string(),
    }
}

/// A tool call's input, one field after another: short fields first, then
/// the long ones. String fields are shown as they are rather than
/// JSON-escaped, so that a file being written reads as the file.
fn input_text(input: &Value) -> String {
    let Some(fields) = input.as_object() else { return pretty(input) };
    let (mut short, mut long) = (String::new(), String::new());
    for (name, value) in fields {
        match value {
            Value::String(value) if value.contains('\n') => {
                let _ = writeln!(long, "\n{name}:\n{}", value.trim_end());
            }
            Value::String(value) => {
                let _ = writeln!(short, "{name}: {value}");
            }
            other => {
                let _ = writeln!(short, "{name}: {other}");
            }
        }
    }
    format!("{short}{long}").trim().to_string()
}

/// What a tool returned: its text, with anything that is not text named.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match (block_type(part), part["text"].as_str()) {
                ("text", Some(text)) => text.to_string(),
                ("image" | "document", _) => format!("[{}]", block_type(part)),
                _ => pretty(part),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => pretty(other),
    }
}

/// `mcp__server__tool` -> `["mcp: server", "tool"]`; anything else is built in.
fn tool_path(name: &str) -> Vec<String> {
    match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, tool)) => vec![format!("mcp: {server}"), tool.to_string()],
        None => vec![name.to_string()],
    }
}

struct Call {
    name: String,
    summary: String,
}

/// Every tool call in the conversation, by `tool_use` id.
fn tool_calls<'a>(slots: &[Slot<'a>]) -> HashMap<&'a str, Call> {
    const TELLING_INPUTS: [&str; 11] = [
        "file_path", "path", "notebook_path", "command", "pattern", "query", "url", "skill",
        "subagent_type", "description", "prompt",
    ];
    let home = paths::home().to_string_lossy().into_owned();
    let mut calls = HashMap::new();
    for message in slots.iter().filter_map(|s| s.message) {
        let Some(content) = message["content"].as_array() else { continue };
        for block in content.iter().filter(|b| is_tool_call(block_type(b))) {
            let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) else { continue };
            let input = &block["input"];
            let summary = TELLING_INPUTS
                .iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .map(str::to_owned)
                .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
            // Paths under the home directory read better, and shorter, as `~/...`.
            let summary = if home.is_empty() { summary } else { summary.replace(&home, "~") };
            calls.insert(id, Call { name: name.to_string(), summary: one_line(&summary, ITEM_SUMMARY) });
        }
    }
    calls
}

fn message_leaves(message: &Value, tokens: f64, slot: &Slot, calls: &HashMap<&str, Call>, out: &mut Vec<Leaf>) {
    let role = message["role"].as_str().unwrap_or("");
    let all = blocks(&message["content"]);
    if all.is_empty() {
        if tokens >= 0.5 {
            out.push(leaf(&["Other content", role], tokens, slot.measured, &pretty(&message["content"])));
        }
        return;
    }
    let weights: Vec<f64> = all.iter().map(|b| block_weight(b)).collect();
    let thinking: Vec<bool> = all.iter().map(|b| is_thinking(block_type(b))).collect();

    // The API reports how much of a reply was thinking; use that rather than
    // the size of the recorded thinking text, which may be a summary.
    let thinking_budget = slot
        .produced_by
        .filter(|usage| usage.thinking > 0 && thinking.contains(&true) && thinking.contains(&false))
        .map(|usage| (usage.thinking as f64).min(tokens));

    let pool = |want_thinking: Option<bool>| -> (f64, f64) {
        let members = (0..all.len()).filter(|&i| want_thinking.is_none_or(|w| thinking[i] == w));
        members.fold((0.0, 0.0), |(bytes, count), i| (bytes + weights[i], count + 1.0))
    };
    for (i, block) in all.iter().enumerate() {
        let (budget, (bytes, count)) = match thinking_budget {
            Some(spent) if thinking[i] => (spent, pool(Some(true))),
            Some(spent) => (tokens - spent, pool(Some(false))),
            None => (tokens, pool(None)),
        };
        let share = if bytes > 0.0 { budget * weights[i] / bytes } else { budget / count };
        block_leaves(role, block, share, slot.measured, calls, out);
    }
}

fn block_leaves(role: &str, block: &Value, tokens: f64, measured: bool, calls: &HashMap<&str, Call>, out: &mut Vec<Leaf>) {
    let kind = block_type(block);
    let text = block["text"].as_str().unwrap_or("");
    let mut push = |path: Vec<String>, tokens: f64, item: Option<String>, content: String| {
        out.push(Leaf { path, tokens, measured, item, content });
    };
    let sectioned = |sections: BTreeMap<Vec<String>, String>, group: &str, push: &mut dyn FnMut(Vec<String>, f64, Option<String>, String)| {
        let bytes: usize = sections.values().map(String::len).sum();
        for (section, content) in sections {
            let mut path = vec![group.to_string()];
            path.extend(section);
            push(path, tokens * content.len() as f64 / bytes.max(1) as f64, None, content);
        }
    };

    match (role, kind) {
        ("system", "text") => sectioned(injected_sections(text), INJECTED, &mut push),
        ("user", "text") if text.trim_start().starts_with("<system-reminder>") => {
            sectioned(reminder_sections(text), REMINDERS, &mut push)
        }
        ("user", "text") => push(
            vec![USER_PROMPTS.into()],
            tokens,
            Some(format!("user prompt: {}", one_line(text, ITEM_SUMMARY))),
            text.to_string(),
        ),
        (_, "text") => push(vec![ASSISTANT.into(), "text".into()], tokens, None, text.to_string()),
        (_, kind) if is_thinking(kind) => {
            let recorded = block["thinking"].as_str().unwrap_or("").trim();
            let content = match recorded {
                "" => "The transcript does not record the text of this thinking, only that it happened.",
                recorded => recorded,
            };
            push(vec![ASSISTANT.into(), "thinking".into()], tokens, None, content.to_string());
        }
        (_, kind) if is_tool_call(kind) => {
            let name = block["name"].as_str().unwrap_or("(unnamed)");
            let mut path = vec![ASSISTANT.to_string(), "tool calls".to_string()];
            path.extend(tool_path(name));
            let summary = block["id"].as_str().and_then(|id| calls.get(id)).map_or("", |c| c.summary.as_str());
            push(path, tokens, Some(format!("call to {name}: {summary}")), input_text(&block["input"]));
        }
        (_, kind) if is_tool_result(kind) => {
            let call = block["tool_use_id"].as_str().and_then(|id| calls.get(id));
            let name = call.map_or("(unknown tool)", |c| c.name.as_str());
            let mut path = vec![TOOL_RESULTS.to_string()];
            path.extend(tool_path(name));
            let summary = call.map_or("", |c| c.summary.as_str());
            push(path, tokens, Some(format!("result of {name}: {summary}")), result_text(&block["content"]));
        }
        (_, "tool_addition") => {
            let definition = &block["tool"]["definition"];
            let mut path = vec![TOOL_DEFINITIONS.to_string()];
            path.extend(tool_path(definition["name"].as_str().unwrap_or("(unnamed)")));
            push(path, tokens, None, tool_text(definition));
        }
        (_, "tool_removal") => {
            let name = block["tool"]["name"].as_str().unwrap_or("(unnamed)");
            push(vec![TOOL_DEFINITIONS.into(), "(removal notices)".into()], tokens, None, format!("Removed: {name}"));
        }
        (_, "image") => push(vec!["Images".into()], tokens, Some("image".into()), "[image]".into()),
        (_, "document") => push(vec!["Documents".into()], tokens, Some("document".into()), "[document]".into()),
        (_, other) => push(vec!["Other content".into(), format!("{role} {other}")], tokens, None, pretty(block)),
    }
}

/// Split the text the harness injects as system messages into its sections,
/// returning the text of each. The text is prose with a few stable
/// landmarks: `# Heading` lines and the sentences that introduce each listing.
/// Text before any landmark is a mid-conversation note (a nudge, a file-change
/// notice, diagnostics), filed under how it opens.
fn injected_sections(text: &str) -> BTreeMap<Vec<String>, String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Plain,
        /// `- name: description` entries (skills, agent types).
        Listing,
        /// One tool name per line.
        ToolNames,
        /// `## server` subsections of server-written prose.
        McpInstructions,
    }
    const LEADS: [(&str, &str, Mode); 4] = [
        ("The following deferred tools are now available", "Deferred tool names", Mode::ToolNames),
        ("Available agent types for the Agent tool", "Agent types", Mode::Listing),
        ("The following skills are available", "Skill listing", Mode::Listing),
        ("Today's date is", "Date", Mode::Plain),
    ];

    let mut sections: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut section = NOTES.to_string();
    let mut entry: Option<String> = None;
    let mut mode = Mode::Plain;
    let mut after_blank = true;
    let mut fenced = false;

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced {
            let lead = LEADS.iter().find(|(lead, _, _)| after_blank && trimmed.starts_with(lead));
            if let Some((_, label, lead_mode)) = lead {
                (section, entry, mode) = (label.to_string(), None, *lead_mode);
            } else if let (Some(heading), true) = (trimmed.strip_prefix("# "), mode != Mode::McpInstructions) {
                // Server-written instructions may contain headings of their own,
                // so inside them only a lead sentence starts a new section.
                mode = if heading == "MCP Server Instructions" { Mode::McpInstructions } else { Mode::Plain };
                (section, entry) = (heading.to_string(), None);
            } else {
                match mode {
                    Mode::McpInstructions => {
                        if let Some(server) = trimmed.strip_prefix("## ") {
                            entry = Some(server.to_string());
                        }
                    }
                    Mode::Listing => {
                        if let Some((name, _)) = trimmed.strip_prefix("- ").and_then(|rest| rest.split_once(':')) {
                            entry = Some(name.trim().to_string());
                        }
                    }
                    Mode::ToolNames => {
                        if let Some((server, _)) = trimmed.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
                            entry = Some(format!("mcp: {server}"));
                        } else if !trimmed.is_empty() && !trimmed.contains(' ') {
                            entry = Some("built-in".to_string());
                        }
                    }
                    Mode::Plain if section == NOTES => {
                        if let Some(tag) = opening_tag(trimmed).filter(|_| after_blank) {
                            entry = Some(tag.to_string());
                        } else if entry.is_none() && !trimmed.is_empty() {
                            entry = Some(note_label(trimmed));
                        }
                    }
                    Mode::Plain => {}
                }
            }
        }
        let mut path = vec![section.clone()];
        path.extend(entry.clone());
        sections.entry(path).or_default().push_str(line);
        after_blank = trimmed.is_empty();
    }
    sections
}

/// `<name>` at the start of a line, as in `<new-diagnostics>The following...`.
fn opening_tag(line: &str) -> Option<&str> {
    let (name, _) = line.strip_prefix('<')?.split_once('>')?;
    let is_name = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    is_name.then_some(name)
}

/// How a note opens, with paths blanked so that notes of one kind group together.
fn note_label(line: &str) -> String {
    let words: Vec<&str> = line
        .split_whitespace()
        .map(|word| if word.contains('/') { "<path>" } else { word })
        .collect();
    one_line(&words.join(" "), 44)
}

/// Split a `<system-reminder>` into its sections. The session-context reminder
/// is a list of `# key` sections (`claudeMd`, `gitStatus`, ...). Memory files
/// (CLAUDE.md and the like) are each introduced by a `Contents of <path>` line,
/// under `claudeMd` or in a reminder of their own. Any other reminder is
/// labelled by how it begins.
fn reminder_sections(text: &str) -> BTreeMap<Vec<String>, String> {
    const CONTEXT_KEYS: [&str; 5] = ["claudeMd", "gitStatus", "userEmail", "currentDate", "important-instruction-reminders"];
    let body = text.trim_start().trim_start_matches("<system-reminder>");
    let keyed = text.lines().any(|line| CONTEXT_KEYS.iter().any(|key| line.strip_prefix("# ") == Some(key)));
    let opening = if keyed {
        "(lead-in)".to_string()
    } else {
        note_label(body.lines().find(|line| !line.trim().is_empty()).unwrap_or(""))
    };

    let mut sections: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut key: Option<String> = None;
    let mut file: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        // Context keys are camelCase words; anything else is a heading in prose.
        let heading = trimmed
            .strip_prefix("# ")
            .filter(|h| h.starts_with(|c: char| c.is_ascii_lowercase()) && !h.contains(char::is_whitespace));
        match heading {
            // Memory files have headings of their own; only a known key ends them.
            Some(heading) if key.as_deref() != Some("claudeMd") || CONTEXT_KEYS.contains(&heading) => {
                (key, file) = (Some(heading.to_string()), None);
            }
            _ => {
                if let Some(rest) = trimmed.strip_prefix("Contents of /") {
                    let path = rest.split(" (").next().unwrap_or(rest).trim_end_matches(':');
                    (key, file) = (Some("claudeMd".to_string()), Some(format!("/{path}")));
                }
            }
        }
        let mut path = vec![key.clone().unwrap_or_else(|| opening.clone())];
        path.extend(file.clone());
        sections.entry(path).or_default().push_str(line);
    }
    sections
}

/// A named share of the context, with the shares it is made of, largest first.
#[derive(Debug)]
pub struct Node {
    pub name: String,
    pub tokens: f64,
    pub children: Vec<Node>,
}

impl Node {
    /// Sum the leaves into a tree along their paths. The root is the whole context.
    fn from_leaves(leaves: &[Leaf]) -> Node {
        #[derive(Default)]
        struct Tally {
            tokens: f64,
            children: BTreeMap<String, Tally>,
        }
        fn into_node(name: String, tally: Tally) -> Node {
            let mut children: Vec<Node> = tally.children.into_iter().map(|(name, child)| into_node(name, child)).collect();
            children.sort_by(|a, b| b.tokens.total_cmp(&a.tokens));
            Node { name, tokens: tally.tokens, children }
        }

        let mut root = Tally::default();
        for leaf in leaves {
            root.tokens += leaf.tokens;
            let mut tally = &mut root;
            for part in &leaf.path {
                tally = tally.children.entry(part.clone()).or_default();
                tally.tokens += leaf.tokens;
            }
        }
        into_node("context".to_string(), root)
    }

    /// The node at `path`, counted from this one.
    pub fn at(&self, path: &[String]) -> Option<&Node> {
        path.iter().try_fold(self, |node, name| node.children.iter().find(|child| &child.name == name))
    }

    /// Resolve a part written by hand, such as `tool results/bash` or
    /// `inj/notes`, to its path from this node. Each `/`-separated step is a
    /// child's name in any case (names may themselves contain `/`), or failing
    /// that the start of exactly one child's name. A name that begins with
    /// `/` follows a doubled separator: `sys/claudemd//path/to/CLAUDE.md`.
    pub fn find(&self, selector: &str) -> Result<Vec<String>, String> {
        let selector = selector.to_lowercase();
        let mut rest = selector.trim().trim_matches('/');
        let mut node = self;
        let mut path: Vec<String> = Vec::new();
        while !rest.is_empty() {
            let place = if path.is_empty() { "the context".to_string() } else { path.join(" › ") };
            let list = |nodes: &[&Node]| nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(", ");
            let named = node
                .children
                .iter()
                .filter(|child| {
                    let name = child.name.to_lowercase();
                    rest.strip_prefix(name.as_str()).is_some_and(|after| after.is_empty() || after.starts_with('/'))
                })
                .max_by_key(|child| child.name.len());
            let (child, used) = match named {
                Some(child) => (child, child.name.to_lowercase().len()),
                None => {
                    let starting = |text: &str| -> Vec<&Node> {
                        node.children.iter().filter(|child| child.name.to_lowercase().starts_with(text)).collect()
                    };
                    // All that is left may be the start of a name with slashes in it.
                    let whole = starting(rest);
                    let step = rest.split('/').next().filter(|step| !step.is_empty()).unwrap_or(rest);
                    let starting = if whole.len() == 1 { whole } else { starting(step) };
                    let step = if whole_is(&starting, rest) { rest } else { step };
                    match starting[..] {
                        [only] => (only, step.len()),
                        [] if node.children.is_empty() => return Err(format!("{place} has no parts, so no '{step}'")),
                        [] => {
                            let all: Vec<&Node> = node.children.iter().collect();
                            return Err(format!("no part '{step}' in {place}, which has: {}", list(&all)));
                        }
                        _ => return Err(format!("'{step}' could be several parts of {place}: {}", list(&starting))),
                    }
                }
            };
            path.push(child.name.clone());
            node = child;
            rest = &rest[used..];
            rest = rest.strip_prefix('/').unwrap_or(rest);
        }
        Ok(path)
    }

    fn to_json(&self) -> Value {
        let mut node = json!({ "name": self.name, "tokens": self.tokens.round() as i64 });
        if !self.children.is_empty() {
            node["children"] = self.children.iter().map(Node::to_json).collect();
        }
        node
    }
}

pub struct RenderOptions {
    /// How many levels of the breakdown to print.
    pub depth: usize,
    /// How many of the largest individual items to list.
    pub top: usize,
    /// Context window size to report usage against.
    pub window: u64,
    /// The part of the breakdown to report on; empty for all of it.
    pub part: Vec<String>,
    /// Print every level and every row, whatever `depth` says.
    pub all: bool,
}

impl Report {
    /// The breakdown as a tree whose root is the whole context.
    pub fn tree(&self) -> Node {
        Node::from_leaves(&self.leaves)
    }

    /// Everything in one part of the breakdown that has text to show, largest
    /// first. The part is named by its path from the root (empty for all).
    pub fn pieces(&self, under: &[String]) -> Vec<Piece<'_>> {
        let mut leaves: Vec<&Leaf> = self
            .leaves
            .iter()
            .filter(|l| !l.content.is_empty() && l.path.starts_with(under))
            .collect();
        leaves.sort_by(|a, b| b.tokens.total_cmp(&a.tokens));
        leaves
            .into_iter()
            .map(|leaf| {
                let below = &leaf.path[under.len()..];
                let title = match &leaf.item {
                    Some(item) => item.clone(),
                    None if below.is_empty() => leaf.path.last().cloned().unwrap_or_default(),
                    None => below.join(" › "),
                };
                Piece { title, path: &leaf.path, tokens: leaf.tokens, content: &leaf.content }
            })
            .collect()
    }

    /// The pieces of one part with their numbers, as `pieces` orders them:
    /// all of them, or the one numbered `only`.
    fn numbered(&self, part: &[String], only: Option<usize>) -> Result<Vec<(usize, Piece<'_>)>, String> {
        let pieces = self.pieces(part);
        let count = pieces.len();
        if let Some(number) = only.filter(|number| !(1..=count).contains(number)) {
            return Err(format!("{} has {count} pieces, so no piece {number}", part_name(part)));
        }
        let numbered = pieces.into_iter().enumerate().map(|(index, piece)| (index + 1, piece));
        Ok(numbered.filter(|(number, _)| only.is_none_or(|only| only == *number)).collect())
    }

    fn pieces_heading(&self, part: &[String]) -> String {
        let pieces = self.pieces(part);
        let tokens: f64 = pieces.iter().map(|piece| piece.tokens).sum();
        let count = match pieces.len() {
            1 => "1 piece".to_string(),
            count => format!("{count} pieces"),
        };
        format!("{} · {} tokens in {count}\n", part_name(part), thousands(tokens.round() as i64))
    }

    /// The pieces of text one part of the breakdown is made of: what each is
    /// and what it weighs, largest first.
    pub fn render_pieces(&self, part: &[String]) -> String {
        let mut out = self.pieces_heading(part);
        let pieces = self.pieces(part);
        if !pieces.is_empty() {
            let _ = writeln!(out, "\n    #     TOKENS       %  PIECE");
        }
        for (index, piece) in pieces.iter().enumerate() {
            let _ = writeln!(
                out,
                "{:>5}  {:>9}  {:>5.1}%  {}",
                index + 1,
                thousands(piece.tokens.round() as i64),
                if self.total > 0.0 { 100.0 * piece.tokens / self.total } else { 0.0 },
                brief(&piece.title, 96)
            );
        }
        out
    }

    /// The text of one part of the breakdown as the model receives it, piece
    /// after piece, each under a line saying what it is and what it weighs.
    /// `only` keeps the piece with that number.
    pub fn render_content(&self, part: &[String], only: Option<usize>) -> Result<String, String> {
        let mut out = self.pieces_heading(part);
        for (number, piece) in self.numbered(part, only)? {
            let _ = writeln!(
                out,
                "\n── {number}. {} · {} tokens ──\n{}",
                brief(&piece.title, 100),
                thousands(piece.tokens.round() as i64),
                piece.content.trim_end()
            );
        }
        Ok(out)
    }

    /// The pieces of one part as JSON, with their text when `text` is set.
    pub fn pieces_json(&self, part: &[String], only: Option<usize>, text: bool) -> Result<Value, String> {
        let all = self.pieces(part);
        let pieces: Vec<Value> = self
            .numbered(part, only)?
            .into_iter()
            .map(|(number, piece)| {
                let mut entry = json!({ "number": number, "title": piece.title, "path": piece.path,
                                        "tokens": piece.tokens.round() as i64 });
                if text {
                    entry["content"] = json!(piece.content);
                }
                entry
            })
            .collect();
        Ok(json!({
            "session": self.session_id,
            "transcript": self.transcript,
            "request_number": self.request_number,
            "part": part,
            "tokens": all.iter().map(|piece| piece.tokens).sum::<f64>().round() as i64,
            "piece_count": all.len(),
            "pieces": pieces,
        }))
    }

    /// The largest individual pieces of conversation within one part of the
    /// breakdown, named by its path from the root (empty for all of it).
    pub fn largest_items(&self, under: &[String], count: usize) -> Vec<&Leaf> {
        let mut items: Vec<&Leaf> = self
            .leaves
            .iter()
            .filter(|l| l.item.is_some() && l.path.starts_with(under))
            .collect();
        items.sort_by(|a, b| b.tokens.total_cmp(&a.tokens));
        items.truncate(count);
        items
    }

    pub fn to_json(&self, options: &RenderOptions) -> Value {
        let usage = self.usage.map(|u| {
            json!({
                "context_tokens": u.context(),
                "cache_read": u.cache_read,
                "cache_write": u.cache_creation,
                "uncached": u.input,
                "output": u.output,
                "thinking": u.thinking,
            })
        });
        let items: Vec<Value> = self
            .largest_items(&options.part, options.top)
            .into_iter()
            .map(|l| json!({ "item": l.item, "tokens": l.tokens.round() as i64, "measured": l.measured }))
            .collect();
        json!({
            "session": self.session_id,
            "transcript": self.transcript,
            "cwd": self.cwd,
            "title": self.title,
            "model": self.model,
            "request_number": self.request_number,
            "query_source": self.query_source,
            "timestamp": self.timestamp,
            "messages": self.message_count,
            "total_tokens": self.total.round() as i64,
            "total_is_exact": self.usage.is_some(),
            "usage": usage,
            "assumed_window": options.window,
            "window_used": self.total / options.window as f64,
            "measured_share": self.measured_share,
            "part": if options.part.is_empty() { Value::Null } else { json!(options.part) },
            "part_tokens": self.tree().at(&options.part).map(|node| node.tokens.round() as i64),
            "breakdown": self.tree().at(&options.part).map(|node| node.to_json()["children"].clone()),
            "largest_items": items,
            "notes": self.notes,
        })
    }

    pub fn render(&self, options: &RenderOptions) -> String {
        let mut out = String::new();
        let percent = |tokens: f64| if self.total > 0.0 { 100.0 * tokens / self.total } else { 0.0 };

        let _ = writeln!(out, "Session  {}  {}", self.session_id, self.cwd.as_deref().unwrap_or(""));
        if let Some(title) = &self.title {
            let _ = writeln!(out, "         {}", one_line(title, 90));
        }
        let _ = writeln!(
            out,
            "Request  #{} ({}) at {} · {} · {} messages",
            self.request_number, self.query_source, self.timestamp, self.model, self.message_count
        );
        let _ = writeln!(
            out,
            "Context  {} tokens{} · {:.1}% of a {} window (assumed, set with --window)",
            thousands(self.total.round() as i64),
            if self.usage.is_some() { ", as reported by the API" } else { ", estimated" },
            100.0 * self.total / options.window as f64,
            thousands(options.window as i64),
        );
        if let Some(u) = self.usage {
            let _ = writeln!(
                out,
                "         cache read {} · cache write {} · uncached {} · reply {} output tokens ({} thinking)",
                thousands(u.cache_read as i64),
                thousands(u.cache_creation as i64),
                thousands(u.input as i64),
                thousands(u.output as i64),
                thousands(u.thinking as i64),
            );
        }
        let _ = writeln!(
            out,
            "Split    {:.0}% placed from API usage; the rest, and shares inside one message, by text size",
            100.0 * self.measured_share
        );
        for note in &self.notes {
            let _ = writeln!(out, "Note     {note}");
        }

        let tree = self.tree();
        let Some(part) = tree.at(&options.part) else { return out };
        if !options.part.is_empty() {
            let _ = writeln!(
                out,
                "Part     {} · {} tokens · {:.1}% of this context · {:.1}% of the window",
                options.part.join(" › "),
                thousands(part.tokens.round() as i64),
                percent(part.tokens),
                100.0 * part.tokens / options.window as f64
            );
        }
        let depth = if options.all { usize::MAX } else { options.depth };

        if !part.children.is_empty() {
            let _ = writeln!(out, "\n   TOKENS       %");
        }
        for node in &part.children {
            let name = &node.name;
            let _ = writeln!(
                out,
                "{:>9}  {:>5.1}%  {:<12} {}",
                thousands(node.tokens.round() as i64),
                percent(node.tokens),
                bar(percent(node.tokens)),
                name
            );
            render_children(node, 1, depth, options.all, &percent, &mut out);
        }

        let items = self.largest_items(&options.part, options.top);
        if !items.is_empty() {
            let _ = writeln!(out, "\nLargest single items");
            for item in items {
                let _ = writeln!(
                    out,
                    "{:>9}  {:>5.1}%  {}",
                    thousands(item.tokens.round() as i64),
                    percent(item.tokens),
                    brief(item.item.as_deref().unwrap_or(""), 96)
                );
            }
        }
        out
    }
}

fn render_children(node: &Node, level: usize, depth: usize, all: bool, percent: &dyn Fn(f64) -> f64, out: &mut String) {
    if level >= depth {
        return;
    }
    let indent = " ".repeat(13 + 2 * level);
    let children = &node.children;
    let shown = if !all && children.len() > MAX_CHILDREN + 1 { MAX_CHILDREN } else { children.len() };
    for child in &children[..shown] {
        let name = &child.name;
        let _ = writeln!(
            out,
            "{:>9}  {:>5.1}%  {indent}{}",
            thousands(child.tokens.round() as i64),
            percent(child.tokens),
            name
        );
        render_children(child, level + 1, depth, all, percent, out);
    }
    if shown < children.len() {
        let rest: f64 = children[shown..].iter().map(|c| c.tokens).sum();
        let _ = writeln!(
            out,
            "{:>9}  {:>5.1}%  {indent}… {} more",
            thousands(rest.round() as i64),
            percent(rest),
            children.len() - shown
        );
    }
}

/// Whether `text` in full, rather than its first step, is what selected the
/// single node in `found`.
fn whole_is(found: &[&Node], text: &str) -> bool {
    matches!(found, [only] if only.name.to_lowercase().starts_with(text))
}

/// A part of the breakdown as written in reports.
fn part_name(part: &[String]) -> String {
    if part.is_empty() { "The context".to_string() } else { part.join(" › ") }
}

/// A 12-cell bar for a percentage.
fn bar(percent: f64) -> String {
    let cells = (percent.clamp(0.0, 100.0) / 100.0 * 12.0).round() as usize;
    "█".repeat(cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::message_hash;
    use crate::session::testing::{session_from, usage_json};

    fn tokens_at(report: &Report, path: &[&str]) -> f64 {
        report
            .leaves
            .iter()
            .filter(|l| l.path.len() >= path.len() && l.path.iter().zip(path).all(|(a, b)| a == b))
            .map(|l| l.tokens)
            .sum()
    }

    fn request(id: &str, base: Option<&str>, keep: usize, tail: &[&String], shape: &str) -> Value {
        json!({ "type": "api-request", "id": id, "base": base, "keep": keep, "tail": tail, "shapeHash": shape,
                "params": { "model": "m" }, "querySource": "repl_main_thread", "timestamp": "t" })
    }

    fn blob(role: &str, content: &Value) -> (String, Value) {
        let hash = message_hash(role, content);
        let record = json!({ "type": "api-request-blob", "hash": hash, "message": { "role": role, "content": content } });
        (hash, record)
    }

    fn reply(request: &str, id: &str, content: &Value, usage: Value) -> (String, Value) {
        let record = json!({ "type": "assistant", "requestRef": request,
                             "message": { "id": id, "content": content, "usage": usage } });
        (message_hash("assistant", content), record)
    }

    /// A tool definition whose canonical JSON is exactly `bytes` long.
    fn tool(name: &str, bytes: usize) -> Value {
        let mut tool = json!({ "name": name, "description": "", "input_schema": {} });
        tool["description"] = "x".repeat(bytes - canonical_len(&tool)).into();
        assert_eq!(canonical_len(&tool), bytes);
        tool
    }

    /// 100 bytes of system prompt, a 100-byte tool definition and a 200-byte
    /// prompt, reported as 400 tokens: one token per byte.
    fn opening() -> (Vec<Value>, String) {
        let shape = json!({ "type": "api-request-shape", "shapeHash": "s1",
                            "shape": { "system": [{ "type": "text", "text": "s".repeat(100) }], "tools": [tool("Bash", 100)] } });
        let (h_prompt, prompt) = blob("user", &json!([{ "type": "text", "text": "p".repeat(200) }]));
        let first = request("r1", None, 0, &[&h_prompt], "s1");
        (vec![shape, prompt, first], h_prompt)
    }

    #[test]
    fn first_request_is_split_by_size() {
        let (mut records, _) = opening();
        let (_, answer) = reply("r1", "m1", &json!([{ "type": "text", "text": "ok" }]), usage_json(400, 5, 0));
        records.push(answer);
        let report = analyze(&session_from(&records)).unwrap();

        assert_eq!(report.total, 400.0);
        assert_eq!(tokens_at(&report, &[SYSTEM_PROMPT]), 100.0);
        assert_eq!(tokens_at(&report, &[TOOL_DEFINITIONS, "Bash"]), 100.0);
        assert_eq!(tokens_at(&report, &[USER_PROMPTS]), 200.0);
        assert_eq!(report.measured_share, 1.0);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn growth_between_requests_goes_to_the_reply_and_the_new_messages() {
        let (mut records, _) = opening();
        // The reply cost 50 output tokens, 30 of them thinking. Its recorded
        // thinking text is tiny, so size alone would get this wrong.
        let call = json!({ "type": "tool_use", "id": "t1", "name": "mcp__files__read", "input": { "path": "/a" } });
        let content = json!([{ "type": "thinking", "thinking": "hm" }, call]);
        let (h_reply, answer) = reply("r1", "m1", &content, usage_json(400, 50, 30));
        // Context then grew by 350: 50 for the reply, 300 for what came back.
        let result = json!([
            { "type": "tool_result", "tool_use_id": "t1", "content": "r".repeat(250) },
            { "type": "text", "text": format!("<system-reminder>\n{}", "n".repeat(32)) },
        ]);
        let (h_result, result) = blob("user", &result);
        let second = request("r2", Some("r1"), 1, &[&h_reply, &h_result], "s1");
        let (_, done) = reply("r2", "m2", &json!([{ "type": "text", "text": "done" }]), usage_json(750, 4, 0));
        records.extend([answer, result, second, done]);
        let report = analyze(&session_from(&records)).unwrap();

        assert_eq!(report.total, 750.0);
        assert_eq!(report.message_count, 3);
        assert_eq!(tokens_at(&report, &[ASSISTANT, "thinking"]), 30.0);
        assert_eq!(tokens_at(&report, &[ASSISTANT, "tool calls", "mcp: files", "read"]), 20.0);
        assert_eq!(tokens_at(&report, &[TOOL_RESULTS, "mcp: files", "read"]), 250.0);
        assert_eq!(tokens_at(&report, &[REMINDERS]), 50.0);
        assert_eq!(tokens_at(&report, &[UNATTRIBUTED]), 0.0);
        let largest = report.largest_items(&[], 1);
        let calls = report.largest_items(&[ASSISTANT.to_string(), "tool calls".to_string()], 5);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].item.as_deref(), Some("call to mcp__files__read: /a"));
        // Each share carries the text it stands for, in a readable form.
        let content = |path: &[&str]| -> Vec<&str> {
            let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            report.pieces(&path).iter().map(|piece| piece.content).collect()
        };
        assert_eq!(content(&[ASSISTANT, "tool calls"]), ["path: /a"]);
        assert_eq!(content(&[ASSISTANT, "thinking"]), ["hm"]);
        assert_eq!(content(&[TOOL_RESULTS]), ["r".repeat(250).as_str()]);
        assert_eq!(content(&[SYSTEM_PROMPT]), ["s".repeat(100).as_str()]);
        assert!(content(&[TOOL_DEFINITIONS])[0].ends_with("Input schema:\n{}"), "{:?}", content(&[TOOL_DEFINITIONS]));
        assert_eq!(report.pieces(&[]).len(), 7);
        assert_eq!(report.pieces(&[])[0].path[0], TOOL_RESULTS, "largest first");

        let tree = report.tree();
        assert_eq!(tree.tokens, 750.0);
        let names: Vec<&str> = tree.children.iter().map(|n| n.name.as_str()).collect();
        // Largest first; equal shares in name order.
        assert_eq!(names, [TOOL_RESULTS, USER_PROMPTS, SYSTEM_PROMPT, TOOL_DEFINITIONS, ASSISTANT, REMINDERS]);
        assert_eq!(largest[0].item.as_deref(), Some("result of mcp__files__read: /a"));
    }

    #[test]
    fn new_tools_mid_session_are_sized_from_what_the_kept_messages_leave_over() {
        let (mut records, _) = opening();
        let (h_reply, answer) = reply("r1", "m1", &json!([{ "type": "text", "text": "ok" }]), usage_json(400, 10, 0));
        // A second, 300-byte tool appears; context grows by 310.
        let shape = json!({ "type": "api-request-shape", "shapeHash": "s2",
                            "shape": { "system": [{ "type": "text", "text": "s".repeat(100) }],
                                       "tools": [tool("Bash", 100), tool("Grep", 300)] } });
        let (h_next, next) = blob("user", &json!([{ "type": "text", "text": "q".repeat(8) }]));
        let second = request("r2", Some("r1"), 1, &[&h_reply, &h_next], "s2");
        let (_, done) = reply("r2", "m2", &json!([{ "type": "text", "text": "k" }]), usage_json(710, 3, 0));
        records.extend([answer, shape, next, second, done]);
        let report = analyze(&session_from(&records)).unwrap();

        // 710 - 200 kept = 510 tokens for 500 bytes of shape and 10 of messages.
        assert_eq!(report.total, 710.0);
        assert!((tokens_at(&report, &[TOOL_DEFINITIONS, "Grep"]) - 300.0).abs() < 1e-6);
        assert!((tokens_at(&report, &[TOOL_DEFINITIONS]) - 400.0).abs() < 1e-6);
        assert!((tokens_at(&report, &[USER_PROMPTS]) - 208.0).abs() < 1e-6);
        assert_eq!(tokens_at(&report, &[UNATTRIBUTED]), 0.0);
    }

    #[test]
    fn a_request_without_usage_is_sized_at_the_next_one_that_has_it() {
        let (mut records, _) = opening();
        let (h_reply, answer) = reply("r1", "m1", &json!([{ "type": "text", "text": "ok" }]), usage_json(400, 40, 0));
        let (h_next, next) = blob("user", &json!([{ "type": "text", "text": "again" }]));
        // r2 was interrupted; r3 retries it unchanged and reports 500 tokens.
        let second = request("r2", Some("r1"), 1, &[&h_reply, &h_next], "s1");
        let retry = request("r3", Some("r2"), 3, &[], "s1");
        let (_, done) = reply("r3", "m3", &json!([{ "type": "text", "text": "k" }]), usage_json(500, 3, 0));
        records.extend([answer, next, second, retry, done]);
        let report = analyze(&session_from(&records)).unwrap();

        assert_eq!(report.total, 500.0);
        assert_eq!(tokens_at(&report, &[ASSISTANT, "text"]), 40.0);
        assert_eq!(tokens_at(&report, &[USER_PROMPTS]), 260.0);
        assert_eq!(tokens_at(&report, &[UNATTRIBUTED]), 0.0);
        assert_eq!(report.measured_share, 1.0);
    }

    #[test]
    fn context_that_shrinks_without_losing_messages_is_reported_as_dropped() {
        let (mut records, _) = opening();
        let thought = json!([{ "type": "thinking", "thinking": "long" }, { "type": "text", "text": "ok" }]);
        let (h_reply, answer) = reply("r1", "m1", &thought, usage_json(400, 150, 140));
        let (h_next, next) = blob("user", &json!([{ "type": "text", "text": "n".repeat(20) }]));
        // With the reply and 20 more bytes the context should grow; the API reports 380.
        let second = request("r2", Some("r1"), 1, &[&h_reply, &h_next], "s1");
        let (h_done, done) = reply("r2", "m2", &json!([{ "type": "text", "text": "k" }]), usage_json(380, 5, 0));
        // Later growth is measured from the smaller context.
        let (h_last, last) = blob("user", &json!([{ "type": "text", "text": "z".repeat(30) }]));
        let third = request("r3", Some("r2"), 3, &[&h_done, &h_last], "s1");
        let (_, end) = reply("r3", "m3", &json!([{ "type": "text", "text": "k" }]), usage_json(415, 1, 0));
        records.extend([answer, next, second, done, last, third, end]);
        let report = analyze(&session_from(&records)).unwrap();

        assert_eq!(report.total, 415.0);
        // The first reply is sized by bytes here (6 bytes), not by its 150 output tokens.
        assert_eq!(tokens_at(&report, &[ASSISTANT]), 6.0 + 5.0);
        assert_eq!(tokens_at(&report, &[USER_PROMPTS]), 200.0 + 20.0 + 30.0);
        assert_eq!(tokens_at(&report, &[DROPPED]), 380.0 - 426.0);
        assert_eq!(tokens_at(&report, &[UNATTRIBUTED]), 0.0);
        assert!(report.notes[0].contains("on 1 request(s) the context shrank"), "{:?}", report.notes);
    }

    #[test]
    fn tools_added_mid_conversation_count_as_tool_definitions() {
        let (mut records, _) = opening();
        let (h_reply, answer) = reply("r1", "m1", &json!([{ "type": "text", "text": "ok" }]), usage_json(400, 10, 0));
        let addition = json!([{ "type": "tool_addition", "tool": { "type": "tool_definition",
                                "definition": { "name": "mcp__docs__create", "description": "Create a doc" } } }]);
        let (h_added, added) = blob("system", &addition);
        let second = request("r2", Some("r1"), 1, &[&h_reply, &h_added], "s1");
        let (_, done) = reply("r2", "m2", &json!([{ "type": "text", "text": "k" }]), usage_json(470, 3, 0));
        records.extend([answer, added, second, done]);
        let report = analyze(&session_from(&records)).unwrap();

        assert_eq!(tokens_at(&report, &[TOOL_DEFINITIONS, "mcp: docs", "create"]), 60.0);
        assert_eq!(tokens_at(&report, &[TOOL_DEFINITIONS]), 160.0);
    }

    #[test]
    fn a_pending_request_and_unmatched_messages_are_estimated_and_flagged() {
        let (mut records, _) = opening();
        let (h_reply, answer) = reply("r1", "m1", &json!([{ "type": "text", "text": "ok" }]), usage_json(400, 10, 0));
        let missing = "0".repeat(64);
        records.extend([answer, request("r2", Some("r1"), 1, &[&h_reply, &missing], "s1")]);
        let report = analyze(&session_from(&records)).unwrap();

        assert!(report.usage.is_none());
        // 400 measured, plus the 2-byte reply at the calibrated one byte per token.
        assert_eq!(report.total, 402.0);
        assert!(report.measured_share < 1.0);
        assert_eq!(report.notes.len(), 2, "{:?}", report.notes);
        let options = RenderOptions { depth: 3, top: 5, window: 1000, part: Vec::new(), all: false };
        assert!(report.render(&options).contains("40.2% of a 1,000 window"));
    }

    /// A report with the given shares, each `(path, tokens, item, content)`.
    fn report_of(shares: &[(&[&str], f64, Option<&str>, &str)]) -> Report {
        let leaves: Vec<Leaf> = shares
            .iter()
            .map(|(path, tokens, item, content)| Leaf {
                item: item.map(str::to_owned),
                ..leaf(path, *tokens, true, content)
            })
            .collect();
        Report {
            session_id: "s".into(),
            transcript: PathBuf::from("/t/s.jsonl"),
            cwd: None,
            title: None,
            model: "m".into(),
            query_source: "q".into(),
            timestamp: "t".into(),
            request_number: 1,
            message_count: 1,
            usage: None,
            total: leaves.iter().map(|l| l.tokens).sum(),
            measured_share: 1.0,
            leaves,
            notes: Vec::new(),
        }
    }

    fn sample() -> Report {
        report_of(&[
            (&[TOOL_RESULTS, "Bash"], 500.0, Some("result of Bash: cargo test"), "ok\n"),
            (&[TOOL_RESULTS, "Bash"], 100.0, Some("result of Bash: ls"), "src"),
            (&[TOOL_RESULTS, "BashOutput"], 50.0, Some("result of BashOutput: 1"), "done"),
            (&[TOOL_RESULTS, "mcp: files", "read"], 50.0, Some("result of mcp__files__read: /a"), "A"),
            (&[REMINDERS, "claudeMd", "/p/CLAUDE.md"], 200.0, None, "Be terse."),
            (&[REMINDERS, "gitStatus"], 100.0, None, "clean"),
        ])
    }

    #[test]
    fn parts_are_found_by_name_or_by_an_unambiguous_start() {
        let tree = sample().tree();
        let found = |selector: &str| tree.find(selector).map(|path| path.join(" > "));
        assert_eq!(found("Tool results/Bash").unwrap(), "Tool results > Bash");
        assert_eq!(found("tool results/bash").unwrap(), "Tool results > Bash", "a full name wins over a longer one");
        assert_eq!(found("tool/basho").unwrap(), "Tool results > BashOutput");
        assert_eq!(found("t/mcp/r").unwrap(), "Tool results > mcp: files > read");
        assert_eq!(found("/sys/").unwrap(), "System reminders");
        // Names with slashes of their own.
        assert_eq!(found("sys/claudemd//p/CLAUDE.md").unwrap(), "System reminders > claudeMd > /p/CLAUDE.md");
        assert_eq!(found("sys/claudemd//p/cl").unwrap(), "System reminders > claudeMd > /p/CLAUDE.md");
        assert_eq!(found("sys/claude/").unwrap(), "System reminders > claudeMd");
        assert_eq!(found("").unwrap(), "");

        assert_eq!(found("nope").unwrap_err(), "no part 'nope' in the context, which has: Tool results, System reminders");
        assert_eq!(
            found("tool/ba").unwrap_err(),
            "'ba' could be several parts of Tool results: Bash, BashOutput",
        );
        assert_eq!(found("sys/git/x").unwrap_err(), "System reminders › gitStatus has no parts, so no 'x'");

        let path = tree.find("tool/bash").unwrap();
        assert_eq!(tree.at(&path).unwrap().tokens, 600.0);
        assert!(tree.at(&["Nowhere".to_string()]).is_none());
    }

    #[test]
    fn a_part_can_be_reported_on_its_own_and_in_full() {
        let report = sample();
        let part = report.tree().find("tool").unwrap();
        let text = report.render(&RenderOptions { depth: 1, top: 2, window: 10_000, part: part.clone(), all: false });
        assert!(text.contains("Part     Tool results · 700 tokens · 70.0% of this context · 7.0% of the window"), "{text}");
        assert!(text.contains("      600   60.0%  ███████      Bash\n"), "{text}");
        assert!(!text.contains("read") && !text.contains("System reminders"), "{text}");
        assert!(text.contains("Largest single items\n      500   50.0%  result of Bash: cargo test\n      100"), "{text}");

        // Everything: all levels, whatever the depth.
        let all = report.render(&RenderOptions { depth: 1, top: 0, window: 10_000, part: Vec::new(), all: true });
        assert!(all.contains("/p/CLAUDE.md") && all.contains("read"), "{all}");

        let json = report.to_json(&RenderOptions { depth: 1, top: 1, window: 10_000, part, all: false });
        assert_eq!(json["part"], json!(["Tool results"]));
        assert_eq!(json["part_tokens"], 700);
        assert_eq!(json["breakdown"][0]["name"], "Bash");
        assert_eq!(json["largest_items"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn content_is_listed_and_printed_piece_by_piece() {
        let report = sample();
        let part = report.tree().find("tool/bash").unwrap();
        assert_eq!(
            report.render_content(&part, None).unwrap(),
            "Tool results › Bash · 600 tokens in 2 pieces\n\
             \n── 1. result of Bash: cargo test · 500 tokens ──\nok\n\
             \n── 2. result of Bash: ls · 100 tokens ──\nsrc\n"
        );
        assert_eq!(
            report.render_content(&part, Some(2)).unwrap(),
            "Tool results › Bash · 600 tokens in 2 pieces\n\n── 2. result of Bash: ls · 100 tokens ──\nsrc\n"
        );
        assert_eq!(report.render_content(&part, Some(3)).unwrap_err(), "Tool results › Bash has 2 pieces, so no piece 3");
        assert_eq!(report.render_content(&[], Some(0)).unwrap_err(), "The context has 6 pieces, so no piece 0");

        assert_eq!(
            report.render_pieces(&part),
            "Tool results › Bash · 600 tokens in 2 pieces\n\
             \n    #     TOKENS       %  PIECE\n\
             \x20   1        500   50.0%  result of Bash: cargo test\n\
             \x20   2        100   10.0%  result of Bash: ls\n"
        );
        assert!(report.render_pieces(&[]).starts_with("The context · 1,000 tokens in 6 pieces\n"));
        let one = report.tree().find("sys/git").unwrap();
        assert!(report.render_pieces(&one).starts_with("System reminders › gitStatus · 100 tokens in 1 piece\n"));

        let titles: Vec<String> = report.pieces(&[REMINDERS.to_string()]).into_iter().map(|piece| piece.title).collect();
        assert_eq!(titles, ["claudeMd › /p/CLAUDE.md", "gitStatus"]);
        let own = report.pieces(&[REMINDERS.to_string(), "gitStatus".to_string()]);
        assert_eq!((own[0].title.as_str(), own[0].content), ("gitStatus", "clean"));

        let json = report.pieces_json(&part, Some(2), true).unwrap();
        assert_eq!((json["tokens"].as_i64(), json["piece_count"].as_i64()), (Some(600), Some(2)));
        assert_eq!(json["pieces"], json!([{ "number": 2, "title": "result of Bash: ls", "path": ["Tool results", "Bash"],
                                            "tokens": 100, "content": "src" }]));
        let listed = report.pieces_json(&part, None, false).unwrap();
        assert_eq!(listed["pieces"].as_array().unwrap().len(), 2);
        assert!(listed["pieces"][0].get("content").is_none());
    }

    #[test]
    fn tool_inputs_and_results_read_as_text() {
        let input = json!({ "file_path": "/a.rs", "content": "fn main() {\n    run();\n}\n", "replace_all": false });
        assert_eq!(input_text(&input), "file_path: /a.rs\nreplace_all: false\n\ncontent:\nfn main() {\n    run();\n}");
        assert_eq!(input_text(&json!("bare")), "\"bare\"");

        assert_eq!(result_text(&json!("plain")), "plain");
        let mixed = json!([{ "type": "text", "text": "one" }, { "type": "image", "source": {} }, { "type": "text", "text": "two" }]);
        assert_eq!(result_text(&mixed), "one\n[image]\ntwo");
        assert_eq!(result_text(&Value::Null), "");
    }

    #[test]
    fn injected_text_is_split_at_its_landmarks() {
        let text = "# Environment\nYou are in /work\n\n\
                    The following deferred tools are now available via ToolSearch:\nWebSearch\nmcp__figma__get_file\nmcp__figma__export\n\n\
                    # MCP Server Instructions\n\nintro\n\n## figma\n# Not a section\nuse wisely\n\n\
                    The following skills are available for use with the Skill tool:\n\n- tdd: test first\n  continued\n- pr: write bodies\n\n\
                    Today's date is 2026-11-23.";
        let sections = injected_sections(text);
        let size = |path: &[&str]| sections[&path.iter().map(|s| s.to_string()).collect::<Vec<_>>()].len();

        assert_eq!(sections.values().map(String::len).sum::<usize>(), text.len());
        assert_eq!(size(&["Environment"]), "# Environment\nYou are in /work\n\n".len());
        assert_eq!(size(&["Deferred tool names", "mcp: figma"]), "mcp__figma__get_file\nmcp__figma__export\n\n".len());
        assert_eq!(size(&["Deferred tool names", "built-in"]), "WebSearch\n".len());
        assert_eq!(size(&["MCP Server Instructions", "figma"]), "## figma\n# Not a section\nuse wisely\n\n".len());
        assert_eq!(size(&["Skill listing", "tdd"]), "- tdd: test first\n  continued\n".len());
        assert_eq!(size(&["Skill listing", "pr"]), "- pr: write bodies\n\n".len());
        assert_eq!(sections[&vec!["Skill listing".to_string(), "tdd".to_string()]], "- tdd: test first\n  continued\n");
        assert_eq!(size(&["Date"]), "Today's date is 2026-11-23.".len());
    }

    #[test]
    fn mid_conversation_notes_are_grouped_by_how_they_open() {
        let changed = |file: &str| format!("Note: /work/src/{file} changed on disk since you last read it.\n\nKeep it.");
        let one = injected_sections(&changed("a.rs"));
        let two = injected_sections(&changed("main.rs"));
        let label = vec![NOTES.to_string(), "Note: <path> changed on disk since you last…".to_string()];
        assert_eq!(one.keys().collect::<Vec<_>>(), [&label]);
        assert_eq!(two.keys().collect::<Vec<_>>(), [&label]);

        let text = "Note: /x changed.\n\n<new-diagnostics>The following issues:\n\na.rs:\n  <T> is unused\n</new-diagnostics>";
        let sections = injected_sections(text);
        let diagnostics = &sections[&vec![NOTES.to_string(), "new-diagnostics".to_string()]];
        assert_eq!(diagnostics, "<new-diagnostics>The following issues:\n\na.rs:\n  <T> is unused\n</new-diagnostics>");
    }

    #[test]
    fn reminders_are_split_by_context_key_and_memory_file() {
        let text = "<system-reminder>\nAs you answer:\n# claudeMd\nintro\n\n\
                    Contents of /p/CLAUDE.md (project instructions, checked in):\n\n# Overview\nrules\n\n\
                    Contents of /h/.claude/CLAUDE.md (user instructions):\n\nmore\n# gitStatus\nclean\n</system-reminder>";
        let sections = reminder_sections(text);
        let size = |path: &[&str]| sections[&path.iter().map(|s| s.to_string()).collect::<Vec<_>>()].len();

        assert_eq!(sections.values().map(String::len).sum::<usize>(), text.len());
        assert_eq!(size(&["(lead-in)"]), "<system-reminder>\nAs you answer:\n".len());
        assert_eq!(
            size(&["claudeMd", "/p/CLAUDE.md"]),
            "Contents of /p/CLAUDE.md (project instructions, checked in):\n\n# Overview\nrules\n\n".len()
        );
        assert_eq!(size(&["claudeMd", "/h/.claude/CLAUDE.md"]), "Contents of /h/.claude/CLAUDE.md (user instructions):\n\nmore\n".len());
        assert_eq!(size(&["gitStatus"]), "# gitStatus\nclean\n</system-reminder>".len());

        // A memory file delivered in a reminder of its own, with prose headings.
        let nested = reminder_sections("<system-reminder>\nShown below:\n\nContents of /p/sub/CLAUDE.md:\n\n# Communication\nbe terse\n");
        let paths: Vec<&Vec<String>> = nested.keys().collect();
        assert_eq!(paths, [&vec!["Shown below:".to_string()], &vec!["claudeMd".to_string(), "/p/sub/CLAUDE.md".to_string()]]);

        let other = reminder_sections("<system-reminder>\nPostToolUse hook said: slow down\n</system-reminder>");
        assert_eq!(other.keys().next().unwrap(), &vec!["PostToolUse hook said: slow down".to_string()]);
    }
}
