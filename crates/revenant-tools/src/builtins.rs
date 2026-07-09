//! Built-in tools. Each is a small struct; heavy shared logic lives in the
//! jail and the store.

use crate::{Jail, Tool, ToolCx};
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
        Arc::new(MemoryRead { home: home.clone() }),
        Arc::new(MemoryAppend { home: home.clone() }),
        Arc::new(UseSkill { skills: skills.clone() }),
        Arc::new(FindSkill { skills: skills.clone() }),
        Arc::new(ReadSkillFile { skills }),
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
            "Full-text search across past conversations and memory notes. Use before asking the user something they may have already told you.",
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
