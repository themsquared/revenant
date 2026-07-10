//! Reference native plugin: proves both extension points.
//! - `roll_dice` tool: a new capability the agent can call.
//! - `turn-logger` hook: fires around every top-level turn.

use revenant_plugin::*;

// ---- a Tool plugin ----

struct Dice;

#[async_trait]
impl Tool for Dice {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "roll_dice".into(),
            description: "Roll an N-sided die (default 6) and return the result.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "sides": { "type": "integer", "description": "faces (default 6)" } }
            }),
        }
    }
    fn permission(&self) -> PermissionTier {
        PermissionTier::ReadOnly
    }
    async fn invoke(&self, _cx: &ToolCx, args: serde_json::Value) -> ToolOutput {
        let sides = args.get("sides").and_then(|v| v.as_u64()).filter(|n| *n >= 2).unwrap_or(6);
        // Cheap entropy without pulling a rng dep into the example.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let roll = 1 + (nanos % sides);
        ToolOutput::ok(format!("🎲 rolled {roll} on a d{sides}"))
    }
}

register_tool!(Dice);

// ---- a Hook plugin ----

struct TurnLogger;

#[async_trait]
impl Hook for TurnLogger {
    fn name(&self) -> &str {
        "turn-logger"
    }
    async fn pre_turn(&self, cx: &HookCx) {
        let preview: String = cx.user_text.chars().take(60).collect();
        eprintln!("[plugin:turn-logger] session {} → {preview}", cx.session_id);
    }
    async fn post_turn(&self, cx: &HookCx, final_text: &str) {
        eprintln!(
            "[plugin:turn-logger] session {} done ({} chars out)",
            cx.session_id,
            final_text.len()
        );
    }
}

register_hook!(TurnLogger);
