//! Built-in tools. Each is a small struct; heavy shared logic lives in the
//! jail and the store.

use crate::{A2aTarget, Jail, Tool, ToolCx};
use revenant_core::home::Home;
use revenant_core::{PermissionTier, ToolOutput, ToolSpec};
use revenant_skills::SkillIndex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Tool results larger than this are truncated in-context (full content
/// retrieval via ranges comes with `expand_result` in M2).
const MAX_RESULT_BYTES: usize = 8 * 1024;

pub fn all(home: &Home, skills: Arc<SkillIndex>) -> Vec<Arc<dyn Tool>> {
    let read_jail = Jail::new(vec![home.workspace_dir(), home.skills_dir()]);
    let write_jail = Jail::new(vec![home.workspace_dir()]);
    vec![
        Arc::new(ReadFile { jail: read_jail.clone() }),
        Arc::new(WriteFile { jail: write_jail.clone() }),
        Arc::new(ListDir { jail: read_jail.clone() }),
        Arc::new(Exec { workspace: home.workspace_dir() }),
        Arc::new(Recall),
        Arc::new(MemorySave),
        Arc::new(MemoryRead { home: home.clone() }),
        Arc::new(MemoryAppend { home: home.clone() }),
        Arc::new(UseSkill { skills: skills.clone() }),
        Arc::new(FindSkill { skills: skills.clone() }),
        Arc::new(ReadSkillFile { skills: skills.clone() }),
        Arc::new(SkillWrite { skills }),
        Arc::new(LoopCreate),
        Arc::new(LoopControl),
        Arc::new(LoopUpdate),
    ]
}

fn truncate_result(mut s: String) -> String {
    if s.len() <= MAX_RESULT_BYTES {
        return s;
    }
    let total = s.len();
    let mut end = MAX_RESULT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str(&format!("\n…[truncated: {total} bytes total]"));
    s
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutput> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolOutput::err(format!("missing required string arg '{key}'")))
}

macro_rules! spec {
    ($name:expr, $desc:expr, $schema:expr) => {
        ToolSpec { name: $name.into(), description: $desc.into(), input_schema: $schema }
    };
}

// ---- fs ----

struct ReadFile {
    jail: Jail,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec!(
            "read_file",
            "Read a file from the workspace or skills directory. Relative paths resolve against the workspace.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let path = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        match self.jail.resolve_read(path).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
            Ok(content) => ToolOutput::ok(truncate_result(content)),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

struct WriteFile {
    jail: Jail,
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec!(
            "write_file",
            "Write a file inside the workspace. Creates parent directories. Overwrites existing content.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (path, content) = match (arg_str(&args, "path"), arg_str(&args, "content")) {
            (Ok(p), Ok(c)) => (p, c),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        match self.jail.resolve_write(path).and_then(|p| {
            std::fs::write(&p, content)?;
            Ok(p)
        }) {
            Ok(p) => ToolOutput::ok(format!("wrote {} bytes to {}", content.len(), p.display())),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

struct ListDir {
    jail: Jail,
}

#[async_trait::async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        spec!(
            "list_dir",
            "List a directory in the workspace or skills dir. Defaults to the workspace root.",
            json!({"type":"object","properties":{"path":{"type":"string"}}})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let dir = match self.jail.resolve_read(path) {
            Ok(d) => d,
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut lines: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let suffix = if e.path().is_dir() { "/" } else { "" };
                        format!("{}{}", e.file_name().to_string_lossy(), suffix)
                    })
                    .collect();
                lines.sort();
                ToolOutput::ok(truncate_result(lines.join("\n")))
            }
            Err(err) => ToolOutput::err(format!("reading {}: {err}", dir.display())),
        }
    }
}

// ---- exec ----

struct Exec {
    workspace: std::path::PathBuf,
}

#[async_trait::async_trait]
impl Tool for Exec {
    fn spec(&self) -> ToolSpec {
        spec!(
            "exec",
            "Run a shell command in the workspace (60s timeout, output capped). Call it directly — the system handles any needed owner approval.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Dangerous
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let command = match arg_str(&args, "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin")
            .env("HOME", &self.workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(err) => return ToolOutput::err(format!("spawn failed: {err}")),
        };
        match tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await {
            Err(_) => ToolOutput::err("command timed out after 60s (killed)".to_string()),
            Ok(Err(err)) => ToolOutput::err(format!("exec error: {err}")),
            Ok(Ok(output)) => {
                let mut text = String::new();
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                if !output.stderr.is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                let status = output.status.code().unwrap_or(-1);
                let body = truncate_result(text);
                if output.status.success() {
                    ToolOutput::ok(body)
                } else {
                    ToolOutput::err(format!("exit code {status}\n{body}"))
                }
            }
        }
    }
}

// ---- memory & recall ----

struct Recall;

#[async_trait::async_trait]
impl Tool for Recall {
    fn spec(&self) -> ToolSpec {
        spec!(
            "recall",
            "Hybrid search (keyword + semantic + knowledge graph) across past conversations and the memory vault. Use before asking the user something they may have already told you.",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let query = match arg_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        if let Some(memory) = &cx.memory {
            return match memory.recall(query, 10).await {
                Ok(memories) if memories.is_empty() => ToolOutput::ok("no matches".to_string()),
                Ok(memories) => ToolOutput::ok(
                    memories
                        .iter()
                        .map(|m| {
                            let label = m.note.as_deref().unwrap_or("conversation");
                            format!("- [{label}] {}", m.text)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(err) => ToolOutput::err(format!("recall failed: {err:#}")),
            };
        }
        // Memory disabled: plain FTS fallback.
        match cx.store.recall_search(query, 8).await {
            Ok(hits) if hits.is_empty() => ToolOutput::ok("no matches".to_string()),
            Ok(hits) => ToolOutput::ok(
                hits.iter()
                    .map(|h| format!("[{}#{}] {}", h.source, h.reference, h.snippet))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(err) => ToolOutput::err(format!("recall failed: {err:#}")),
        }
    }
}

struct MemorySave;

#[async_trait::async_trait]
impl Tool for MemorySave {
    fn spec(&self) -> ToolSpec {
        spec!(
            "memory_save",
            "Save a durable memory to the vault (events, project facts, decisions, relationships). Use memory_append only for core owner-profile facts.",
            json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let content = match arg_str(&args, "content") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let Some(memory) = &cx.memory else {
            return ToolOutput::err("memory engine is disabled".to_string());
        };
        match memory.save(content, cx.session_id).await {
            Ok(path) => ToolOutput::ok(format!("saved to {path}")),
            Err(err) => ToolOutput::err(format!("memory_save failed: {err:#}")),
        }
    }
}

struct MemoryRead {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for MemoryRead {
    fn spec(&self) -> ToolSpec {
        spec!(
            "memory_read",
            "Read MEMORY.md — durable facts about the owner and standing instructions.",
            json!({"type":"object","properties":{}})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, _args: Value) -> ToolOutput {
        match std::fs::read_to_string(self.home.workspace_dir().join("MEMORY.md")) {
            Ok(content) => ToolOutput::ok(content),
            Err(_) => ToolOutput::ok("(MEMORY.md is empty — nothing remembered yet)".to_string()),
        }
    }
}

struct MemoryAppend {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for MemoryAppend {
    fn spec(&self) -> ToolSpec {
        spec!(
            "memory_append",
            "Append a durable fact to MEMORY.md (one concise line: owner preferences, standing instructions, important facts).",
            json!({"type":"object","properties":{"fact":{"type":"string"}},"required":["fact"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let fact = match arg_str(&args, "fact") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let path = self.home.workspace_dir().join("MEMORY.md");
        let mut current = std::fs::read_to_string(&path).unwrap_or_default();
        if current.len() + fact.len() > 8_192 {
            return ToolOutput::err(
                "MEMORY.md is at its size cap — consolidate existing entries before adding more"
                    .to_string(),
            );
        }
        if !current.is_empty() && !current.ends_with('\n') {
            current.push('\n');
        }
        current.push_str(&format!("- {fact}\n"));
        if let Err(err) = std::fs::write(&path, &current) {
            return ToolOutput::err(format!("writing MEMORY.md: {err}"));
        }
        let _ = cx.store.recall_index("memory", "MEMORY.md", &current).await;
        ToolOutput::ok("remembered".to_string())
    }
}

// ---- skills ----

struct UseSkill {
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for UseSkill {
    fn spec(&self) -> ToolSpec {
        spec!(
            "use_skill",
            "Load the full instructions of a skill by name (see the skills index in your system prompt).",
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let name = match arg_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        match self.skills.body(name) {
            Ok(body) => ToolOutput::ok(truncate_result(body)),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

struct FindSkill {
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for FindSkill {
    fn spec(&self) -> ToolSpec {
        spec!(
            "find_skill",
            "Search installed skills by keyword when the index in your prompt doesn't obviously cover the task.",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let query = match arg_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let hits = self.skills.find(query);
        if hits.is_empty() {
            ToolOutput::ok("no matching skills".to_string())
        } else {
            ToolOutput::ok(
                hits.iter()
                    .map(|s| format!("- {}: {}", s.name, s.description))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }
}

struct SkillWrite {
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for SkillWrite {
    fn spec(&self) -> ToolSpec {
        spec!(
            "skill_create",
            "Author or update a skill so you can reuse a capability later. Provide a kebab-case name, a one-line description (this is what future-you sees to decide when to load it), and the full instructions as the body. The skill is available immediately.",
            json!({"type":"object","properties":{
                "name":{"type":"string","description":"kebab-case, e.g. draft-standup-update"},
                "description":{"type":"string","description":"one line: when should this skill be used"},
                "body":{"type":"string","description":"the full skill instructions (markdown)"}
            },"required":["name","description","body"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (name, description, body) = match (
            arg_str(&args, "name"),
            arg_str(&args, "description"),
            arg_str(&args, "body"),
        ) {
            (Ok(n), Ok(d), Ok(b)) => (n, d, b),
            (Err(e), ..) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        match self.skills.write_skill(name, description, body) {
            Ok(()) => ToolOutput::ok(format!(
                "skill '{name}' saved and indexed — you can use_skill it now"
            )),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

/// Delegate to another agent over A2A. By default the call is proxied through
/// the gateway (governed egress); `via_gateway=false` means a direct call on a
/// trusted substrate. Never talks to an unlisted URL.
pub struct CallAgent {
    targets: Vec<A2aTarget>,
    http: reqwest::Client,
}

impl CallAgent {
    pub fn new(targets: Vec<A2aTarget>) -> Self {
        CallAgent {
            targets,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CallAgent {
    fn spec(&self) -> ToolSpec {
        let roster = self
            .targets
            .iter()
            .map(|t| {
                format!("{} ({})", t.name, if t.via_gateway { "via gateway" } else { "direct" })
            })
            .collect::<Vec<_>>()
            .join(", ");
        spec!(
            "call_agent",
            format!(
                "Delegate a task to another agent over A2A and return its reply. Known agents: {roster}."
            ),
            json!({"type":"object","properties":{
                "agent":{"type":"string","description":"name of a configured agent"},
                "message":{"type":"string"}
            },"required":["agent","message"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (agent, message) = match (arg_str(&args, "agent"), arg_str(&args, "message")) {
            (Ok(a), Ok(m)) => (a, m),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        let Some(target) = self.targets.iter().find(|t| t.name == agent) else {
            return ToolOutput::err(format!(
                "unknown agent '{agent}' — configure it under [[a2a_agents]]"
            ));
        };
        let body = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": { "message": { "role": "user", "parts": [{ "kind": "text", "text": message }] } }
        });
        let mut req = self.http.post(&target.url).json(&body);
        if let Some(token) = &target.token {
            req = req.bearer_auth(token);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => return ToolOutput::err(format!("A2A call to '{agent}' failed: {err}")),
        };
        if !resp.status().is_success() {
            return ToolOutput::err(format!("agent '{agent}' returned {}", resp.status()));
        }
        let value: Value = match resp.json().await {
            Ok(v) => v,
            Err(err) => return ToolOutput::err(format!("bad A2A response from '{agent}': {err}")),
        };
        if let Some(err) = value.get("error") {
            return ToolOutput::err(format!("agent '{agent}' error: {err}"));
        }
        // A2A reply: result.parts[].text (message) — concatenate text parts.
        let text = value
            .pointer("/result/parts")
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.is_empty() {
            ToolOutput::ok("(agent returned no text)".to_string())
        } else {
            ToolOutput::ok(truncate_result(text))
        }
    }
}

struct LoopCreate;

#[async_trait::async_trait]
impl Tool for LoopCreate {
    fn spec(&self) -> ToolSpec {
        spec!(
            "loop_create",
            "Create a recurring job that runs a prompt on a schedule (heartbeat/cron). Use for standing tasks: check something periodically, a daily summary, a watch. Runs off the main thread; results appear in loop history and can be pushed to a channel.",
            json!({"type":"object","properties":{
                "name":{"type":"string"},
                "schedule":{"type":"string","description":"'every:600s' (min 60s) or 'cron:*/10 * * * *'"},
                "prompt":{"type":"string","description":"what to do each run"},
                "tier":{"type":"string","enum":["fast","balanced","deep","local"],"description":"default fast"},
                "channel_out":{"type":"string","description":"optional: 'telegram' to push results to paired chats"}
            },"required":["name","schedule","prompt"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        // Creating a spending, self-firing job is a capability escalation.
        PermissionTier::Dangerous
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let (name, schedule, prompt) = match (
            arg_str(&args, "name"),
            arg_str(&args, "schedule"),
            arg_str(&args, "prompt"),
        ) {
            (Ok(n), Ok(s), Ok(p)) => (n, s, p),
            (Err(e), ..) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        let tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or("fast");
        let channel_out = args.get("channel_out").and_then(|v| v.as_str());

        // Validate schedule + get first fire time (enforces the 60s floor).
        let now = unix_now();
        let next_run = match revenant_core::loops::first_next_run(schedule, now) {
            Ok(n) => n,
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        // Cap total loops.
        match cx.store.loops_list().await {
            Ok(loops) if loops.len() >= revenant_core::loops::MAX_LOOPS => {
                return ToolOutput::err(format!(
                    "loop limit reached ({}); delete one first",
                    revenant_core::loops::MAX_LOOPS
                ));
            }
            _ => {}
        }
        let id = format!("lp-{}", uuid_short());
        match cx
            .store
            .loop_upsert(&id, name, schedule, prompt, tier, channel_out, 48, "agent", next_run)
            .await
        {
            Ok(()) => ToolOutput::ok(format!("loop '{name}' created ({id}), first run soon")),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

struct LoopControl;

#[async_trait::async_trait]
impl Tool for LoopControl {
    fn spec(&self) -> ToolSpec {
        spec!(
            "loop_control",
            "Inspect and control loops. action: list | runs | pause | resume | delete. 'runs' shows a loop's recent outcomes (use it to decide how to tune). All but 'list' need the loop id.",
            json!({"type":"object","properties":{
                "action":{"type":"string","enum":["list","runs","pause","resume","delete"]},
                "id":{"type":"string"}
            },"required":["action"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        // Listing/pausing is harmless; the create path is the guarded one.
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let action = match arg_str(&args, "action") {
            Ok(a) => a,
            Err(e) => return e,
        };
        match action {
            "list" => match cx.store.loops_list().await {
                Ok(loops) if loops.is_empty() => ToolOutput::ok("no loops defined".to_string()),
                Ok(loops) => ToolOutput::ok(
                    loops
                        .iter()
                        .map(|l| {
                            format!(
                                "- {} [{}] {} · tier {} · {}",
                                l.id,
                                if l.enabled { "on" } else { "paused" },
                                l.schedule,
                                l.tier,
                                l.name
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(err) => ToolOutput::err(format!("{err:#}")),
            },
            "pause" | "resume" => {
                let id = match arg_str(&args, "id") {
                    Ok(i) => i,
                    Err(e) => return e,
                };
                match cx.store.loop_set_enabled(id, action == "resume").await {
                    Ok(true) => ToolOutput::ok(format!("loop {id} {action}d")),
                    Ok(false) => ToolOutput::err(format!("no loop {id}")),
                    Err(err) => ToolOutput::err(format!("{err:#}")),
                }
            }
            "delete" => {
                let id = match arg_str(&args, "id") {
                    Ok(i) => i,
                    Err(e) => return e,
                };
                match cx.store.loop_delete(id).await {
                    Ok(true) => ToolOutput::ok(format!("loop {id} deleted")),
                    Ok(false) => ToolOutput::err(format!("no loop {id}")),
                    Err(err) => ToolOutput::err(format!("{err:#}")),
                }
            }
            "runs" => {
                let id = match arg_str(&args, "id") {
                    Ok(i) => i,
                    Err(e) => return e,
                };
                match cx.store.loop_runs(id, 15).await {
                    Ok(runs) if runs.is_empty() => ToolOutput::ok("no runs yet".to_string()),
                    Ok(runs) => ToolOutput::ok(
                        runs.iter()
                            .map(|r| {
                                format!(
                                    "{} · {}in/{}out tok · {}",
                                    r.status,
                                    r.tokens_in,
                                    r.tokens_out,
                                    r.outcome.as_deref().unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(err) => ToolOutput::err(format!("{err:#}")),
                }
            }
            other => ToolOutput::err(format!("unknown action '{other}'")),
        }
    }
}

struct LoopUpdate;

#[async_trait::async_trait]
impl Tool for LoopUpdate {
    fn spec(&self) -> ToolSpec {
        spec!(
            "loop_update",
            "Tune an existing loop: change its schedule, prompt, or tier. Use this to self-tune — e.g. slow down a loop that keeps finding nothing, or make a valuable one cheaper. Only omitted fields are kept.",
            json!({"type":"object","properties":{
                "id":{"type":"string"},
                "schedule":{"type":"string","description":"new 'every:<n>s' or 'cron:...' (optional)"},
                "prompt":{"type":"string","description":"new prompt (optional)"},
                "tier":{"type":"string","enum":["fast","balanced","deep","local"]}
            },"required":["id"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        // Tuning an existing loop (not spawning a new spender) — WriteWorkspace.
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let id = match arg_str(&args, "id") {
            Ok(i) => i,
            Err(e) => return e,
        };
        let current = match cx.store.loop_get(id).await {
            Ok(Some(l)) => l,
            Ok(None) => return ToolOutput::err(format!("no loop {id}")),
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        let schedule = args.get("schedule").and_then(|v| v.as_str()).unwrap_or(&current.schedule);
        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or(&current.prompt);
        let tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or(&current.tier);
        // Validate + recompute next fire (enforces the interval floor).
        let next_run = match revenant_core::loops::first_next_run(schedule, unix_now()) {
            Ok(n) => n,
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        match cx.store.loop_retune(id, schedule, prompt, tier, next_run).await {
            Ok(true) => ToolOutput::ok(format!("loop {id} retuned: {schedule} · {tier}")),
            Ok(false) => ToolOutput::err(format!("no loop {id}")),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}

fn uuid_short() -> String {
    // Cheap unique-ish id without pulling uuid into this crate's surface.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now:x}")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct ReadSkillFile {
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for ReadSkillFile {
    fn spec(&self) -> ToolSpec {
        spec!(
            "read_skill_file",
            "Read a reference file or script bundled inside a skill's directory.",
            json!({"type":"object","properties":{"name":{"type":"string"},"path":{"type":"string"}},"required":["name","path"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (name, path) = match (arg_str(&args, "name"), arg_str(&args, "path")) {
            (Ok(n), Ok(p)) => (n, p),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        match self.skills.read_file(name, path) {
            Ok(content) => ToolOutput::ok(truncate_result(content)),
            Err(err) => ToolOutput::err(format!("{err:#}")),
        }
    }
}
