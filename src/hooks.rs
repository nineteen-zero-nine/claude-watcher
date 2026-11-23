//! Collecting hook configuration from every place Claude Code reads it.
//!
//! Hooks are defined under a `hooks` key in settings files (managed, project
//! local, project, user), in the `hooks/hooks.json` or manifest of enabled
//! plugins, and in the frontmatter of skills and agents (active only while
//! that skill or agent runs). Hooks from all sources run together; precedence
//! only matters for switches such as `disableAllHooks`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::paths::{config_dir, managed_settings, tilde};
use crate::util::one_line;

/// A file that may define hooks.
pub struct Source {
    pub scope: String,
    pub path: PathBuf,
    /// The file the path finally leads to, when it is a symlink.
    pub link_target: Option<PathBuf>,
    pub state: State,
    /// Why the file may not be in effect, when that is uncertain.
    pub caveat: Option<&'static str>,
}

pub enum State {
    Absent,
    Unreadable(String),
    Read { hooks: usize },
}

pub struct Hook {
    pub scope: String,
    pub file: PathBuf,
    pub event: String,
    /// Tool or event pattern the hook is limited to; empty matches everything.
    pub matcher: String,
    /// `command`, `prompt`, `agent`, `http`, ...
    pub kind: String,
    /// The command, prompt or URL.
    pub action: String,
    /// Remaining settings such as `timeout`.
    pub options: Vec<(String, String)>,
}

/// A skill or agent whose frontmatter declares hooks.
pub struct ScopedHooks {
    pub file: PathBuf,
    pub declaration: String,
}

pub struct HookReport {
    pub project: PathBuf,
    /// Highest precedence first.
    pub sources: Vec<Source>,
    pub hooks: Vec<Hook>,
    pub scoped: Vec<ScopedHooks>,
    /// Scope of the settings file that sets `disableAllHooks`.
    pub disabled_by: Option<String>,
    /// Scope of the settings file that sets `allowManagedHooksOnly`.
    pub managed_only_by: Option<String>,
    /// Scope and command of the status line, which also runs on every update.
    pub status_line: Option<(String, String)>,
}

pub fn collect(project: &Path) -> HookReport {
    collect_from(project, &config_dir(), &managed_settings())
}

/// `collect`, with the user configuration directory and managed settings file given.
pub fn collect_from(project: &Path, config: &Path, managed: &Path) -> HookReport {
    let mut report = HookReport {
        project: project.to_path_buf(),
        sources: Vec::new(),
        hooks: Vec::new(),
        scoped: Vec::new(),
        disabled_by: None,
        managed_only_by: None,
        status_line: None,
    };

    let settings_files = [
        ("managed", managed.to_path_buf(), None),
        ("project local", project.join(".claude/settings.local.json"), None),
        ("project", project.join(".claude/settings.json"), None),
        ("user", config.join("settings.json"), None),
        (
            "user local",
            config.join("settings.local.json"),
            Some("not a documented settings location; Claude Code may ignore it"),
        ),
    ];

    let mut plugin_switches: BTreeMap<String, bool> = BTreeMap::new();
    for (scope, path, caveat) in settings_files {
        if caveat.is_some() && !path.exists() {
            continue;
        }
        let settings = report.read_source(scope, &path, caveat, hooks_key);
        let Some(settings) = settings else { continue };
        let is_true = |key: &str| settings.get(key).and_then(Value::as_bool) == Some(true);
        if is_true("disableAllHooks") && report.disabled_by.is_none() {
            report.disabled_by = Some(scope.to_string());
        }
        if is_true("allowManagedHooksOnly") && report.managed_only_by.is_none() {
            report.managed_only_by = Some(scope.to_string());
        }
        if report.status_line.is_none() {
            if let Some(command) = settings.pointer("/statusLine/command").and_then(Value::as_str) {
                report.status_line = Some((scope.to_string(), command.to_string()));
            }
        }
        // Sources are visited highest precedence first, so the first word on a plugin wins.
        if let Some(switches) = settings.get("enabledPlugins").and_then(Value::as_object) {
            for (plugin, enabled) in switches {
                plugin_switches.entry(plugin.clone()).or_insert(enabled.as_bool() == Some(true));
            }
        }
    }

    let mut frontmatter_roots = vec![config.to_path_buf(), project.join(".claude")];
    for (plugin, root) in enabled_plugin_roots(config, project, &plugin_switches) {
        report.read_plugin(&plugin, &root);
        frontmatter_roots.push(root);
    }
    for root in frontmatter_roots {
        report.scan_frontmatter(&root);
    }
    report
}

fn hooks_key(document: &Value) -> Option<&Value> {
    document.get("hooks")
}

/// Plugin hook files wrap their hooks in a `hooks` key; accept them bare too.
fn hooks_key_or_whole(document: &Value) -> Option<&Value> {
    document.get("hooks").or(Some(document))
}

/// Install directories of the enabled plugins that apply to this project.
fn enabled_plugin_roots(config: &Path, project: &Path, switches: &BTreeMap<String, bool>) -> Vec<(String, PathBuf)> {
    let installed = fs::read_to_string(config.join("plugins/installed_plugins.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    let mut roots = Vec::new();
    for (plugin, enabled) in switches {
        if !enabled {
            continue;
        }
        let installs = installed["plugins"][plugin].as_array().map_or(&[][..], Vec::as_slice);
        let applies = |install: &&Value| match install["projectPath"].as_str() {
            Some(path) => Path::new(path) == project,
            None => true,
        };
        if let Some(path) = installs.iter().find(applies).and_then(|i| i["installPath"].as_str()) {
            roots.push((plugin.clone(), PathBuf::from(path)));
        }
    }
    roots
}

impl HookReport {
    /// Record a source, collect the hooks `locate` finds in it, and return its JSON.
    fn read_source(
        &mut self,
        scope: &str,
        path: &Path,
        caveat: Option<&'static str>,
        locate: fn(&Value) -> Option<&Value>,
    ) -> Option<Value> {
        // Follow the whole chain: the first hop may be one of several.
        let link_target = fs::read_link(path).ok().map(|first| fs::canonicalize(path).unwrap_or(first));
        let (state, document) = match fs::read_to_string(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (State::Absent, None),
            Err(error) => (State::Unreadable(error.to_string()), None),
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Err(error) => (State::Unreadable(format!("invalid JSON: {error}")), None),
                Ok(document) => {
                    let found = locate(&document).map(|hooks| parse_hooks(scope, path, hooks)).unwrap_or_default();
                    let state = State::Read { hooks: found.len() };
                    self.hooks.extend(found);
                    (state, Some(document))
                }
            },
        };
        self.sources.push(Source { scope: scope.to_string(), path: path.to_path_buf(), link_target, state, caveat });
        document
    }

    /// A plugin's hooks live in `hooks/hooks.json`, or wherever its manifest
    /// points, or inline in the manifest.
    fn read_plugin(&mut self, plugin: &str, root: &Path) {
        let scope = format!("plugin {plugin}");
        let manifest_path = root.join(".claude-plugin/plugin.json");
        let manifest = fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or_default();

        let mut files = vec![root.join("hooks/hooks.json")];
        match &manifest["hooks"] {
            Value::String(path) => files.push(root.join(path)),
            Value::Array(paths) => files.extend(paths.iter().filter_map(Value::as_str).map(|p| root.join(p))),
            _ => {}
        }
        files.dedup();
        let before = self.sources.len();
        if manifest["hooks"].is_object() {
            self.read_source(&scope, &manifest_path, None, hooks_key);
        }
        for file in files.iter().filter(|file| file.exists()) {
            self.read_source(&scope, file, None, hooks_key_or_whole);
        }
        if self.sources.len() == before {
            // Nothing to read: show the plugin itself as a source without hooks.
            self.sources.push(Source {
                scope,
                path: root.to_path_buf(),
                link_target: None,
                state: if root.is_dir() { State::Read { hooks: 0 } } else { State::Absent },
                caveat: None,
            });
        }
    }

    /// Look for a `hooks:` key in the frontmatter of skills and agents under `root`.
    fn scan_frontmatter(&mut self, root: &Path) {
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(root.join("skills")).into_iter().flatten().flatten() {
            files.push(entry.path().join("SKILL.md"));
        }
        for dir in ["agents", "commands"] {
            for entry in fs::read_dir(root.join(dir)).into_iter().flatten().flatten() {
                if entry.path().extension().is_some_and(|x| x == "md") {
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        for file in files {
            let Ok(text) = fs::read_to_string(&file) else { continue };
            if let Some(declaration) = frontmatter_hooks(&text) {
                self.scoped.push(ScopedHooks { file, declaration });
            }
        }
    }
}

/// The `hooks:` entry of a document's YAML frontmatter, verbatim.
fn frontmatter_hooks(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let frontmatter: Vec<&str> = lines.take_while(|line| line.trim_end() != "---").collect();
    let start = frontmatter.iter().position(|line| line.starts_with("hooks:"))?;
    let nested = frontmatter[start + 1..]
        .iter()
        .take_while(|line| line.is_empty() || line.starts_with([' ', '\t', '-']));
    let block: Vec<&str> = std::iter::once(&frontmatter[start]).chain(nested).copied().collect();
    Some(block.join("\n").trim_end().to_string())
}

/// Flatten `{ Event: [ { matcher, hooks: [ {type, ...} ] } ] }`.
fn parse_hooks(scope: &str, file: &Path, hooks: &Value) -> Vec<Hook> {
    let mut parsed = Vec::new();
    let Some(events) = hooks.as_object() else { return parsed };
    for (event, groups) in events {
        for group in groups.as_array().map_or(&[][..], Vec::as_slice) {
            let matcher = group["matcher"].as_str().unwrap_or("").to_string();
            // Tolerate a hook written directly where a matcher group belongs.
            let direct = [group.clone()];
            let entries = match group["hooks"].as_array() {
                Some(entries) => entries.as_slice(),
                None if group.get("type").is_some() => &direct[..],
                None => &[],
            };
            for entry in entries {
                parsed.push(parse_hook(scope, file, event, &matcher, entry));
            }
        }
    }
    parsed
}

fn parse_hook(scope: &str, file: &Path, event: &str, matcher: &str, entry: &Value) -> Hook {
    let kind = entry["type"].as_str().unwrap_or("?").to_string();
    let action_key = ["command", "prompt", "url"].into_iter().find(|key| entry.get(key).is_some());
    let display = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let mut options = Vec::new();
    for (key, value) in entry.as_object().into_iter().flatten() {
        if key != "type" && Some(key.as_str()) != action_key {
            options.push((key.clone(), display(value)));
        }
    }
    Hook {
        scope: scope.to_string(),
        file: file.to_path_buf(),
        event: event.to_string(),
        matcher: matcher.to_string(),
        kind,
        action: action_key.map(|key| display(&entry[key])).unwrap_or_default(),
        options,
    }
}

impl HookReport {
    pub fn to_json(&self) -> Value {
        let sources: Vec<Value> = self
            .sources
            .iter()
            .map(|s| {
                let (state, hooks, error) = match &s.state {
                    State::Absent => ("absent", 0, None),
                    State::Unreadable(error) => ("unreadable", 0, Some(error.clone())),
                    State::Read { hooks } => ("read", *hooks, None),
                };
                json!({ "scope": s.scope, "path": s.path, "symlink_to": s.link_target,
                        "state": state, "hooks": hooks, "error": error, "caveat": s.caveat })
            })
            .collect();
        let hooks: Vec<Value> = self
            .hooks
            .iter()
            .map(|h| {
                let options: BTreeMap<&str, &str> = h.options.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                json!({ "scope": h.scope, "file": h.file, "event": h.event, "matcher": h.matcher,
                        "type": h.kind, "action": h.action, "options": options })
            })
            .collect();
        let scoped: Vec<Value> = self
            .scoped
            .iter()
            .map(|s| json!({ "file": s.file, "declaration": s.declaration }))
            .collect();
        json!({
            "project": self.project,
            "sources": sources,
            "hooks": hooks,
            "skill_and_agent_hooks": scoped,
            "disable_all_hooks_set_by": self.disabled_by,
            "allow_managed_hooks_only_set_by": self.managed_only_by,
            "status_line": self.status_line.as_ref().map(|(scope, command)| json!({ "scope": scope, "command": command })),
        })
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Hook configuration for {}\n", tilde(&self.project));

        let _ = writeln!(out, "Sources, highest precedence first");
        for source in &self.sources {
            let state = match &source.state {
                State::Absent => "absent".to_string(),
                State::Unreadable(error) => format!("UNREADABLE: {error}"),
                State::Read { hooks: 0 } => "no hooks".to_string(),
                State::Read { hooks: 1 } => "1 hook".to_string(),
                State::Read { hooks } => format!("{hooks} hooks"),
            };
            let _ = writeln!(out, "  {:<9} {}", state, source.scope);
            let _ = writeln!(out, "  {:<9}   {}", "", tilde(&source.path));
            if let Some(target) = &source.link_target {
                let _ = writeln!(out, "  {:<9}   symlink to {}", "", tilde(target));
            }
            if let Some(caveat) = source.caveat {
                let _ = writeln!(out, "  {:<9}   {caveat}", "");
            }
        }

        if let Some(scope) = &self.disabled_by {
            let _ = writeln!(out, "\ndisableAllHooks is set in {scope} settings: no hook below will run.");
        }
        if let Some(scope) = &self.managed_only_by {
            let _ = writeln!(out, "\nallowManagedHooksOnly is set in {scope} settings: only managed hooks run.");
        }

        if self.hooks.is_empty() {
            let _ = writeln!(out, "\nHooks: none configured in any settings file or enabled plugin.");
        } else {
            let _ = writeln!(out, "\nHooks ({})", self.hooks.len());
            let mut by_event: BTreeMap<&str, Vec<&Hook>> = BTreeMap::new();
            for hook in &self.hooks {
                by_event.entry(&hook.event).or_default().push(hook);
            }
            for (event, hooks) in by_event {
                let _ = writeln!(out, "  {event}");
                for hook in hooks {
                    let matcher = if hook.matcher.is_empty() { "*" } else { &hook.matcher };
                    let _ = writeln!(out, "    on {matcher} · {} · from {}", hook.kind, hook.scope);
                    let _ = writeln!(out, "      {}", one_line(&hook.action, 200));
                    if !hook.options.is_empty() {
                        let options: Vec<String> = hook.options.iter().map(|(k, v)| format!("{k}={}", one_line(v, 60))).collect();
                        let _ = writeln!(out, "      {}", options.join("  "));
                    }
                }
            }
        }

        if !self.scoped.is_empty() {
            let _ = writeln!(out, "\nHooks declared by skills and agents (active only while they run)");
            for scoped in &self.scoped {
                let _ = writeln!(out, "  {}", tilde(&scoped.file));
                for line in scoped.declaration.lines() {
                    let _ = writeln!(out, "    {line}");
                }
            }
        }

        if let Some((scope, command)) = &self.status_line {
            let _ = writeln!(out, "\nStatus line command, run on every update ({scope} settings)\n  {command}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_events_matchers_and_hooks() {
        let hooks = json!({
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [
                    { "type": "command", "command": "guard.sh", "timeout": 10 },
                    { "type": "prompt", "prompt": "Is this safe?" },
                ] },
            ],
            "Stop": [ { "hooks": [ { "type": "http", "url": "http://localhost/done", "async": true } ] } ],
            "Notification": [ { "type": "command", "command": "beep" } ],
        });
        let parsed = parse_hooks("user", Path::new("/s.json"), &hooks);
        let brief: Vec<(&str, &str, &str, &str)> = parsed
            .iter()
            .map(|h| (h.event.as_str(), h.matcher.as_str(), h.kind.as_str(), h.action.as_str()))
            .collect();
        assert_eq!(
            brief,
            [
                ("Notification", "", "command", "beep"),
                ("PreToolUse", "Bash", "command", "guard.sh"),
                ("PreToolUse", "Bash", "prompt", "Is this safe?"),
                ("Stop", "", "http", "http://localhost/done"),
            ]
        );
        assert_eq!(parsed[1].options, [("timeout".to_string(), "10".to_string())]);
        assert_eq!(parsed[3].options, [("async".to_string(), "true".to_string())]);
    }

    #[test]
    fn finds_hooks_in_frontmatter_only() {
        let skill = "---\nname: guard\nhooks:\n  PreToolUse:\n    - matcher: Bash\nmodel: x\n---\nhooks: not this\n";
        assert_eq!(
            frontmatter_hooks(skill).as_deref(),
            Some("hooks:\n  PreToolUse:\n    - matcher: Bash")
        );
        assert_eq!(frontmatter_hooks("---\nname: plain\n---\nhooks: body text\n"), None);
        assert_eq!(frontmatter_hooks("hooks: no frontmatter\n"), None);
    }

    #[test]
    fn reads_settings_plugins_and_switches_for_a_project() {
        let root = std::env::temp_dir().join(format!("claude-watcher-hooks-{}", std::process::id()));
        let (project, config, plugin) = (root.join("project"), root.join("config"), root.join("plugin"));
        let write = |path: PathBuf, text: String| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            project.join(".claude/settings.json"),
            json!({ "disableAllHooks": true, "statusLine": { "type": "command", "command": "project-line.sh" },
                    "enabledPlugins": { "off@market": false },
                    "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "hello" } ] } ] } })
            .to_string(),
        );
        write(project.join(".claude/settings.local.json"), "{ not json".to_string());
        write(project.join(".claude/skills/guard/SKILL.md"), "---\nhooks:\n  Stop: []\n---\n".to_string());
        write(
            config.join("settings.json"),
            json!({ "statusLine": { "type": "command", "command": "user-line.sh" },
                    "enabledPlugins": { "fmt@market": true, "off@market": true, "elsewhere@market": true } })
            .to_string(),
        );
        write(
            config.join("plugins/installed_plugins.json"),
            json!({ "plugins": {
                "fmt@market": [ { "scope": "user", "installPath": plugin } ],
                "off@market": [ { "scope": "user", "installPath": root.join("off") } ],
                "elsewhere@market": [ { "scope": "project", "projectPath": "/another/project", "installPath": root.join("elsewhere") } ],
            } })
            .to_string(),
        );
        write(
            plugin.join("hooks/hooks.json"),
            json!({ "hooks": { "PostToolUse": [ { "matcher": "Edit|Write", "hooks": [ { "type": "command", "command": "fmt" } ] } ] } })
                .to_string(),
        );

        let report = collect_from(&project, &config, &root.join("managed-settings.json"));
        let rendered = report.render();
        fs::remove_dir_all(&root).unwrap();

        let states: Vec<(&str, &str)> = report
            .sources
            .iter()
            .map(|s| {
                let state = match s.state {
                    State::Absent => "absent",
                    State::Unreadable(_) => "unreadable",
                    State::Read { hooks: 0 } => "no hooks",
                    State::Read { .. } => "hooks",
                };
                (s.scope.as_str(), state)
            })
            .collect();
        assert_eq!(
            states,
            [
                ("managed", "absent"),
                ("project local", "unreadable"),
                ("project", "hooks"),
                ("user", "no hooks"),
                // Disabled by the project, and installed for another project: both skipped.
                ("plugin fmt@market", "hooks"),
            ]
        );
        let brief: Vec<(&str, &str, &str)> = report
            .hooks
            .iter()
            .map(|h| (h.scope.as_str(), h.event.as_str(), h.action.as_str()))
            .collect();
        assert_eq!(brief, [("project", "SessionStart", "hello"), ("plugin fmt@market", "PostToolUse", "fmt")]);
        assert_eq!(report.disabled_by.as_deref(), Some("project"));
        assert_eq!(report.status_line, Some(("project".to_string(), "project-line.sh".to_string())));
        assert_eq!(report.scoped.len(), 1);
        assert_eq!(report.scoped[0].declaration, "hooks:\n  Stop: []");
        assert!(rendered.contains("UNREADABLE: invalid JSON"), "{rendered}");
        assert!(rendered.contains("disableAllHooks is set in project settings"), "{rendered}");
    }
}
