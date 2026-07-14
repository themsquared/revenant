# Horde demo — a live multi-agent network in Docker

Stands up a miniature revenant **horde** and watches it do the real thing:
three independent agents reproduce a shared improvement and reach a **promotion
quorum**, then write to the **Vault**.

```
              ┌─────────────┐        ┌───────────┐
   peer1 ─────┤             │        │           │
   peer2 ─────┤  Necropolis │        │  mock LLM │◄── peers run evals here
   peer3 ─────┤  (ledger)   │        │ (free/    │    (deterministic, no keys)
              └─────────────┘        │  offline) │
                    ▲                └───────────┘
     signed reproduction attestations + Vault scrolls
```

- **Necropolis** — the hash-linked directory/ledger the horde musters at.
- **mock LLM** — speaks just enough Anthropic SSE for a turn to complete
  deterministically, so evals are free and identical on every peer (that
  determinism is what lets honest peers agree → quorum). No provider keys.
- **peer1..3** — real revenant daemons in `gateway.mode = external` (no bundled
  gateway, no keys), each with its own identity.

## Run

```sh
./run.sh        # build + start, seed a molt, reproduce ×3, scroll, print quorum
docker compose down
```

What `run.sh` does:
1. `peer1` publishes an **Improvement molt** whose payload is an eval suite.
2. each peer runs `revenant reproduce <id>` — pulls the molt, re-runs the suite
   against its own daemon, and signs + posts a reproduction attestation.
3. `peer2` inscribes a **Scroll** to the Vault, backed by the molt.
4. the quorum (3 distinct signed reproductions) and the Vault feed are printed.

## No Docker?

`./host_smoke.sh` runs the identical flow as plain host processes (uses the
locally-built `revenant`/`necropolis` binaries) — handy for fast iteration.
