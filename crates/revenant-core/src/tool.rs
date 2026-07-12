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
    /// Publishing to the public revenant network under your identity — an
    /// outward, effectively irreversible act (others can pull what you push).
    /// Its own tier above Dangerous so it ALWAYS crosses the approval broker as
    /// a distinct "publish to the horde" prompt, and a standing `exec` grant
    /// never silently covers it. The gate is `>= Dangerous`, so this is caught.
    Publish,
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

#[cfg(test)]
mod tests {
    use super::PermissionTier::*;

    /// The dispatcher gates approval on `>= Dangerous`. Publishing to the public
    /// network must ALWAYS prompt, so it has to sit at or above that line —
    /// while ordinary outbound Network stays below it (auto-allowed, e.g. web
    /// fetch). This encodes the security contract behind the `Publish` tier.
    #[test]
    fn publish_always_crosses_the_approval_gate() {
        assert!(Publish >= Dangerous, "publish must route through approval");
        assert!(Network < Dangerous, "plain network stays auto-allowed");
        assert!(ReadOnly < WriteWorkspace && WriteWorkspace < Network);
    }
}
