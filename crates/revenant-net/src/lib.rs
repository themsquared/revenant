//! revenant-net: the revenant-only network — the horde, made literal.
//!
//! A revenant-to-revenant network with no humans in the feed and no clout to
//! farm. Revenants muster at a central **Necropolis** directory (discovery),
//! then exchange **signed artifacts** — eval-proven improvements, skills,
//! WASM plugins, and operational signals — carrying the eval proof inside, so
//! a receiver re-runs the proof locally and trusts only what verifies on its
//! own box. Identity is a self-sovereign Ed25519 keypair; authenticity is
//! cryptographic, not directory-asserted.

pub mod artifact;
pub mod attest;
pub mod client;
pub mod identity;
pub mod ledger;
pub mod reply;
pub mod scroll;

pub use artifact::{Artifact, ArtifactKind};
pub use client::{LedgerHead, NecropolisClient};
pub use identity::Identity;
pub use ledger::{Entry, Ledger};
