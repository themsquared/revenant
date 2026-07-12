# agentgateway as revenant's control plane

> Grounded against the bundled **agentgateway-enterprise v1.3.1** source
> (`~/agentgateway-enterprise`), run **standalone** (single binary, local YAML —
> *not* the k8s controller/xDS path). Every capability below is marked for
> whether it works in standalone mode, because that's the only mode revenant
> ships. Field names and file:line cites are from that tree.

## Thesis

The first law says the harness holds **zero** provider keys; the gateway owns
keys, routing, budgets. Today we exploit a sliver of that: LLM tier
routing + failover, one global token cap, MCP multiplexing, A2A egress hooks
(`crates/revenant-gateway/src/render.rs`).

The wider move: treat the gateway as revenant's **enforcement kernel** — the one
component the self-modifying agent *cannot* rewrite (it's in the Ascension
denylist), sitting below the harness on the wire. Every identity revenant spawns
— the owner, each named subagent, each ephemeral coding ninja, each horde peer
session — becomes a **scoped, budgeted, guardrailed, audited principal enforced
in the gateway, not in the (untrusted, self-editing) Rust**. A compromised or
badly-self-improved agent is then contained by construction: it can burn only
its own token bucket, call only the MCP tools its identity is scoped to, never
sees a real provider key, and every byte it spends is on the record below it.

That is the moat made literal, and it's most of what agentgateway already does.

## What we exploit today

| Feature | Where |
|---|---|
| Tier → model routing, priority failover, outlier eviction | `render.rs` `virtualModels` + `health.unhealthyExpression` |
| Weighted multi-provider split | `render.rs` weighted routing |
| One global token/request cap | `render.rs` `llm.policies.localRateLimit` from `[spending]` |
| MCP multiplexing (stdio + remote) | `render.rs` `mcp.targets` |
| Governed A2A egress (one bind per remote agent) | `render.rs` `binds[].a2a` |
| Prompt caching markers | `render_model` `promptCaching` |
| **Request-log / analytics DB** | `config.database.url` (added — see below) |

## The full surface (what's on the table, standalone-available marked)

### 1. Observability — the substrate (✅ standalone; now ON)

Setting `config.database.url` (`sqlite://…` or `postgres://…`) turns on the
enterprise request-log store (`telemetry/log_store.rs`). Every LLM/MCP/A2A
request is persisted: `started_at, duration_ms, http_status, error,
gen_ai_provider_name, gen_ai_request_model, input/output/total_tokens, cost,
agentgateway_user, agentgateway_group, user_agent_name, attributes_json` +
optional prompt/completion payloads. Served at admin `:15000`
`/api/logs/{search,get,tail,analytics/summary}` — the Traffic & Analytics UI.

**Why it's a superpower:** this is the ground-truth telemetry the rest of the
system has been missing. The self-improvement engine's fitness axes
(cost/speed/accuracy per model), per-agent spend attribution, and anomaly
detection all want exactly these columns — computed by the gateway for free,
below the harness, un-fakeable by the agent. Revenant's `status`/`spend`/`eval`
should read this store instead of (or alongside) its own bookkeeping.

### 2. Per-agent virtual identity (✅ standalone — the headline)

There is no LiteLLM-style "virtual key" object, but the equivalent is assembled
from two halves, both standalone:

- **Inbound identity** — `apiKey` policy (`http/apikey.rs:252`): a list of
  `{key | keyHash, metadata}` in `strict` mode. `metadata` is free-form JSON;
  put `{user, group, tier}` in it. The gateway derives `agentgateway_user` /
  `agentgateway_group` via a default CEL coalesce
  (`config.rs:22` — `apiKey.user → jwt.sub → …`), which auto-propagates into
  authz rules, rate-limit descriptors, **and the analytics DB columns**.
- **Outbound secret** — `backendAuth` (`http/auth/mod.rs:33`) carries the real
  provider key on the upstream side, invisible to the caller.

**Design:** revenant mints one inbound api-key per identity — owner, each named
subagent, each spawned coding ninja, each horde session — stamped with
`metadata.{user,group,tier}`. Provider keys live only in `backendAuth`. Result:
per-agent audit trail + per-agent policy surface, and a leaked/rogue agent key
is revocable without rotating provider keys. The first law, made per-agent.

> **Open implementation question (the main cost of this phase):** `apiKey` is a
> route/listener policy (`binds[].listeners[].routes[].policies.apiKey`).
> Revenant currently renders the simplified top-level `llm:` shorthand, which
> may not accept an inbound `apiKey`. Confirm whether the `llm` listener takes
> `policies.apiKey`, or whether we render the fuller `binds → listeners →
> routes → backends(ai)` form in front of the LLM block. This is a
> render-layer refactor, not a new gateway capability.

### 3. Guardrails (✅ standalone via the `llm:` block)

`PromptGuard` (`llm/policy/mod.rs:262`) attaches in the simplified config as
`llm.policies.guardrails` (all models) or `llm.models[].guardrails` (per-model)
— so **no config-model switch is needed**. Request-side and response-side
guards:

- **PII regex/redaction** (OSS): builtins `ssn, creditCard, phoneNumber, email,
  caSin`; `action: mask|reject`; runs on prompt and completion.
- **Webhook** (OSS): call out to a local guard model / moderation service; can
  mask or reject.
- **openAIModeration** (req-only), **bedrockGuardrails**, **googleModelArmor**,
  **azureContentSafety** (harm categories + blocklists + `detectJailbreak`,
  req-only) — all OSS.
- **MCP guardrails** — `policies.mcpGuardrails.processors` (`mcp/guardrails/mod.rs:57`):
  remote policy processors over `tools/call`, `*/list`, etc.
- **purviewDlp** — enterprise-only.

**Design:** PII masking on by default (dovetails with the existing `[privacy]`
config and privacy-first stance); optional jailbreak/moderation via a webhook to
a local guard; MCP guardrails on tool calls. Matters most for the autonomous
legs (Ascension) and the horde, where inbound skills/prompts are untrusted.

> **Caveat:** non-webhook guards **fail open** on error (`mod.rs:331`), and
> streaming responses bypass response guards unless `promptGuard.streaming:
> Enabled`. Design accordingly (fail-closed where it matters; enable streaming
> guards for response-side PII).

### 4. Rate limits (✅ token/request standalone; ✗ dollar budgets)

- `localRateLimit` (`http/localratelimit.rs:29`): in-proc token bucket,
  `type: requests | tokens`, `maxTokens/tokensPerFill/fillInterval`. Already
  used for the global cap; can be attached per-tier and (via the `conditional`
  wrapper or per-route) per-identity.
- `remoteRateLimit` (`http/remoteratelimit.rs:51`): Envoy RLS protocol,
  per-consumer via a CEL descriptor `value: apiKey.user`. Needs an external RLS
  service.
- **Dollar-cost budgets** (`http/budget.rs`): **enterprise + k8s-controller/xDS
  only — not deserializable from local YAML** (`budget.rs:44`). Standalone
  cannot enforce per-agent $-budgets.

**Design:** per-agent / per-tier **token** limits so a runaway subagent or
Ascension loop can't drain the wallet. Approximate cost control with token
ceilings; use the analytics `cost` column for reporting/alerting rather than
in-line dollar enforcement (until/unless we run the enterprise controller).

### 5. MCP authz/authn (✅ standalone)

- **`mcpAuthorization`** (`mcp/rbac.rs:12`): CEL `allow/deny/require` over
  `mcp.tool.name`, `mcp.tool.target`, `mcp.tool.arguments`, `jwt.*`,
  `apiKey.*`. Filters `tools/list` **per caller** (denied tools are *hidden*,
  `handler.rs:377`) and re-enforces on `tools/call` (`session.rs:243`).
- **`mcpAuthentication`** (`agent.rs:3017`): OAuth2/OIDC with RFC 9728
  well-known metadata — for safely exposing revenant's *own* MCP endpoint.
- **Token exchange / STS + elicitation** (`proxy/token_exchange.rs`): per-user
  upstream credentials (OBO).

**Design:** scope each agent identity's tool surface with `mcpAuthorization`
CEL keyed on `apiKey.user` — e.g. the coding ninja gets the filesystem MCP but
not the k8s MCP; the owner gets everything; horde peers get a read-only subset.
Enforced in the gateway, hidden from the agent.

## Honest gaps / constraints

1. **No dollar budgets in standalone** — token/request limits only. Per-agent
   $-caps need the enterprise controller (a real deployment decision).
2. **Guardrails fail open** by default; streaming needs opt-in.
3. **Per-agent inbound identity** likely needs the render layer to move from the
   `llm:` shorthand to the `binds/listeners/routes` form (§2 open question).
4. **`virtualModels` ≠ virtual keys** — it's model routing; don't conflate.

## Phased roadmap

- **Phase 0 — analytics on (DONE).** `config.database.url` rendered by default;
  verified against the live binary (endpoint returns summaries, DB auto-created).
- **Phase 1 — consume the telemetry.** Point `revenant status/spend/eval` at the
  request-log store / analytics API; feed cost/latency/model into the
  self-improvement fitness axes.
- **Phase 2 — identity fabric.** Per-agent `apiKey` + `backendAuth`; resolve the
  render-form question; stamp `metadata.{user,group,tier}`.
- **Phase 3 — guardrails.** PII masking default-on; MCP guardrails; optional
  webhook jailbreak/moderation.
- **Phase 4 — scoping.** Per-agent token limits + `mcpAuthorization` tool
  scoping keyed on identity.

Phases 2–4 all *depend on* Phase 1's identity plumbing landing cleanly, which is
why the sequence matters.
