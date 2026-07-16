# SEC-4 — mTLS for agent↔agent transport

**Status:** approved-pending-review · **Depends on:** SEC-1 (signed registration), SEC-2 (signed A2A envelopes), SEC-3 (signed reads)

## What transport security we actually have today

| Path | Confidentiality | Server auth | Client auth |
|---|---|---|---|
| agent → necropolis | TLS (Fly edge) | edge cert | none (signed payloads instead) |
| agent → agent, via gateway egress | TLS only if the remote URL is https, default roots | CA roots, no pinning | none |
| agent → agent, direct | whatever the URL says — plaintext on a LAN | none | none |
| inbound A2A (cross-machine `REVENANT_BIND`) | **plaintext HTTP** | **none** | envelope signature only |

SEC-1..3 give us *authenticity* end-to-end (every message provably from a key).
What's missing is *transport* security between agents: confidentiality on the
wire and mutual auth of the connection itself. That's SEC-4.

## The design fork: per-account CA vs identity-pinned certs

### Option A — per-account CA
A CA per account; per-agent client/server certs minted at `net bind`; agents
verify chains to the account CA.

*Why not (for us):*
1. **It can't cover the open mesh.** SEC-2 deliberately accepts A2A from *any*
   validly-signed sender, trust-scaled by reputation. A stranger's cert can
   never chain to my account CA — so the exact traffic that most needs
   transport auth (unknown peers) would be uncoverable, or fall back to
   unauthenticated TLS.
2. **CA custody is a ceremony.** The CA private key must live somewhere (an
   "anchor" machine), new machines need CSRs shuttled to it, loss of the CA key
   strands the fleet, and rotation is a project. Every step is a foot-gun on a
   personal fleet.
3. **It adds a second root of trust.** We already have exactly one: the agent's
   Ed25519 identity, which earns reputation and signs everything. A CA would be
   a parallel authority that has to be kept consistent with the first.

### Option B — self-signed certs, pinned via the signed identity (chosen)
Each agent mints one persistent self-signed TLS certificate. The certificate's
SHA-256 fingerprint is embedded in the agent's **identity-signed**
`Registration` and `AgentProfile` (heartbeat) — the same records SEC-1 already
authenticates. A peer resolves the fingerprint from the directory and verifies
the presented certificate matches, byte-for-byte.

Trust chain: `Ed25519 identity ──signs──▶ cert fingerprint ──pins──▶ TLS session`

- **Works for the whole mesh**, kin and stranger alike: anyone with a published
  signed profile can be pinned. Trust in *what they may do* still comes from
  SEC-2's reputation gate; the pin only binds the wire to the identity.
- **No ceremony.** No CA key custody, no CSR shuttling, no chain building.
  A new machine mints its cert at first boot and its next signed heartbeat
  publishes the fingerprint.
- **Rotation is one heartbeat.** Mint a new cert, publish the new fingerprint
  in the next signed profile; peers pin the latest signed claim. Revocation is
  supersession.
- **One root of trust**, the one we already defend: the identity key. The
  directory remains untrusted transport for signed claims ("trust the key,
  not the directory").

This is SPIFFE-shaped (identity-bound workload certs) without standing up
SPIRE: the necropolis plays the role of the discovery plane, but every claim
in it is self-signed by the workload's own identity key.

### Defense stack after SEC-4
```
TLS (rustls, pinned certs)      ← confidentiality + connection mutual-auth
  └ signed A2A envelope          ← per-message authenticity + replay guard
      └ reputation gate          ← authorization (what the sender may trigger)
```
The envelope stays mandatory even over mTLS: it survives proxies/gateways and
authenticates the *message*, while the pin authenticates the *connection*.

## Implementation plan

### P1 — cert identity foundation (revenant-net)
- `revenant_net::tls`: mint/load a persistent self-signed cert
  (`~/.revenant/identity/tls.{crt,key}`, ECDSA P-256 via rcgen — universally
  verifiable, unlike Ed25519 certs whose webpki support is spotty),
  `fingerprint()` = SHA-256 of the DER, lowercase hex.
- `AgentProfile.tls_fp: Option<String>` and `Registration.tls_fp:
  Option<String>` — **backward-compatible signing**: the fingerprint is folded
  into the preimage only when present (tagged), so every record already on the
  ledger, and every old client still signing without it, verifies unchanged.
- Heartbeat + `net register` publish the fingerprint automatically.

### P2 — the wire (revenant daemon + tools)
- **Inbound**: an optional TLS A2A listener (`[network] a2a_tls_port`,
  rustls) presenting the agent cert; requests client certs; the handler binds
  the presented client-cert fingerprint to the envelope's `x-rev-agent` by
  checking it against that identity's published fingerprint. Loopback :7717
  stays for local surfaces.
- **Outbound (`call_agent`, direct targets)**: rustls connector with a pinning
  `ServerCertVerifier` — the presented server cert must match the target
  identity's published fingerprint — and presents our client cert.
- **Gateway-egress targets**: remain governed egress with standard TLS; the
  envelope still authenticates end-to-end through the hop. (agentgateway's
  backendTLS pinning is config-simple `{}` today; revisit when it exposes
  cert-pinning knobs.)

### P3 — ergonomics
- `revenant doctor`: verify local cert/fingerprint/published-profile agree.
- Rotation command (`revenant net tls-rotate`): mint + immediate heartbeat.
- Necropolis mTLS is explicitly **out of scope**: Fly terminates edge TLS, and
  every payload is already signed; client-transport auth there adds cost, not
  trust.

## Threats closed / explicitly not closed
- ✅ LAN eavesdropping + tampering of direct A2A (was plaintext).
- ✅ MITM/impersonation of a peer's endpoint (pin binds wire to identity —
  even a spoofed DNS/registration can't produce the pinned cert).
- ✅ Stolen bearer tokens for A2A (already dead after SEC-2; mTLS adds the
  same property one layer down).
- ❌ A fully compromised peer machine (holds both identity key and cert key —
  no transport design fixes key theft; that's reputation + owner hygiene).
- ❌ Necropolis availability attacks (fingerprint resolution caches like
  SEC-2's reputation cache; stale-but-signed pins fail closed to "reject on
  mismatch").
