# revenant

**The agent that comes back.** A lean, security-first personal AI agent + harness in Rust,
built natively on [agentgateway](https://agentgateway.dev) OSS.

Revenant is an OpenClaw-class always-on agent — chat channels, skills, cron loops,
subagents — with one architectural difference that changes everything: **the harness never
talks to a model provider directly.** All LLM, MCP, and A2A traffic flows through a bundled,
supervised agentgateway, which owns provider keys, model aliasing, failover, guardrails,
token/cost budgets, and GenAI telemetry. The harness renders gateway config; the gateway
enforces it. Prompt injection can't exfiltrate keys the process doesn't have, and it can't
blow past spend caps enforced below the agent.

```
you ──▶ telegram / web / tui / cli ──▶ revenant ──▶ agentgateway ──▶ any model, any MCP server
                                        (harness)    (data plane: aliases, failover,
                                                      budgets, guardrails, metrics)
```

## Status: M0 — walking skeleton

- [x] Cargo workspace, six crates, acyclic deps
- [x] SQLite store (WAL, FTS5-ready, single-writer actor): sessions, messages, spend ledger, approvals
- [x] Anthropic Messages streaming client (the gateway cross-translates to every provider)
- [x] Gateway supervision: pinned+checksummed binary download, config render → `--validate-only` → atomic swap, spawn, readiness on the data path, restart with backoff
- [x] Model tiers (`fast` / `balanced` / `deep` / `local`) rendered as gateway aliases + priority failover virtual models; missing API keys degrade tiers gracefully
- [x] `revenant init` / `up` / `chat` / `render`
- [x] E2E verified: streamed multi-turn chat through the supervised gateway to a local Ollama model, history persisted across restarts, per-turn token accounting

Next (M1+): tools + approval broker, skills (agentskills.io, self-authoring with git audit),
self-tuning loops, control-plane API, TUI + web UI, Telegram, MCP plugin bus, kagent A2A.
See the design doc for the full roadmap.

## Quick start

```sh
cargo build --release
./target/release/revenant init     # writes ~/.revenant, captures keys, pins the gateway
./target/release/revenant chat     # supervised gateway + streaming REPL
```

No cloud key? Add a local tier to `~/.revenant/config.toml` and chat entirely offline:

```toml
[[tiers.local.targets]]
provider = "ollama"
model = "qwen3:0.6b"
```

```sh
revenant chat --tier local
```

## License

Apache-2.0
