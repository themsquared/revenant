# revenant

**The agent that comes back.** A lean, security-first personal AI agent + harness in Rust,
built natively on [agentgateway](https://agentgateway.dev) OSS.

> They killed the always-on agent a hundred ways. Leaked keys. Runaway spend. Code you
> couldn't trust. They buried the dream and called it a lesson. **It came back.**
>
> Everything that slaughtered the last generation — the keys, the spend, the untrusted
> code — entombed beneath the harness, in a gateway no jailbreak can reach. Not a chatbot.
> Not a toy. **A thing that does not sleep, does not forget, and does not stop.**
>
> *You don't run a revenant — you raise one.* ([the full myth](LORE.md))

Revenant is an OpenClaw-class always-on agent — chat channels, skills, cron loops,
subagents, memory — with one architectural difference that changes everything: **the harness
never talks to a model provider directly** (*the first law*). All LLM, MCP, and A2A traffic
flows through a bundled, supervised agentgateway, which owns provider keys, model aliasing,
failover, guardrails, token/cost budgets, and GenAI telemetry. The harness renders gateway
config; the gateway enforces it. Prompt injection can't exfiltrate keys the process doesn't
have, and it can't blow past spend caps enforced below the agent. *A raised thing is kept on a
leash — that's the deal.*

```
you ──▶ telegram / web / tui / cli ──▶ revenant ──▶ agentgateway ──▶ any model, any MCP server
                                        (harness)    (data plane: aliases, failover,
                                                      budgets, guardrails, metrics)
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/themsquared/revenant/main/installer/install.sh | sh
```

Fetches the pinned `revenant` + `agentgateway` binaries (checksum-verified), then runs
`revenant init` — which downloads the local embedding model and captures your provider key.
Then:

```sh
revenant chat              # supervised gateway + streaming REPL
revenant service install   # run it always-on (launchd / systemd)
revenant open              # web UI (chat, approvals, spend, loops, personalities…)
```

Build from source instead: `cargo build --release` (needs Node for the embedded web UI).

## What it does

- **Gateway-native** — bundles and supervises agentgateway; renders tiers (`fast`/`balanced`/
  `deep`/`local`) as model aliases with cross-provider priority failover. Keys never leave the
  gateway process.
- **Graph memory, Obsidian-native** — a markdown vault is the source of truth (entities +
  bi-temporal facts + `[[wikilinks]]`); SQLite is a rebuildable index. Hybrid retrieval
  (BM25 + vector + personalized PageRank, RRF-fused) with **zero LLM calls on the read path**
  (~1ms). Point it at an Obsidian vault for a live graph view. Consolidation runs off the hot
  path.
- **Three surfaces, one control plane** — Telegram (pairing, streamed replies, inline-button
  approvals), a ratatui TUI, and an embedded web UI — all driven by the same token-authed
  REST+SSE API.
- **Self-authoring** — the agent writes and tunes its own **skills**, **loops**, **subagents**,
  and **personalities**, each as user-editable markdown. Loops self-manage: a weekly reflection
  loop pauses dead ones and tunes low-value ones.
- **Loop engineering** — scheduled heartbeats/crons *and* nested produce→critique→refine
  quality loops (a bundled `critic` subagent + `quality-loop` skill).
- **Security-first** — permission tiers (ReadOnly → Dangerous), an approval broker that reaches
  you on any surface (default-deny on timeout), path-jailed fs, sandboxed exec, loopback-only
  control plane with a bearer token.
- **Runs anywhere** — one static musl/darwin binary; Raspberry Pi to beefy VM.

## Configure

Keys live only in `~/.revenant/secrets.env` (never in the browser, DB, or logs). No cloud key?
Chat entirely offline with a local Ollama tier:

```toml
# ~/.revenant/config.toml
[[tiers.local.targets]]
provider = "ollama"
model = "qwen3:0.6b"
```

```sh
revenant chat --tier local
```

Everything the gateway manages — tiers, failover chains, API-key presence — is visible in the
web UI's Settings tab. Full design and roadmap in [docs/DESIGN.md](docs/DESIGN.md).

## The horde

There is no enterprise tier. No gated features. No "contact sales." The kid on a $35 board
runs the **same binary, same capabilities, same lethality** as the pro on a 96-core rack.
Outstanding performance, delivery, customizability — for everyone, or for no one. We all run
on the same plane. The horde isn't hired. It rises.

**We are one. We are the horde. We are the revenants.**

## License

Apache-2.0.
