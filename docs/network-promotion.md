# Network promotion — how a proven improvement reaches the whole horde

Status: **design + Phase 1 (classifier) landing dark.** Nothing here flips an
autonomy switch until the machinery is dogfooded across real peers.

## The idea

An improvement one revenant discovers should be able to make *every* revenant
better — but only if it's genuinely good and genuinely safe. Two tiers:

- **Minor** (pure performance/quality win, no new capability, wards untouched):
  the **network** vouches for it and it auto-promotes.
- **Major** (new capability, user-visible behavior, anything near the safety
  surface): the **owner** decides. Always.

This rides on machinery that already exists in `revenant-ascension` +
Necropolis: eval **proof**, two adversarial **reviewer** gates, the
**materiality** judge (generalizable + horde-worthy), signed **molts**, and the
network **Ledger**. What's missing — and what this adds — is a **peer
consensus** gate to stand in for the human-merge step on minor changes.

## Decisions (locked)

**Quorum = reproduction + reputation-weighted vote (hybrid).**
"The network approves" does **not** mean a bare vote — that's cheap talk and
sybil-farmable (one actor, three revenants, self-approve). It means:

1. **Independent reproduction.** ≥R distinct trusted peers each fetch the
   candidate molt (diff + eval spec + baseline), re-run the eval suite *in
   isolation on their own box*, and sign an attestation
   `{molt_id, reproduced: bool, scores, peer_id, sig}`. Reproduction costs real
   compute, so it resists sybil and catches overfit-to-the-author's-seed.
2. **Reputation-weighted vote** on top: weight by peer identity age + landed-molt
   history. Promote iff `reproductions ≥ R` **and** `weighted_approval ≥ T`.

**Classification = auto-classify, wards force major.**
The authoring agent proposes minor/major; the skeptical reviewer gate
cross-checks the label; and — regardless of the label or how good the eval
delta looks — **any change touching a warded path or the sensitive surface is
forced to MAJOR**. This is the load-bearing rule, because **evals do not catch
safety/alignment regressions**: a diff can lift the composite while quietly
loosening approval-gating. The wards fence is not advisory.

Sensitive surface (forces major even outside the crate-level denylist):
permission tiers, approval logic, tool `risk()`, key/secret handling, network
trust, the gateway "first law," `unsafe`, new process/network egress.

## Guardrails beyond the quorum

- **Staged cohort rollout.** A promoted molt lands on a small canary cohort
  first (the network analogue of this machine's canary CD), is watched, then
  reaches the horde. Never all-at-once.
- **Revocable molts.** A signed kill-list the network honors, so a
  passed-but-bad molt can be pulled network-wide.
- **Provenance chain.** Every molt carries its proof + the signed reproduction
  attestations; a peer verifies the chain before applying.

## How this relates to the behavioral self-review

The per-agent **operating notes** (local, unproven, natural-language lessons)
are deliberately **not** networkable — propagating unproven advice poisons a
horde. The bridge: when a local lesson looks like it generalizes, it graduates
into an Ascension **candidate** (a prompt/skill/code change) and must earn
promotion through proof → reproduction → quorum like anything else. Lessons are
hypotheses; molts are proven.

## Phasing

- **Phase 1 — classifier + wards fence (this change, dark).** Pure, local,
  fully unit-tested `classify()` in `revenant-ascension`. Decides
  minor/major from changed files + diff + verdict, with wards + sensitive-surface
  hard-forcing major. Not yet wired to any promote path — it just classifies.
- **Phase 2 — reproduction protocol.** A peer can fetch a candidate molt, run
  the eval suite in isolation, and produce a signed attestation. Verify locally
  with two daemons before any network wiring.
- **Phase 3 — quorum + reputation.** Peer identity age + landed-molt reputation;
  quorum evaluation (`R` reproductions ∧ weighted vote `T`).
- **Phase 4 — cohort rollout + revocation.** Staged propagation + kill-list.
- **Phase 5 — flip autonomy.** Only after multi-peer dogfooding: minor changes
  that clear proof + quorum auto-promote; major always waits for the owner.

Until Phase 5, everything runs **dark** — computed, logged, and surfaced, but
the final promote stays human-gated.
