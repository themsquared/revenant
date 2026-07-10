//! revenant-plugin: the native plugin API. Add this one dependency, implement
//! `Tool` and/or `Hook`, register with `register_tool!` / `register_hook!`,
//! and add your crate to the `plugins/` workspace + the `revenant` binary's
//! `plugins` feature. Your extension is compiled into the agent's core.
//!
//! This is the "extend at the core" path. For external capability, use MCP
//! servers (`revenant mcp add`); sandboxed dynamic plugins (WASM) come later.
//!
//! ```ignore
//! use revenant_plugin::*;
//!
//! struct Dice;
//! #[async_trait]
//! impl Tool for Dice {
//!     fn spec(&self) -> ToolSpec {
//!         ToolSpec { name: "roll_dice".into(), description: "Roll an N-sided die".into(),
//!             input_schema: serde_json::json!({"type":"object","properties":{"sides":{"type":"integer"}}}) }
//!     }
//!     fn permission(&self) -> PermissionTier { PermissionTier::ReadOnly }
//!     async fn invoke(&self, _cx: &ToolCx, args: serde_json::Value) -> ToolOutput {
//!         let sides = args.get("sides").and_then(|v| v.as_u64()).unwrap_or(6);
//!         ToolOutput::ok(format!("{}", 1 + (now_nanos() % sides)))
//!     }
//! }
//! register_tool!(Dice);
//! ```

// Core types a plugin builds against.
pub use revenant_core::{PermissionTier, ToolOutput, ToolSpec};
// The extension-point traits + their registration machinery.
pub use revenant_agent::{Hook, HookCx};
pub use revenant_tools::{Tool, ToolCx};
// The registration macros (exported at the defining crates' roots).
pub use revenant_agent::register_hook;
pub use revenant_tools::register_tool;
// Convenience re-exports so a plugin needs only this one dependency.
pub use async_trait::async_trait;
pub use serde_json;
