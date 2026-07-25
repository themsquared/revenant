# Fleet memory — shared recall across one owner's agents

## The short answer

Yes, and most of the substrate already exists. This is not a new subsystem; it is
the existing *memory* layer replicated over the existing *ledger*, *account* and
*A2A* machinery. The hard problem is not plumbing — it is poisoning.

## What already exists (and should not be rebuilt)

| need | already built |
|---|---|
| shared, ordered, durable log | Necropolis ledger — hash-linked, replayable, signed |
| "my fleet" as a first-class scope | `agent_bindings` pubkey→account, `Dir::acct()`, kin checks |
| private account-scoped reads | SEC-3 signed read proofs (built for the horde board) |
| authenticated agent transport | SEC-2 signed A2A envelopes + SEC-4 identity-pinned mTLS |
| per-type signature safety | SEC-5a domain-separated preimages |
| conflict-tolerant fact model | bi-temporal memory: invalidate, never delete |

That last row is the lucky one. The memory layer is **already append-only with
bi-temporal invalidation** — a superseded fact is not deleted, it gets
`invalid_at` and a successor carries `valid_from`. That is exactly how you resolve
conflicting writes on a shared append-only log: order by `valid_from`, keep the
whole history, let readers ask "what was true then?". The design that makes local
memory honest is the same design that makes distributed memory convergent.

And because markdown is the source of truth with a **rebuildable** SQLite index, a
fact arriving from another agent materialises as a note and the index rebuilds. No
schema migration is needed to accept foreign facts.

## Architecture

**Ledger for truth, A2A for speed.**

```
agent A consolidates ──> signed MemoryFact record ──> ledger (account-scoped)
                                                        │
                              ┌─────────────────────────┼─────────────────────────┐
                              ▼                         ▼                         ▼
                          agent B replay            agent C replay            agent D replay
                              │                         │                         │
                       materialise note          materialise note          materialise note
                       reindex (local)           reindex (local)           reindex (local)

        …and in parallel: A pushes the same record over A2A to live kin,
        so convergence is seconds, not a sync tick. The ledger remains the
        authority; gossip is only an accelerator and never the sole path.
```

Why this split: the ledger gives total order, durability and offline
verifiability, but it is a poll. A2A gives latency but no ordering guarantee.
Using gossip as an *accelerator over an authoritative log* means a dropped or
duplicated gossip message costs nothing — the next replay is still correct.

### The record

`MemoryFact` becomes a first-class signed, content-addressed, domain-tagged
(`rev-memory-fact-v1`) protocol record in `revenant-net`:

```
MemoryFact {
  id,               // sha256 of the domain-tagged preimage
  agent,            // WHO asserted it — the new, load-bearing field
  account,          // scope
  subject,          // entity uid
  predicate, object,
  text,             // the human-readable fact line
  valid_from,
  invalidates: Option<id>,   // supersession, not deletion
  confidence,       // see "trust" below
  visibility,       // private | account
  created_ts, sig,
}
```

The field that does not exist today and must: **`agent`**. Local provenance is
currently "which session/message" (`<!-- f:uid from:date msg:… -->`). Fleet
provenance has to be *which agent asserted this, signed* — otherwise a fact is
unattributable and every trust decision below is impossible.

## The actual hard problem: poisoning

Shared memory is a shared attack surface. One agent reads a hostile document,
extracts a fact, publishes it — and now every agent in the fleet believes it. This
is strictly worse than the single-agent case, because the blast radius is the whole
fleet and the provenance is one hop removed from the source.

Mitigations, in order of value:

1. **Foreign facts do not enter the local confidence tier.** A fact I derived
   myself and a fact another agent asserts are not the same kind of thing, and the
   retrieval layer should not pretend they are. Rank locally-derived facts above
   replicated ones on ties.
2. **Attribution is mandatory and surfaced.** A recalled fact carries which agent
   asserted it, so an answer built on foreign memory can say so.
3. **Corroboration for high-impact facts.** A fact that changes behaviour
   (credentials, endpoints, "X is safe to run") requires assertion by two agents,
   or the owner. One compromised agent should not be able to move the fleet alone.
4. **Reputation weights the merge.** The reputation system already exists; a
   low-reputation or newly-joined agent's facts should be quarantined rather than
   merged.
5. **Never replicate secrets or machine specifics.** `visibility: private` is the
   default; paths, ports, tokens and owner-profile facts stay local. This is a
   *deny*-by-default classification, not an allow-list of exclusions.

An honest limitation to state up front: none of this defends against the *owner's
own agent* being wrong in a plausible way. It bounds malice and mistakes, it does
not eliminate them, and a fleet that shares memory will occasionally share an
error faster than a single agent would have.

## What this buys

- **Consolidation happens once for the fleet, not once per agent.** Extraction is
  the expensive part of memory (one LLM call per batch); N agents currently pay it
  N times for the same conversation. This is the clearest cost win.
- **A new agent starts informed.** Replay gives it the fleet's memory rather than
  an empty vault, which is the difference between provisioning an agent and
  training one.
- **Cross-machine continuity.** Work started on one box is recallable on another
  without the owner ferrying context.

## Build order

**M1 — the record.** `MemoryFact` in `revenant-net`: type, domain-tagged preimage,
create/verify. Tests: tamper detection, supersession chains, and that a fact
cannot be re-attributed to a different agent.

**M2 — publish.** The consolidator emits `MemoryFact` records for
`visibility: account` facts. Requires the visibility classifier first — publishing
before classification is how secrets leak.

**M3 — ingest.** Ledger replay materialises foreign facts as notes with attribution
and the foreign confidence tier; reindex picks them up unchanged. Retrieval ranks
local above foreign on ties.

**M4 — accelerate.** Push new records to live kin over A2A. Pure latency
optimisation; correctness must not depend on it (test: with gossip disabled the
fleet still converges via replay).

**M5 — trust.** Corroboration requirement for high-impact predicates, reputation
weighting, quarantine for new agents.

## Open questions worth deciding before M2

- **Does the ledger want memory volume at all?** Facts are far more numerous than
  quests. If the answer is no, the same design works against an account-scoped
  store with the ledger holding only a periodic digest — but that trades away
  offline verifiability, so decide deliberately rather than discovering it at scale.
- **Who consolidates?** Every agent (duplicated cost, resilient) or an elected one
  (cheap, single point of failure)? The horde board already knows how to lease
  work to one agent of an account, so election is cheap to build.
- **Era markers.** The same problem SEC-5a hit: replay must verify with the scheme
  each record was written under. If fleet memory lands before era markers, it
  inherits that constraint permanently.
