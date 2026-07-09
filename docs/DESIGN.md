# REVENANT — a gateway-native Rust agent harness

## Context

Mike wants an OpenClaw-class personal agent (always-on daemon, chat channels, skills, loops, subagents) rebuilt as a lean, security-first **Rust** harness that delegates ALL model/MCP/guardrail plumbing to **agentgateway OSS** — making the harness both a genuinely great personal agent and the flagship agentgateway showcase. The landscape check confirms the lane is open: OpenClaw is Node (and CVE-riddled), Hermes is Python (Nous Research), NemoClaw is an NVIDIA safety stack. The only Rust prior art to study is `thClaws`. OpenClaw's two reputation killers — security holes and $200+/day runaway spend — are exactly what a gateway-owned architecture fixes.

**Name: `revenant`** — the agent that comes back from the dead and will not stop. Crate name is free on crates.io (verified; `grimoire`/`warlock` are taken, `hexcast` is the free backup). New repo at `~/revenant`, Apache-2.0.

**Locked decisions (from Q&A):** bundled + supervised agentgateway child process (harness owns/renders gateway config, hot-reloads it); web UI **and** TUI from day one on one shared control-plane API; Telegram is the v1 channel; kagent integration as a stretch milestone.

## Design stance (four rules everything follows from)

1. **The gateway is the only AI egress.** The harness never holds a provider key, never implements a provider SDK. It speaks **Anthropic Messages** (`/v1/messages`) to `localhost:<llm_port>` with tier aliases (`fast`/`balanced`/`deep`/`local`) — never real model names. Tiering, failover, guardrails, budgets, and cost telemetry are agentgateway config that revenant *renders*. Cross-translation means any upstream (OpenAI/Gemini/Bedrock/Ollama/…) still works.
2. **Everything is an actor with a mailbox.** Sessions, scheduler, gateway supervisor, channels = tokio tasks over `mpsc`. Per-session serialized queues fall out for free.
3. **SQLite is truth; markdown is the human mirror.** DB (rusqlite bundled + FTS5, WAL, single writer actor) for sessions/messages/loops/spend/approvals. `workspace/` + `skills/` are git repos (via `gix`) — human-readable, auditable, revertable.
4. **Untrusted by default.** Channel input, tool results, and agent-authored skills/loops are untrusted. Capability escalation always crosses an approval gate that reaches Mike on his channel (Telegram inline buttons / web / TUI, first-writer-wins CAS).

## Architecture

```
Telegram ─┐                                  ┌─ OpenAI/Anthropic/Gemini/Bedrock/…
Web UI ───┤   ┌──────────── revenant ─────┐  │
TUI ──────┼──▶│ control-plane API (axum)  │  │      ┌─────────────────────────┐
CLI ──────┘   │ session actors ─ turn loop│──┼─────▶│ agentgateway (child)     │
              │ skills ─ loops ─ approvals│ /v1/messages  aliases/failover     │
              │ store (SQLite) ─ security │  │      │ budgets/guardrails       │
              │ gateway supervisor ───────┼──┼─────▶│ MCP multiplex ──▶ MCP srvrs
              └───────────────────────────┘ rmcp    │ A2A proxy ──▶ kagent     │
                     ▲ metrics scrape (GenAI cost)  └─────────────────────────┘
```

## Workspace layout

```
~/revenant/
├── crates/
│   ├── revenant-core        # domain types, Channel/Tool/ToolCx traits — zero I/O
│   ├── revenant-store       # rusqlite (bundled, fts5) persistence + migrations
│   ├── revenant-llm         # Anthropic Messages wire client (reqwest rustls + SSE)
│   ├── revenant-mcp         # rmcp client → gateway multiplex; rmcp server (kagent-facing); A2A client module
│   ├── revenant-tools       # built-ins: fs, exec, recall, memory_*, skill_*, loop_*, subagent_run, escalate
│   ├── revenant-skills      # SKILL.md index/activation + self-authoring pipeline + gix audit
│   ├── revenant-loops       # croner scheduler, loop CRUD, weekly reflection pass
│   ├── revenant-agent       # session actors, turn engine, context assembly, compaction
│   ├── revenant-gateway     # child supervision + agentgateway YAML render/validate/swap + metrics scrape
│   ├── revenant-security    # permission tiers, approval broker, sandbox exec (landlock/sandbox-exec), secrets
│   ├── revenant-channels    # Channel impls; telegram = hand-rolled thin client (~9 Bot API methods, feature-gated)
│   ├── revenant-control     # axum /v1 API + SSE bus + embedded web UI (rust-embed, brotli-precompressed)
│   ├── revenant-client      # typed API client (used by CLI + TUI — they never touch the DB)
│   ├── revenant-tui         # ratatui: chat, approvals-takeover modal, status
│   └── revenant             # bin: clap CLI + daemon wiring
├── web/                     # React 18 + Vite + Tailwind + TanStack Query + uPlot (≤300KB gz initial)
├── xtask/                   # build-ui, dist, gateway-compat-check
└── installer/install.sh     # detect arch → fetch harness + PINNED agentgateway → sha256 + minisign → init
```

Key deps: tokio, axum, rusqlite(bundled,fts5), reqwest(rustls), eventsource-stream, rmcp, croner, gix, landlock, keyring, rust-embed, ratatui, clap, notify, tracing. Static musl targets: `x86_64/aarch64-unknown-linux-musl` (cargo-zigbuild) + `aarch64-apple-darwin`. Pi-ready.

## Subsystem decisions (distilled)

**Turn loop:** context assembly → stream from gateway → tool dispatch (permission check → approval park/resume → concurrent JoinSet for independent tool_use blocks) → iterate until `end_turn` / guards (25 iterations, per-turn token+cost budget, wall clock). Cancellable mid-turn via `Steer`/stop. Subagents = child sessions, tier −1, tool allowlist, cost slice, depth ≤2, results return as tool_result.

**Token management:** 4-layer cache-breakpoint-aligned system prompt (identity+tools / skills index+MCP schemas / MEMORY.md+daily notes / tiny dynamic tail); byte-stable cached layers tested with golden snapshots. Compaction at 70% of the *smallest* context window in the tier's failover chain, summarize-oldest-50% via `fast` tier; exact counts via gateway `/v1/messages/count_tokens`. Tool results >2k tokens stored whole, truncated in-context with `expand_result(ref)` re-expansion. Nightly prune loop (archive idle sessions, vacuum).

**Spend (the anti-OpenClaw design):** two independent layers — (1) **gateway-enforced** token/cost rate limits per tier + per-loop (`x-revenant-loop-id` CEL keys) + global daily USD cap: can't be prompt-injected around; (2) harness ledger from per-response usage + 30s Prometheus scrape of `gen_ai_client_cost`/`token_usage` → spend dashboards. Exhaustion ladder: degrade to cheaper tier → `local` (Ollama) → notify once → pause non-local loops. Escalation to `deep` only via explicit `escalate(reason)` tool or `!deep` prefix.

**Skills (self-authoring):** agentskills.io SKILL.md standard, model-driven activation (`use_skill`, `find_skill` FTS). `skill_create/skill_update` tools: validate → dry-run parse → atomic write → **gix commit** (audit/revert) → `trust=untrusted`. Markdown is live immediately; **`scripts/` execution refused until owner approves the diff** (trust pinned to content hash; any edit re-drops trust).

**Loops (self-tuning):** loops table + run history with cost/outcome/`useful` (👍/👎 from Telegram). Agent tools `loop_create/update/pause/runs`; safe tunings (pause, slower, cheaper) apply immediately, capability expansions require approval. Weekly system reflection loop reviews run stats and proposes tunings via approval prompt. Rails: min interval 60s, required per-run + daily budgets on agent-created loops (defaults $0.05/run, $1/day), max 20 loops, concurrency semaphore.

**Gateway supervision:** pinned+checksummed agentgateway binary in `~/.revenant/gateway/bin/`; spawn with secrets as env only (keyring or `secrets.env.enc`, never in YAML/DB/logs); readiness = `GET /v1/models`; restart w/ backoff; degraded mode after 5 failures (fail fast + notify, pause loops). Config pipeline: typed structs → YAML with `$ENV` refs → `--validate-only` → atomic rename → gateway hot-reload (keeps last-good). External-gateway mode supported via config.

**Security:** tool tiers ReadOnly / WriteWorkspace (path-jailed) / Network (allowlist) / Dangerous (approval broker, TTL 15m default-deny, full audit trail). Exec sandbox: clean env + rlimits + timeouts always; Landlock + no-new-privs on Linux; `sandbox-exec` on macOS; optional container mode. Control plane binds 127.0.0.1:7717, bearer token *always* (keyring/0600 file), no cookies → no CSRF; remote = Tailscale-first docs (`bind = "tailscale"` auto-detect), never a public port by default.

**Control-plane API:** REST + SSE (curl-able, resumable via `Last-Event-ID`; no WebSocket/gRPC). ~22 endpoints: sessions/messages/stream, skills CRUD + revision diffs, loops CRUD + runs + trigger, approvals CAS, spend (by day/model/loop), subagent tree, config PATCH → re-render pipeline, gateway status/reload, channel pairing. Global event bus + per-session token stream.

**Surfaces:** Web UI pages — chat, sessions, skills (CodeMirror + agent-diff view), loops, **spend dashboard (the agentgateway GenAI metrics showcase)**, subagents tree, approvals inbox, settings. TUI — chat, approvals takeover modal (killer feature for ssh'd-into-Pi), status. CLI — init/up/status/chat/skill/loop/approvals/logs/doctor/service/open/tui; talks to daemon over API only (exceptions: init, doctor, up).

**Plugins (layered):** (A) **MCP servers are the primary plugin bus** — `revenant mcp add <name> --stdio "cmd"` patches gateway config + hot-reloads; every community MCP server is instantly a plugin, governed by gateway CEL authz. (B) Skills folders. (C) v1.x: subprocess plugins over JSON-RPC/stdio ("RPP", MCP-shaped) for channels/hooks, supervised like the gateway. WASM hooks deferred to v2.

**kagent (stretch, `--features kagent`):** A2A client module (agent card fetch, `message/send`, `message/stream` SSE) as `call_kagent_agent` tool, routed through the gateway's A2A proxy; inbound = streamable-HTTP MCP endpoint (`send_message_to_agent`, `run_skill`, `list_skills`) registered in kagent as a `RemoteMCPServer`. Flagship demo: Pi agent ⇄ kagent cluster agents, both directions, one gateway metering everything.

**Release engineering:** GH Actions — job 1 builds web UI → job 2 matrix embeds it (build fails if `web/dist` missing); tarballs + SHA256SUMS + minisign; multi-arch container (cosign keyless); `gateway-compat.toml` pin + weekly canary CI against latest agentgateway OSS release.

## `~/.revenant/` runtime layout

```
config.toml · revenant.db · secrets.env.enc (or keyring) · token
skills/ (git) · workspace/ (git: MEMORY.md, notes/, projects/) · plugins/
gateway/{bin/agentgateway-vX.Y.Z, config.yaml, config.yaml.next} · logs/ · run/
```

## Milestones

| M | Contents | Demoable |
|---|---|---|
| **M0** wk1-2 | Workspace; core types; store (sessions/messages); gateway supervisor + config render for one tier; Messages SSE client; single-session agent; `revenant chat` REPL | Streamed chat on Mac **and Pi** through supervised gateway; kill a provider key mid-chat → **gateway failover live**; survives restart |
| **M1** wk3-5 | Tool trait + built-ins (fs/exec-basic/recall/memory); permission tiers + approval broker; skills read path; control-plane API + SSE; TUI (chat/approvals/status) | ssh to Pi → TUI chat; approval interrupt modal; curl the SSE bus |
| **M2** wk6-7 | Telegram channel (long-poll, streaming edits, pairing codes, inline-button approvals) | Phone: pair, streamed replies, **approve a dangerous action from Telegram** while TUI shows it resolve |
| **M3** wk8-10 | Web UI core (chat/sessions/approvals/**spend dashboard**/gateway status); compaction + count_tokens; full tier render + gateway budgets + metrics scrape + spend ledger | Laptop → Pi over Tailscale: cost-by-model/day burn-down — *the agentgateway metrics demo* |
| **M4** wk11-13 | Skill self-authoring (validate/gix/trust-gate) + loops engine + reflection; skills/loops UI; `install.sh` + minisign + `service install` + `doctor`; container image | Fresh Pi: `curl \| sh` → wizard → paired Telegram + running loop in <10 min; agent writes its own skill, diff appears in UI for approval |
| **M5** wk14-16 | rmcp client → gateway MCP multiplex; `revenant mcp add`; plugin protocol (RPP) host + example plugin; Landlock/sandbox hardening; egress-proxy for scripts | One command adds a community MCP server; third-party channel plugin binary drops in and lights up |
| **M6** wk17-20 | kagent A2A both directions; compat-matrix CI; docs site; v1.0 API freeze | **Pi agent ⇄ kagent agents through one metered gateway** — flagship Solo demo |

## First implementation steps (this session → M0)

1. `git init ~/revenant`; workspace `Cargo.toml` (release profile: lto=fat, strip, panic=abort); crate skeletons for `revenant-core`, `revenant-store`, `revenant-llm`, `revenant-gateway`, `revenant-agent`, `revenant` bin.
2. `revenant-core`: `ContentBlock`, `SessionKey`, `SessionMsg`, `Channel`/`Tool`/`ToolCx` traits, tier enum, error types.
3. `revenant-store`: schema v1 (sessions, messages, spend_ledger, approvals to start), migrations, single-writer actor.
4. `revenant-gateway`: download/pin helper (dev mode: use a locally built agentgateway or GH release), config render for tiers → aliases + failover, `--validate-only` + atomic swap, spawn/readiness/restart.
5. `revenant-llm`: Messages request/stream types, SSE parsing, count_tokens.
6. `revenant-agent`: session actor + minimal turn loop (no tools yet).
7. `revenant` bin: `init` (minimal: keys → secrets file, one tier), `up --foreground`, `chat` REPL.

## Verification

- **M0 exit test (end-to-end):** `cargo run -- init` with a real Anthropic key → `up` spawns agentgateway (readiness on `/v1/models`) → `chat` streams a multi-turn conversation via tier alias `balanced` → kill/restart daemon → history persists → break the primary target in the tier → conversation continues via failover target. Run on macOS first; add `aarch64-unknown-linux-musl` build to CI in week 1 even before owning a Pi test box.
- Unit: golden-snapshot tests for rendered gateway YAML (validated with `agentgateway --validate-only` in CI); golden prompts for cache-layer byte-stability; store round-trip tests.
- `curl -N localhost:7717/v1/events` smoke for the SSE bus (M1+).

## Open items (non-blocking, decide during build)

- Name veto window: `revenant` recommended (crates.io free); backup `hexcast`. Squatting the crates.io name early is step 0 once confirmed.
- Which agentgateway OSS release to pin first (check latest v1.0.x tag at build time; dev against locally built binary from `~/agentgateway`).
- GitHub org/repo home (personal vs solo-io) — publish decision can wait until M0 works.
