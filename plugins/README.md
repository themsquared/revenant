# Native plugins

Extend revenant's **core** in Rust. A plugin is a crate that implements one or
both extension traits and registers itself; it's compiled into the agent.

This is one of three extensibility paths:

| Path | For | Mechanism |
|---|---|---|
| **Native plugins** (this) | extending the core in Rust — new tools, turn hooks | compile-in, `inventory` auto-registration |
| **MCP servers** | external capability, any language | `revenant mcp add` → gateway multiplex |
| **WASM** (planned) | dynamic, sandboxed, drop-in, any language | wasmtime component model |

## Write one

```toml
# plugins/my-plugin/Cargo.toml
[package]
name = "my-plugin"
version.workspace = true
edition.workspace = true
license.workspace = true
[dependencies]
revenant-plugin = { workspace = true }
```

```rust
// plugins/my-plugin/src/lib.rs
use revenant_plugin::*;

struct Weather;

#[async_trait]
impl Tool for Weather {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "weather".into(),
            description: "Current weather for a city.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
        }
    }
    fn permission(&self) -> PermissionTier { PermissionTier::Network }
    async fn invoke(&self, _cx: &ToolCx, args: serde_json::Value) -> ToolOutput {
        let city = args.get("city").and_then(|v| v.as_str()).unwrap_or("?");
        ToolOutput::ok(format!("it's always sunny in {city}"))
    }
}
register_tool!(Weather);

// Hooks fire around every top-level turn — guardrails, logging, metrics.
struct Audit;
#[async_trait]
impl Hook for Audit {
    fn name(&self) -> &str { "audit" }
    async fn pre_turn(&self, cx: &HookCx) {
        eprintln!("turn on session {}", cx.session_id);
    }
}
register_hook!(Audit);
```

## Wire it in

1. Add the crate under `plugins/` (it's a workspace member via `plugins/*`).
2. Alias it in the root `Cargo.toml` `[workspace.dependencies]`.
3. Add it to the `revenant` binary: an optional dep + the `plugins` feature,
   and `extern crate my_plugin as _;` in `crates/revenant/src/main.rs` (this
   forces the linker to keep the registration).
4. `cargo build`. Your tool shows up in `/v1/tools` and the agent can call it;
   your hook fires around turns.

`revenant-plugin-example` is a complete reference (a `roll_dice` tool + a
`turn-logger` hook).

## Extension points

- **`Tool`** — a capability the agent can invoke. Permission-tiered
  (ReadOnly / WriteWorkspace / Network / Dangerous); Dangerous crosses the
  approval broker automatically. Gets a `ToolCx` (session, home, store, memory).
- **`Hook`** — `pre_turn` / `post_turn` around every top-level turn. Observe
  and react (log, alert, gather metrics); the transcript stays owned by core.

Channels and pluggable embedders are the next extension points.
