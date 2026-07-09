//! revenant-tools: the Tool trait and built-in tools.
//!
//! Errors are returned as `ToolOutput::err` (visible to the model), never as
//! Rust errors — a failing tool must not kill the turn.

mod builtins;
mod jail;

pub use jail::Jail;

use revenant_core::home::Home;
use revenant_core::{PermissionTier, ToolOutput, ToolSpec};
use revenant_skills::SkillIndex;
use revenant_store::Store;
use std::collections::BTreeMap;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn permission(&self) -> PermissionTier;
    async fn invoke(&self, cx: &ToolCx, args: serde_json::Value) -> ToolOutput;
}

/// Capability handle passed to tools — deliberately narrow.
#[derive(Clone)]
pub struct ToolCx {
    pub session_id: i64,
    pub home: Home,
    pub store: Store,
    pub memory: Option<std::sync::Arc<revenant_memory::MemoryEngine>>,
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn builtin(home: &Home, skills: Arc<SkillIndex>) -> Self {
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        for tool in builtins::all(home, skills) {
            tools.insert(tool.spec().name.clone(), tool);
        }
        ToolRegistry { tools: Arc::new(tools) }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }
}
