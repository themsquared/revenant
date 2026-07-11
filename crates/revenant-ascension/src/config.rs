//! Ascension configuration lives in `revenant-core` (like every other config
//! section) so the harness config can own it without a dependency cycle. This
//! module re-exports it for ergonomic use from the engine.

pub use revenant_core::config::AscensionConfig;
