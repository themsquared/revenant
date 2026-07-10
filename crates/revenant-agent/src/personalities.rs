//! Selectable personalities — a swappable *voice* layer.
//!
//! A personality is a markdown file `~/.revenant/personalities/<name>.md`
//! (frontmatter: name, description, emoji; body = the voice directive). It's
//! injected below the identity/safety rules so it can flavor tone but never
//! override how the agent behaves. Cosmetic only — no tools/model scoping
//! (that's what subagent defs are for). Files-as-truth, like everything else.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct Personality {
    pub name: String,
    pub description: String,
    pub emoji: String,
    pub voice: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    emoji: String,
}

pub struct PersonalityRegistry {
    root: PathBuf,
    items: RwLock<BTreeMap<String, Personality>>,
}

impl PersonalityRegistry {
    pub fn new(root: PathBuf) -> Self {
        PersonalityRegistry { root, items: RwLock::new(BTreeMap::new()) }
    }

    pub fn scan(&self) -> Result<usize> {
        let mut found = BTreeMap::new();
        if self.root.exists() {
            for entry in std::fs::read_dir(&self.root)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                match parse(&std::fs::read_to_string(&path)?, &path) {
                    Ok(p) => {
                        found.insert(p.name.clone(), p);
                    }
                    Err(err) => tracing::warn!("skipping personality {}: {err:#}", path.display()),
                }
            }
        }
        let count = found.len();
        *self.items.write().unwrap() = found;
        Ok(count)
    }

    pub fn get(&self, name: &str) -> Option<Personality> {
        self.items.read().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<Personality> {
        self.items.read().unwrap().values().cloned().collect()
    }

    pub fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.md", slug(name)))
    }

    pub fn write(&self, p: &Personality) -> Result<()> {
        if p.name.trim().is_empty() {
            bail!("personality name is required");
        }
        if p.voice.trim().is_empty() {
            bail!("personality voice directive is required");
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(&p.name);
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, render(p))?;
        std::fs::rename(&tmp, &path)?;
        self.scan()?;
        Ok(())
    }
}

fn parse(raw: &str, path: &std::path::Path) -> Result<Personality> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .context("personality must start with '---' frontmatter")?;
    let end = rest.find("\n---").context("unterminated frontmatter")?;
    let mut fm: Frontmatter =
        serde_yaml::from_str(&rest[..end]).context("invalid personality frontmatter")?;
    let voice = rest[end + 4..].trim_start_matches(['\r', '\n']).trim().to_string();
    if fm.name.is_empty() {
        fm.name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unnamed").to_string();
    }
    if voice.is_empty() {
        bail!("personality '{}' has an empty voice directive", fm.name);
    }
    Ok(Personality { name: fm.name, description: fm.description, emoji: fm.emoji, voice })
}

fn render(p: &Personality) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", p.name));
    if !p.description.is_empty() {
        out.push_str(&format!("description: {}\n", p.description));
    }
    if !p.emoji.is_empty() {
        out.push_str(&format!("emoji: {}\n", p.emoji));
    }
    out.push_str("---\n\n");
    out.push_str(p.voice.trim());
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
    let t = out.trim_end_matches('-').to_string();
    if t.is_empty() { "unnamed".into() } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("rev-persona-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = PersonalityRegistry::new(dir.clone());
        reg.write(&Personality {
            name: "deadpan".into(),
            description: "dry and unimpressed".into(),
            emoji: "😐".into(),
            voice: "Reply with dry, terse understatement.".into(),
        })
        .unwrap();
        assert_eq!(reg.scan().unwrap(), 1);
        let p = reg.get("deadpan").unwrap();
        assert_eq!(p.emoji, "😐");
        assert!(p.voice.contains("understatement"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
