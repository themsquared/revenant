//! The markdown vault: source of truth for memory.
//!
//! Notes are Obsidian-idiomatic markdown — YAML frontmatter, `[[wikilinks]]`,
//! fact bullets under `## Facts` with machine provenance in trailing HTML
//! comments (invisible in Obsidian's reading view), strikethrough for
//! invalidated facts. The parser is deliberately conservative: anything it
//! doesn't recognize is preserved byte-for-byte on render, so human edits
//! survive agent rewrites.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frontmatter {
    pub uid: String,
    pub kind: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    /// Unrecognized keys, preserved verbatim (rendered after known keys).
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FactLine {
    pub uid: Option<String>,
    /// Human-visible text (may contain [[wikilinks]]), without the comment.
    pub text: String,
    /// Wikilink targets mentioned in the text.
    pub links: Vec<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    /// Provenance: (session_id, message_id).
    pub msg: Option<(i64, i64)>,
    pub struck: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelLine {
    pub uid: Option<String>,
    pub rel: String,
    pub target: String,
}

/// A parsed note. `body_pre` is everything between frontmatter and the first
/// managed section; `tail` is everything after the managed sections — both
/// preserved verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub front: Frontmatter,
    pub title: String,
    pub body_pre: String,
    pub facts: Vec<FactLine>,
    pub relations: Vec<RelLine>,
    pub tail: String,
}

impl Note {
    pub fn new(uid: &str, kind: &str, title: &str) -> Note {
        Note {
            front: Frontmatter {
                uid: uid.to_string(),
                kind: kind.to_string(),
                aliases: vec![],
                tags: vec![kind.to_string()],
                extra: BTreeMap::new(),
            },
            title: title.to_string(),
            body_pre: String::new(),
            facts: vec![],
            relations: vec![],
            tail: String::new(),
        }
    }

    pub fn parse(raw: &str) -> Result<Note> {
        let rest = raw
            .strip_prefix("---\n")
            .or_else(|| raw.strip_prefix("---\r\n"))
            .context("note must start with '---' YAML frontmatter")?;
        let end = rest.find("\n---").context("unterminated frontmatter")?;
        let yaml_src = &rest[..end];
        let after = rest[end + 4..].trim_start_matches(['\r', '\n']);

        let mut map: BTreeMap<String, serde_yaml::Value> =
            serde_yaml::from_str(yaml_src).context("invalid YAML frontmatter")?;
        let take_str = |m: &mut BTreeMap<String, serde_yaml::Value>, k: &str| -> String {
            m.remove(k)
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default()
        };
        let take_list = |m: &mut BTreeMap<String, serde_yaml::Value>, k: &str| -> Vec<String> {
            m.remove(k)
                .map(|v| match v {
                    serde_yaml::Value::Sequence(seq) => seq
                        .into_iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect(),
                    serde_yaml::Value::String(s) => vec![s],
                    _ => vec![],
                })
                .unwrap_or_default()
        };
        let front = Frontmatter {
            uid: take_str(&mut map, "uid"),
            kind: take_str(&mut map, "kind"),
            aliases: take_list(&mut map, "aliases"),
            tags: take_list(&mut map, "tags"),
            extra: map,
        };
        if front.uid.is_empty() {
            bail!("frontmatter missing 'uid'");
        }

        // Split body into: title line, pre-section prose, ## Facts, ## Relations, tail.
        let mut title = String::new();
        let mut body_pre = String::new();
        let mut facts = Vec::new();
        let mut relations = Vec::new();
        let mut tail = String::new();

        #[derive(PartialEq)]
        enum Section {
            Pre,
            Facts,
            Relations,
            Tail,
        }
        let mut section = Section::Pre;

        for line in after.lines() {
            let trimmed = line.trim_end();
            if section == Section::Pre && title.is_empty() {
                if let Some(t) = trimmed.strip_prefix("# ") {
                    title = t.trim().to_string();
                    continue;
                }
            }
            match trimmed {
                "## Facts" => {
                    section = Section::Facts;
                    continue;
                }
                "## Relations" => {
                    section = Section::Relations;
                    continue;
                }
                _ => {}
            }
            // Any OTHER heading after a managed section ends managed parsing.
            if matches!(section, Section::Facts | Section::Relations)
                && trimmed.starts_with("## ")
            {
                section = Section::Tail;
                tail.push_str(line);
                tail.push('\n');
                continue;
            }
            match section {
                Section::Pre => {
                    body_pre.push_str(line);
                    body_pre.push('\n');
                }
                Section::Facts => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        facts.push(parse_fact_line(item));
                    }
                }
                Section::Relations => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        if let Some(rel) = parse_rel_line(item) {
                            relations.push(rel);
                        }
                    }
                }
                Section::Tail => {
                    tail.push_str(line);
                    tail.push('\n');
                }
            }
        }

        Ok(Note {
            front,
            title,
            body_pre: body_pre.trim_matches('\n').to_string(),
            facts,
            relations,
            tail: tail.trim_matches('\n').to_string(),
        })
    }

    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("uid: {}\n", self.front.uid));
        if !self.front.kind.is_empty() {
            out.push_str(&format!("kind: {}\n", self.front.kind));
        }
        if !self.front.aliases.is_empty() {
            out.push_str(&format!("aliases: [{}]\n", self.front.aliases.join(", ")));
        }
        if !self.front.tags.is_empty() {
            out.push_str(&format!("tags: [{}]\n", self.front.tags.join(", ")));
        }
        for (key, value) in &self.front.extra {
            let rendered = serde_yaml::to_string(value).unwrap_or_default();
            out.push_str(&format!("{key}: {}\n", rendered.trim_end()));
        }
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n", self.title));
        if !self.body_pre.is_empty() {
            out.push('\n');
            out.push_str(&self.body_pre);
            out.push('\n');
        }
        if !self.facts.is_empty() {
            out.push_str("\n## Facts\n");
            for fact in &self.facts {
                out.push_str(&render_fact_line(fact));
                out.push('\n');
            }
        }
        if !self.relations.is_empty() {
            out.push_str("\n## Relations\n");
            for rel in &self.relations {
                let comment = rel
                    .uid
                    .as_ref()
                    .map(|u| format!(" <!-- f:{u} -->"))
                    .unwrap_or_default();
                out.push_str(&format!("- {} [[{}]]{}\n", rel.rel, rel.target, comment));
            }
        }
        if !self.tail.is_empty() {
            out.push('\n');
            out.push_str(&self.tail);
            out.push('\n');
        }
        out
    }

    /// Strike a fact by uid; returns true if found.
    pub fn strike_fact(&mut self, uid: &str, until: Option<&str>) -> bool {
        for fact in &mut self.facts {
            if fact.uid.as_deref() == Some(uid) && !fact.struck {
                fact.struck = true;
                fact.valid_until = until.map(String::from).or(fact.valid_until.take());
                return true;
            }
        }
        false
    }
}

/// Extract `[[wikilink]]` targets (piped display stripped).
pub fn wikilinks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start + 2..].find("]]") else { break };
        let inner = &rest[start + 2..start + 2 + end];
        let target = inner.split('|').next().unwrap_or(inner).trim();
        // Strip #heading / ^block refs
        let target = target.split(['#']).next().unwrap_or(target).trim();
        if !target.is_empty() {
            out.push(target.to_string());
        }
        rest = &rest[start + 2 + end + 2..];
    }
    out
}

fn parse_fact_line(item: &str) -> FactLine {
    // Split off trailing HTML comment: "text <!-- f:uid from:x until:y msg:s/m -->"
    let (visible, comment) = match item.rfind("<!--") {
        Some(pos) if item[pos..].contains("-->") => {
            let comment = item[pos + 4..].trim_end().trim_end_matches("-->").trim();
            (item[..pos].trim_end(), Some(comment))
        }
        _ => (item.trim_end(), None),
    };

    let mut uid = None;
    let mut valid_from = None;
    let mut valid_until = None;
    let mut msg = None;
    if let Some(comment) = comment {
        for token in comment.split_whitespace() {
            if let Some(v) = token.strip_prefix("f:") {
                uid = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("from:") {
                valid_from = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("until:") {
                valid_until = Some(v.to_string());
            } else if let Some(v) = token.strip_prefix("msg:") {
                let mut parts = v.splitn(2, '/');
                if let (Some(s), Some(m)) = (parts.next(), parts.next()) {
                    if let (Ok(s), Ok(m)) = (s.parse(), m.parse()) {
                        msg = Some((s, m));
                    }
                }
            }
        }
    }

    // Strikethrough: "~~text~~ (until 2025-11)" or plain "~~text~~"
    let mut struck = false;
    let mut text = visible.to_string();
    if let Some(stripped) = strip_strike(&text) {
        struck = true;
        text = stripped;
    }

    let links = wikilinks(&text);
    FactLine { uid, text, links, valid_from, valid_until, msg, struck }
}

/// "~~foo~~ (until X)" -> "foo" (the parenthetical is regenerated on render).
fn strip_strike(text: &str) -> Option<String> {
    let start = text.find("~~")?;
    let end = text[start + 2..].find("~~")?;
    Some(text[start + 2..start + 2 + end].trim().to_string())
}

fn render_fact_line(fact: &FactLine) -> String {
    let mut comment_parts = Vec::new();
    if let Some(uid) = &fact.uid {
        comment_parts.push(format!("f:{uid}"));
    }
    if let Some(from) = &fact.valid_from {
        comment_parts.push(format!("from:{from}"));
    }
    if let Some(until) = &fact.valid_until {
        comment_parts.push(format!("until:{until}"));
    }
    if let Some((session, message)) = &fact.msg {
        comment_parts.push(format!("msg:{session}/{message}"));
    }
    let comment = if comment_parts.is_empty() {
        String::new()
    } else {
        format!(" <!-- {} -->", comment_parts.join(" "))
    };

    if fact.struck {
        let until = fact
            .valid_until
            .as_ref()
            .map(|u| format!(" (until {u})"))
            .unwrap_or_default();
        format!("- ~~{}~~{until}{comment}", fact.text)
    } else {
        format!("- {}{comment}", fact.text)
    }
}

fn parse_rel_line(item: &str) -> Option<RelLine> {
    let (visible, comment) = match item.rfind("<!--") {
        Some(pos) if item[pos..].contains("-->") => {
            (item[..pos].trim_end(), Some(item[pos + 4..].trim_end().trim_end_matches("-->").trim()))
        }
        _ => (item.trim_end(), None),
    };
    let link_start = visible.find("[[")?;
    let rel = visible[..link_start].trim().to_string();
    let target = wikilinks(&visible[link_start..]).into_iter().next()?;
    let uid = comment.and_then(|c| {
        c.split_whitespace()
            .find_map(|t| t.strip_prefix("f:").map(String::from))
    });
    Some(RelLine { uid, rel, target })
}

// ---- vault IO ----

pub struct Vault {
    root: PathBuf,
}

impl Vault {
    pub fn new(root: PathBuf) -> Self {
        Vault { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entity_path(&self, name: &str) -> PathBuf {
        self.root.join("entities").join(format!("{}.md", slug(name)))
    }

    pub fn episode_path(&self, date: &str, hint: &str) -> PathBuf {
        self.root.join("episodes").join(format!("{date}-{}.md", slug(hint)))
    }

    /// Walk all notes: (vault-relative path, raw content).
    pub fn walk(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for sub in ["entities", "episodes"] {
            let dir = self.root.join(sub);
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let rel = format!(
                        "{sub}/{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    out.push((rel, std::fs::read_to_string(&path)?));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn read(&self, rel: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.root.join(rel))?)
    }

    /// Atomic write (tmp + rename). Returns the content hash for the
    /// watcher's self-event suppression map.
    pub fn write_atomic(&self, rel: &str, content: &str) -> Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Sha256::digest(content.as_bytes()).into())
    }
}

/// kebab-case filename slug.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

/// Normalized entity name for resolution: lowercase alphanumeric + spaces.
pub fn norm_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
uid: e-9f3c2a
kind: person
aliases: [Mike, mikemoore]
tags: [person]
---

# Mike Moore

Field engineer at [[Solo.io]] working on agentgateway.

## Facts
- Works at [[Solo.io]] as a field engineer <!-- f:a1b2c3 from:2024-01 msg:42/1187 -->
- Prefers Rust for systems tooling <!-- f:d4e5f6 msg:42/1190 -->
- ~~Lives in Austin~~ (until 2025-11) <!-- f:778899 from:2023-05 until:2025-11 msg:38/990 -->

## Relations
- works_at [[Solo.io]] <!-- f:a1b2c3 -->

## Notes by the human
Anything down here is untouched.
"#;

    #[test]
    fn parse_extracts_everything() {
        let note = Note::parse(SAMPLE).unwrap();
        assert_eq!(note.front.uid, "e-9f3c2a");
        assert_eq!(note.front.aliases, vec!["Mike", "mikemoore"]);
        assert_eq!(note.title, "Mike Moore");
        assert!(note.body_pre.contains("[[Solo.io]]"));
        assert_eq!(note.facts.len(), 3);
        assert_eq!(note.facts[0].uid.as_deref(), Some("a1b2c3"));
        assert_eq!(note.facts[0].links, vec!["Solo.io"]);
        assert_eq!(note.facts[0].msg, Some((42, 1187)));
        assert!(note.facts[2].struck);
        assert_eq!(note.facts[2].valid_until.as_deref(), Some("2025-11"));
        assert_eq!(note.facts[2].text, "Lives in Austin");
        assert_eq!(note.relations.len(), 1);
        assert_eq!(note.relations[0].rel, "works_at");
        assert!(note.tail.contains("untouched"));
    }

    #[test]
    fn round_trip_is_stable() {
        let note = Note::parse(SAMPLE).unwrap();
        let rendered = note.render();
        let reparsed = Note::parse(&rendered).unwrap();
        assert_eq!(note, reparsed);
        // Second render is byte-identical (fixed point).
        assert_eq!(rendered, reparsed.render());
    }

    #[test]
    fn strike_updates_fact() {
        let mut note = Note::parse(SAMPLE).unwrap();
        assert!(note.strike_fact("d4e5f6", Some("2026-07")));
        let rendered = note.render();
        assert!(rendered.contains("- ~~Prefers Rust for systems tooling~~ (until 2026-07)"));
        assert!(!note.strike_fact("nonexistent", None));
    }

    #[test]
    fn wikilink_extraction() {
        assert_eq!(
            wikilinks("met [[Jane Doe|Jane]] at [[Solo.io#office]] and [[]]"),
            vec!["Jane Doe", "Solo.io"]
        );
    }

    #[test]
    fn slugs_and_norms() {
        assert_eq!(slug("Mike Moore"), "mike-moore");
        assert_eq!(slug("Solo.io"), "solo-io");
        assert_eq!(norm_name("  Solo.IO!! "), "solo io");
    }
}
