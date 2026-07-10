//! Configurable subagent definitions.
//!
//! Each is a markdown file under `~/.revenant/agents/<name>.md`: YAML
//! frontmatter (tier, tools allowlist, skills) plus a body that becomes the
//! child agent's directive. Revenant can auto-draft these; the user owns and
//! edits them (in any editor or the web UI). Same philosophy as skills —
//! files are the source of truth, the registry is a rebuildable view.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Model tier the child runs on ("fast"/"balanced"/"deep"/"local").
    pub tier: Option<String>,
    /// Tool allowlist. Empty = inherit all of the parent's non-dangerous
    /// tools. Names outside this set are refused for the child.
    pub tools: Vec<String>,
    /// Skills surfaced to the child (informational for now; the child sees
    /// the full skills index regardless — a future filter hook).
    pub skills: Vec<String>,
    /// The directive/system instructions (markdown body).
    pub directive: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
}

pub struct AgentRegistry {
    root: PathBuf,
    agents: RwLock<BTreeMap<String, AgentDef>>,
}

impl AgentRegistry {
    pub fn new(root: PathBuf) -> Self {
        AgentRegistry { root, agents: RwLock::new(BTreeMap::new()) }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Rescan `<root>/*.md`. Invalid definitions are skipped with a warning.
    pub fn scan(&self) -> Result<usize> {
        let mut found = BTreeMap::new();
        if self.root.exists() {
            for entry in std::fs::read_dir(&self.root)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                match parse_agent(&std::fs::read_to_string(&path)?, &path) {
                    Ok(def) => {
                        found.insert(def.name.clone(), def);
                    }
                    Err(err) => tracing::warn!("skipping agent {}: {err:#}", path.display()),
                }
            }
        }
        let count = found.len();
        *self.agents.write().unwrap() = found;
        Ok(count)
    }

    pub fn get(&self, name: &str) -> Option<AgentDef> {
        self.agents.read().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<AgentDef> {
        self.agents.read().unwrap().values().cloned().collect()
    }

    /// One line per agent for the parent's system prompt, so the model knows
    /// which named subagents it can delegate to.
    pub fn roster_lines(&self) -> String {
        self.agents
            .read()
            .unwrap()
            .values()
            .map(|a| format!("- {}: {}", a.name, a.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.md", slug(name)))
    }

    /// Write (create or replace) an agent definition file, then rescan.
    pub fn write(&self, def: &AgentDef) -> Result<()> {
        if def.name.trim().is_empty() {
            bail!("agent name is required");
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(&def.name);
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, render_agent(def))?;
        std::fs::rename(&tmp, &path)?;
        self.scan()?;
        Ok(())
    }
}

fn parse_agent(raw: &str, path: &std::path::Path) -> Result<AgentDef> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .context("agent file must start with '---' frontmatter")?;
    let end = rest.find("\n---").context("unterminated frontmatter")?;
    let mut fm: Frontmatter =
        serde_yaml::from_str(&rest[..end]).context("invalid agent frontmatter")?;
    let directive = rest[end + 4..].trim_start_matches(['\r', '\n']).trim().to_string();
    if fm.name.is_empty() {
        // Fall back to the filename.
        fm.name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
    }
    if directive.is_empty() {
        bail!("agent '{}' has an empty directive body", fm.name);
    }
    Ok(AgentDef {
        name: fm.name,
        description: fm.description,
        tier: fm.tier,
        tools: fm.tools,
        skills: fm.skills,
        directive,
    })
}

fn render_agent(def: &AgentDef) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", def.name));
    if !def.description.is_empty() {
        out.push_str(&format!("description: {}\n", def.description));
    }
    if let Some(tier) = &def.tier {
        out.push_str(&format!("tier: {tier}\n"));
    }
    if !def.tools.is_empty() {
        out.push_str(&format!("tools: [{}]\n", def.tools.join(", ")));
    }
    if !def.skills.is_empty() {
        out.push_str(&format!("skills: [{}]\n", def.skills.join(", ")));
    }
    out.push_str("---\n\n");
    out.push_str(def.directive.trim());
    out.push('\n');
    out
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_render_round_trip() {
        let dir = std::env::temp_dir().join(format!("rev-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = AgentRegistry::new(dir.clone());
        let def = AgentDef {
            name: "Researcher".into(),
            description: "digs into topics".into(),
            tier: Some("fast".into()),
            tools: vec!["recall".into(), "read_file".into()],
            skills: vec![],
            directive: "You research things and return cited summaries.".into(),
        };
        reg.write(&def).unwrap();
        assert!(dir.join("researcher.md").exists());
        assert_eq!(reg.scan().unwrap(), 1);
        let got = reg.get("Researcher").unwrap();
        assert_eq!(got.tier.as_deref(), Some("fast"));
        assert_eq!(got.tools, vec!["recall", "read_file"]);
        assert!(got.directive.contains("cited summaries"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
