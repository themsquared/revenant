//! revenant-skills: agentskills.io SKILL.md read path.
//!
//! Progressive disclosure: only name+description lines are preloaded into
//! the system prompt; `use_skill` pulls the full body as a tool result;
//! `read_skill_file` pulls references/scripts content on demand.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

pub struct SkillIndex {
    root: PathBuf,
    skills: RwLock<BTreeMap<String, Skill>>,
}

impl SkillIndex {
    pub fn new(root: PathBuf) -> Self {
        SkillIndex { root, skills: RwLock::new(BTreeMap::new()) }
    }

    /// Rescan `<root>/*/SKILL.md`. Invalid skills are skipped with a warning
    /// — one broken frontmatter must not take the index down.
    pub fn scan(&self) -> Result<usize> {
        let mut found = BTreeMap::new();
        if self.root.exists() {
            for entry in std::fs::read_dir(&self.root)? {
                let dir = entry?.path();
                let manifest = dir.join("SKILL.md");
                if !dir.is_dir() || !manifest.exists() {
                    continue;
                }
                match parse_skill(&manifest, &dir) {
                    Ok(skill) => {
                        found.insert(skill.name.clone(), skill);
                    }
                    Err(err) => {
                        tracing::warn!("skipping skill at {}: {err:#}", dir.display());
                    }
                }
            }
        }
        let count = found.len();
        *self.skills.write().unwrap() = found;
        Ok(count)
    }

    /// One line per skill for the system prompt (Layer 1).
    pub fn index_lines(&self) -> String {
        let skills = self.skills.read().unwrap();
        skills
            .values()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.skills.read().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<Skill> {
        self.skills.read().unwrap().values().cloned().collect()
    }

    pub fn find(&self, query: &str) -> Vec<Skill> {
        let q = query.to_lowercase();
        self.skills
            .read()
            .unwrap()
            .values()
            .filter(|s| {
                s.name.to_lowercase().contains(&q) || s.description.to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }

    /// Full SKILL.md body (frontmatter included — it's cheap and useful).
    pub fn body(&self, name: &str) -> Result<String> {
        let skill = self.get(name).with_context(|| format!("no skill named '{name}'"))?;
        Ok(std::fs::read_to_string(skill.dir.join("SKILL.md"))?)
    }

    /// Create or replace a skill from agent-authored content: validate the
    /// frontmatter round-trips, write `<name>/SKILL.md` atomically, rescan.
    /// The markdown is live immediately (it's just prompt text the agent
    /// chose to write); any `scripts/` stay gated behind exec approval.
    pub fn write_skill(&self, name: &str, description: &str, body: &str) -> Result<()> {
        let name = name.trim();
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || name.is_empty()
        {
            bail!("skill name must be non-empty kebab/snake ascii, got '{name}'");
        }
        if description.trim().is_empty() {
            bail!("skill description is required");
        }
        if description.len() > 1024 {
            bail!("description over 1024 chars defeats progressive disclosure");
        }
        let content = format!("---\nname: {name}\ndescription: {}\n---\n\n{}\n", description.trim(), body.trim());
        // Round-trip check before writing anything.
        let dir = self.root.join(name);
        parse_skill_str(&content).context("authored skill failed validation")?;

        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        let tmp = dir.join(".SKILL.md.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        self.scan()?;
        Ok(())
    }

    /// Read a file inside a skill's directory, jailed against traversal.
    pub fn read_file(&self, name: &str, rel: &str) -> Result<String> {
        let skill = self.get(name).with_context(|| format!("no skill named '{name}'"))?;
        let base = skill.dir.canonicalize()?;
        let path = base.join(rel);
        let resolved = path
            .canonicalize()
            .with_context(|| format!("no file '{rel}' in skill '{name}'"))?;
        if !resolved.starts_with(&base) {
            bail!("path '{rel}' escapes the skill directory");
        }
        Ok(std::fs::read_to_string(resolved)?)
    }
}

fn parse_skill(manifest: &Path, dir: &Path) -> Result<Skill> {
    let raw = std::fs::read_to_string(manifest)?;
    let (name, description) = parse_skill_str(&raw)?;
    Ok(Skill { name, description, dir: dir.to_path_buf() })
}

/// Validate + extract (name, description) from SKILL.md text.
fn parse_skill_str(raw: &str) -> Result<(String, String)> {
    let rest = raw
        .strip_prefix("---")
        .context("SKILL.md must start with '---' YAML frontmatter")?;
    let end = rest.find("\n---").context("unterminated YAML frontmatter")?;
    let fm: Frontmatter =
        serde_yaml::from_str(&rest[..end]).context("frontmatter needs 'name' and 'description'")?;
    if fm.name.is_empty() || fm.description.is_empty() {
        bail!("frontmatter 'name' and 'description' must be non-empty");
    }
    if fm.description.len() > 1024 {
        bail!("description over 1024 chars defeats progressive disclosure");
    }
    Ok((fm.name, fm.description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_and_read() {
        let root = std::env::temp_dir().join(format!("rev-skills-{}", std::process::id()));
        let dir = root.join("greet");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Greets people warmly\n---\n\nSay hello enthusiastically.\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "extra context").unwrap();

        let index = SkillIndex::new(root.clone());
        assert_eq!(index.scan().unwrap(), 1);
        assert!(index.index_lines().contains("greet: Greets people"));
        assert!(index.body("greet").unwrap().contains("enthusiastically"));
        assert_eq!(index.read_file("greet", "notes.txt").unwrap(), "extra context");
        assert!(index.read_file("greet", "../../etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
