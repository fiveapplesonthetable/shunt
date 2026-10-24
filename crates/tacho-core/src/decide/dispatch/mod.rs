//! Work queue selection, area serialization, prerequisites, holds, retirement,
//! run lineage/resumption, PR attachment, rebase handoffs, and `--compare` expansion.
//!
//! Two layers live here:
//! - the typed, event-driven engine ([`engine::decide_dispatch`] and its focused pure modules
//!   [`model`], [`runs`], [`report`], [`holds`], [`select`], [`hands`], [`start`], [`resume`],
//!   [`queue`], [`compare`], [`brief`]); every [`model::Action`] carries a human reason;
//! - the legacy string-based helpers (re-exported at this level) still used by the CLI and the
//!   daemon until wave 2 moves them onto the engine.

pub mod brief;
pub mod compare;
pub mod config;
pub mod engine;
pub mod hands;
pub mod holds;
pub mod model;
pub mod queue;
pub mod report;
pub mod resume;
pub mod runs;
pub mod select;
pub mod start;

mod legacy_queue;
mod legacy_runs;
mod legacy_support;
mod provider;

pub use config::*;
pub use legacy_queue::*;
pub use legacy_runs::*;
pub use legacy_support::*;
pub use provider::*;

#[cfg(test)]
mod tests;
