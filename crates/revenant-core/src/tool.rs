//! The Tool contract. Tools are the agent's hands; every invocation crosses
//! a permission check, and Dangerous ones cross the approval broker.

use serde::{Deserialize, Serialize};

/// Escalating capability tiers. The dispatcher auto-allows up to the
/// session's grant level and routes anything above it through approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionTier {
    /// Reads within the workspace/skills jail, recall search.
    ReadOnly,
    /// Writes within the workspace/skills jail.
    WriteWorkspace,
    /// Outbound network beyond the gateway (allowlisted hosts).
    Network,
    /// Arbitrary execution or capability expansion. Always needs approval
    /// unless a standing session grant exists.
    Dangerous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input (Anthropic `input_schema` shape).
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), is_error: false }
    }
    pub fn err(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), is_error: true }
    }
}
