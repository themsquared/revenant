//! revenant-tools: the Tool trait and built-in tools.
//!
//! Errors are returned as `ToolOutput::err` (visible to the model), never as
//! Rust errors — a failing tool must not kill the turn.

pub mod builtins;
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
    /// Risk of a SPECIFIC call, given its arguments. The dispatcher gates on
    /// this (not the static tier), so a tool can auto-allow its routine, safe
    /// invocations and only prompt for the consequential ones — e.g. `exec ls`
    /// vs `exec rm -rf`. Defaults to the static tier (no per-call nuance).
    fn risk(&self, _input: &serde_json::Value) -> PermissionTier {
        self.permission()
    }
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

/// A resolved A2A peer the `call_agent` tool can reach: `url` is already the
/// governed gateway-egress URL (or the direct URL on a substrate), and `token`
/// is the resolved bearer, if any.
#[derive(Clone)]
pub struct A2aTarget {
    pub name: String,
    pub url: String,
    pub token: Option<String>,
    pub via_gateway: bool,
    /// SHA-256 fingerprint the peer's TLS cert must match (SEC-4 pinning).
    /// Only meaningful for direct https targets; the dial fails closed.
    pub tls_fp: Option<String>,
}

impl ToolRegistry {
    pub fn builtin(
        home: &Home,
        skills: Arc<SkillIndex>,
        a2a: Vec<A2aTarget>,
        extra: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        // Plugin-contributed tools first (compiled-in via inventory, plus any
        // dynamically-loaded `extra` such as WASM plugins); built-ins win any
        // name clash so a plugin extends rather than silently overrides core.
        for plugin in inventory::iter::<ToolPlugin> {
            let tool = (plugin.make)();
            tools.insert(tool.spec().name.clone(), tool);
        }
        for tool in extra {
            tools.insert(tool.spec().name.clone(), tool);
        }
        let plugin_count = tools.len();
        for tool in builtins::all(home, skills) {
            tools.insert(tool.spec().name.clone(), tool);
        }
        if !a2a.is_empty() {
            let t = Arc::new(builtins::CallAgent::new(a2a, home.clone()));
            tools.insert(t.spec().name.clone(), t);
        }
        if plugin_count > 0 {
            tracing::info!("loaded {plugin_count} plugin tool(s)");
        }
        ToolRegistry { tools: Arc::new(tools) }
    }

    /// A registry with only file tools jailed to `root` — for the Ascension
    /// actuator editing an isolated worktree.
    pub fn coder(root: &std::path::Path) -> Self {
        let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        for tool in builtins::coder(root) {
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
