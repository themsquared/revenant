//! `~/.revenant/` layout helper.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Resolve the home dir: `$REVENANT_HOME` override, else `~/.revenant`.
    pub fn resolve() -> Self {
        let root = std::env::var_os("REVENANT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .expect("cannot resolve home directory")
                    .join(".revenant")
            });
        Home { root }
    }

    /// Construct a Home rooted at an explicit path (tests, isolated runs).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Home { root: root.into() }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn db_path(&self) -> PathBuf {
        self.root.join("revenant.db")
    }
    pub fn secrets_path(&self) -> PathBuf {
        self.root.join("secrets.env")
    }
    pub fn gateway_dir(&self) -> PathBuf {
        self.root.join("gateway")
    }
    pub fn gateway_bin_dir(&self) -> PathBuf {
        self.gateway_dir().join("bin")
    }
    pub fn gateway_config_path(&self) -> PathBuf {
        self.gateway_dir().join("config.yaml")
    }
    pub fn gateway_config_next_path(&self) -> PathBuf {
        self.gateway_dir().join("config.yaml.next")
    }
    pub fn workspace_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }
    pub fn personalities_dir(&self) -> PathBuf {
        self.root.join("personalities")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn memory_dir(&self) -> PathBuf {
        self.workspace_dir().join("memory")
    }
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }
    /// Sandboxed dynamic WASM plugins (`*.wasm`) loaded at daemon startup.
    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }
    /// This revenant's self-sovereign network identity (Ed25519 keypair).
    pub fn identity_dir(&self) -> PathBuf {
        self.root.join("identity")
    }
}
