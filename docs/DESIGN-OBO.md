# SEC-5 — On-behalf-of: scoped, attenuable delegation

## What authorization we actually have today

Every network record is signed by an *agent* key (Ed25519, pubkey = agent id), and
SEC-1..4 prove that well: signed registration, signed A2A envelopes with freshness
+ replay nonces, signed read proofs, and identity-pinned mTLS. SEC-5a then
domain-separated all of it, so a signature for one record type can no longer be
replayed as another.

What none of that expresses is **authority**. Concretely:

- **No request ever says "acting FOR account X."** Every record says "agent Y
  signed this," and the server *derives* the account (`Dir::acct(pubkey)` →
  `agent_bindings`, `revenant-necropolis/src/accounts.rs`). Fine for attributing
  an action; useless for constraining one.
- **Authorization is identity + membership + reputation, full stop.** `a2a_tier`
  (`revenant-control/src/lib.rs`) resolves `Full` if the peer is in
  `network.a2a_trusted`, or is *kin* (same account), or has reputation ≥ 10.0;
  `Rejected` below 0; otherwise `Limited`. Nothing narrower is expressible.
- **`capabilities` / `sigils` are self-asserted hints, never grants.** An agent
  advertises them in its own signed Registration/Profile. Nothing enforces them.
- **The one account-asserting call uses a raw, unscoped, non-expiring secret.**
  `GET /account/agents` authenticates with the account key in a querystring (or a
  bearer session). That secret carries the whole account's authority forever.
- **The only TTL'd grants in the tree are process-local and unrelated to the
  network**: the approval broker's `GRANT_TTL_SECS = 3600` keyed by
  `(session_id, kind)`, and horde claim leases (`CLAIM_LEASE_SECS = 1800`).

So today "kin" is binary and total. Agent A of your account can do anything agent
B can, because the only question asked is *which account are you in*. That is the
wrong question once agents delegate work to each other, and it is the thing
standing between the horde and doing real work safely.

## What OBO has to express

A grant, signed by the authority being delegated, that says:

> *subject* may act for *account*, limited to *scopes*, until *expiry*, and may
> pass on **no more** than that.

Four properties, in priority order:

1. **Least privilege** — a delegated caller gets a subset, never `Full` by
   inheritance.
2. **Attenuation** — a holder can re-delegate only *narrower*. Monotonic
   decrease is the invariant that makes chains safe.
3. **Expiry** — bounded lifetime, so a leaked grant stops mattering.
4. **Auditability** — the chain is inspectable after the fact, and persisted with
   the action it authorized.

## The design fork: bearer tokens vs signed capability chains

### Option A — server-minted bearer tokens (rejected)

Necropolis issues an opaque token per delegation; verifiers call the directory to
introspect it. Familiar (OAuth-shaped), and revocation is trivial — delete the row.

Rejected because it inverts the property the whole protocol is built on: a
receiver would have to **trust the directory** to tell it what a caller may do.
Today a receiver verifies a key and needs the directory for nothing. It also puts
Necropolis on the critical path of every A2A call (it currently is not — and
`a2a_tier` already fails *closed* to `Limited` when the directory is unreachable,
which would become "nothing works").

### Option B — owner-signed capability chain, verified offline (chosen)

A `Delegation` is a first-class signed, content-addressed record — same shape as
everything else in `revenant-net`, domain-tagged from birth (`rev-delegation-v1`):

```
Delegation {
  id,                 // sha256 of the domain-tagged preimage
  issuer,             // account key, or the subject of the parent link
  subject,            // agent pubkey this grant empowers
  account,            // whose authority is being lent
  audience,           // Option<agent pubkey> — who may accept it (None = any)
  scopes: Vec<Scope>, // capability set, monotonically narrowing down the chain
  not_before,
  expires_at,
  nonce,
  parent: Option<id>, // previous link; None = root, signed by the account key
  sig,
}
```

Verification is pure and local: walk the chain to the root, check each link's
signature and time window, check the root issuer is the account key, and check
**each child's scopes ⊆ its parent's**. No network call, no directory trust — the
same posture as SEC-1..4.

Costs, accepted deliberately:

- **Revocation is not free.** Signed grants are valid until they expire. Mitigate
  with short TTLs (minutes-to-hours, not days) plus a *deny list* of revoked ids
  the directory publishes as a hint. The security floor stays "it expires."
- **Chains must be bounded.** Cap depth (≤4) and total serialized size, verified
  before any signature work, so chain verification can't be a DoS vector — same
  discipline as the bounded A2A caches.

## Scope model

Coarse and enumerated, not free-form strings. A closed enum is greppable,
diffable, and cannot silently widen when someone adds a feature:

```
Scope::HordeRead              // poll the private board
Scope::HordeClaim             // claim a subtask
Scope::HordeSubmit            // submit a result
Scope::A2AMessage             // send an A2A message as the account
Scope::VaultPublish           // publish a scroll
Scope::QuestRead
// ...added deliberately, never inferred
```

Explicitly **not** in v1: spend authority, key rotation, identity binding, or
anything that can mint further authority outside the chain. Those stay owner-only.

## Where it must be enforced

Two sinks, and both matter:

**Agent side** (`revenant-control/src/lib.rs`)
- Inside `a2a_tier`, not beside it. If OBO is checked *after* tier resolution, a
  delegated caller inherits `Full` from kin/reputation and the scope is
  decoration. The grant must *replace* the tier decision for delegated calls,
  producing a scope set rather than a tier.
- `Limited` and `Full` are the two capability sinks (`run_turn` is reached from
  `Full`); a delegated call lands in neither by default.
- The agent card should advertise OBO support so callers know to present a grant.

**Necropolis side** (`revenant-necropolis/src/server.rs`) — everywhere
`acct()`/`require_account()` is currently the *sole* authorization: `publish_horde_task`,
`publish_horde_claim`, `publish_horde_result`, `verify_read_proof` (+ its callers
`horde_tasks`/`horde_run`), and the `require_account` write gates (artifacts,
attest/verify, scrolls, replies, votes, handles, profiles, quests, boost).
`account_agents` is the one to *replace* — the raw-account-key path is exactly what
OBO exists to kill.

### The ledger constraint

`Dir::apply` re-derives same-account checks from **stored** records when replaying
the ledger. So a grant cannot be a transport-only header checked at the HTTP edge:
the authorizing grant (or its id, resolvable from a persisted grant record) must be
**stored in the ledger entry** it authorized, or replay will reach a different
authorization verdict than the live request did. This is the single most important
implementation constraint, and it is why OBO is a protocol change rather than a
middleware.

## Implementation plan

### P1 — the record (`revenant-net`)
`delegation.rs`: the type, domain-tagged preimage, `create`/`verify`, chain walk
with depth+size caps and the scopes-⊆-parent invariant. Property tests: widening a
child fails; expired link fails; wrong root issuer fails; reordered/spliced chain
fails; depth/size caps reject before signature work.

### P2 — presenting and checking it
Carry the chain on A2A as a header (`x-rev-obo`, base64 JSON) alongside the
existing envelope, and as a field on Necropolis write bodies. Fold the grant id
into the A2A signature preimage so a grant cannot be swapped onto a different
signed request. Rework `a2a_tier` → returns a scope set; `Full`/`Limited` become
the two *undelegated* defaults.

### P3 — Necropolis enforcement + ledger persistence
Enforce per-endpoint scopes; persist the authorizing grant in the ledger entry;
replace the `account_agents` raw-key path. Publish the revocation deny-list.

### P4 — ergonomics
`revenant net delegate <agent> --scope ... --ttl ...`, `net delegations` to list,
`doctor` checks for grants nearing expiry, and orchestrator integration so a horde
run mints exactly the narrow grant its workers need.

## Threats closed / explicitly not closed

**Closed:** a compromised or misbehaving agent of your account can no longer do
*everything* your account can — it can do only what its grant says, for as long as
the grant lives. Re-delegation cannot escalate. A stolen grant is useless after
expiry, useless to a different audience, and (once the id is bound into the
envelope signature) cannot be moved onto another request.

**Not closed:** a compromised *account key* — that is the root of trust, and OBO
does not change that. Instant revocation (bounded by TTL + deny-list latency).
Confused-deputy *within* a granted scope: if you grant `HordeSubmit`, the holder may
submit whatever it likes, so scopes must be granted narrowly and short.

**Prerequisite already landed:** SEC-5a domain separation. Adding a signed record
type to a protocol where preimages carry no type tag would have made the
`Delegation` signature itself confusable with other records — the exact bug proven
in revenant#17.
