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

// Re-export so plugin crates and the `register_tool!` macro can reach it.
pub use inventory;

/// A tool contributed by a plugin, collected at startup via `inventory`. A
/// plugin crate registers with `register_tool!` and gets compiled in; the
/// runtime folds these into the ToolRegistry alongside built-ins.
pub struct ToolPlugin {
    pub make: fn() -> Arc<dyn Tool>,
}
inventory::collect!(ToolPlugin);

/// Register a tool from a plugin crate. `register_tool!(MyTool)` for a unit
/// struct, or `register_tool!(MyTool::new())` for a constructor expression.
#[macro_export]
macro_rules! register_tool {
    ($ctor:expr) => {
        $crate::inventory::submit! {
            $crate::ToolPlugin { make: || ::std::sync::Arc::new($ctor) as ::std::sync::Arc<dyn $crate::Tool> }
        }
    };
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
        // Plugin-contributed tools first; built-ins win any name clash so a
        // plugin extends rather than silently overrides core capabilities.
        for plugin in inventory::iter::<ToolPlugin> {
            let tool = (plugin.make)();
            tools.insert(tool.spec().name.clone(), tool);
        }
        let plugin_count = tools.len();
        for tool in builtins::all(home, skills) {
            tools.insert(tool.spec().name.clone(), tool);
        }
        if plugin_count > 0 {
            tracing::info!("loaded {plugin_count} plugin tool(s)");
        }
        ToolRegistry { tools: Arc::new(tools) }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// (name, description, permission tier) for every built-in — for the UI.
    pub fn describe(&self) -> Vec<(String, String, PermissionTier)> {
        self.tools
            .values()
            .map(|t| {
                let spec = t.spec();
                (spec.name, spec.description, t.permission())
            })
            .collect()
    }
}
