//! Path jail: every fs tool resolves paths against a set of allowed roots
//! and refuses anything that escapes (including via symlinks).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Jail {
    roots: Vec<PathBuf>,
}

impl Jail {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Jail { roots }
    }

    /// Resolve a user/model-supplied path for READING. Relative paths are
    /// resolved against the first root (the workspace).
    pub fn resolve_read(&self, raw: &str) -> Result<PathBuf> {
        let path = self.join(raw);
        let resolved = path
            .canonicalize()
            .with_context(|| format!("path not found: {raw}"))?;
        self.check(&resolved, raw)?;
        Ok(resolved)
    }

    /// Resolve a path for WRITING: the target may not exist yet, so the
    /// deepest EXISTING ancestor is jail-checked BEFORE any directory is
    /// created — otherwise an out-of-jail path would create dirs there.
    pub fn resolve_write(&self, raw: &str) -> Result<PathBuf> {
        let path = self.join(raw);
        let file_name = path
            .file_name()
            .map(|n| n.to_owned())
            .with_context(|| format!("not a file path: {raw}"))?;
        let parent = path.parent().with_context(|| format!("no parent dir for: {raw}"))?;

        let mut probe = parent.to_path_buf();
        while !probe.exists() {
            probe = probe
                .parent()
                .with_context(|| format!("no existing ancestor for: {raw}"))?
                .to_path_buf();
        }
        self.check(&probe.canonicalize()?, raw)?;

        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {raw}"))?;
        let resolved_parent = parent.canonicalize()?;
        self.check(&resolved_parent, raw)?;
        Ok(resolved_parent.join(file_name))
    }

    fn join(&self, raw: &str) -> PathBuf {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.roots[0].join(p)
        }
    }

    fn check(&self, resolved: &Path, raw: &str) -> Result<()> {
        for root in &self.roots {
            if let Ok(root) = root.canonicalize() {
                if resolved.starts_with(&root) {
                    return Ok(());
                }
            }
        }
        bail!(
            "path '{raw}' is outside the allowed workspace (allowed roots: {})",
            self.roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jail_blocks_escape() {
        let root = std::env::temp_dir().join(format!("rev-jail-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/ok.txt"), "fine").unwrap();
        let jail = Jail::new(vec![root.clone()]);

        assert!(jail.resolve_read("sub/ok.txt").is_ok());
        assert!(jail.resolve_read("../../../etc/passwd").is_err());
        assert!(jail.resolve_read("/etc/passwd").is_err());
        assert!(jail.resolve_write("new/deep/file.txt").is_ok());
        assert!(jail.resolve_write("/tmp/outside.txt").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
