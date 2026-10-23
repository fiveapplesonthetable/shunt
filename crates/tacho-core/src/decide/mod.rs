//! Pure decision engine for fleet orchestration, verification, landing, and supervision.
//!
//! Every decision in this module is a pure function of plain data structures:
//! no IO, no threads, no system clocks, and no mutable globals.
//! Every decision carries its explicit `because` reason so it can be recorded
//! into the SQLite WAL event log (`$TACHO_HOME/events.sqlite`) and replayed
//! deterministically.
//!
//! Organized by concern:
//! - [`dispatch`]: work queue selection, area serialization, prerequisites, holds,
//!   retirement, run lineage/resumption, PR attachment, rebase handoffs, and `--compare` expansion.
//! - [`verify`]: PR stack ordering, batch planning, prefix bisection, log failure classification
//!   (code vs. infrastructure), flake ledger, single-retry fix state machine, and proof preflight.
//! - [`land`]: ready queue, verified merge trains, combined train PR assembly, generator settling,
//!   and compile-breaker bisection.
//! - [`watch`]: agent & chain progress verdicts (`MOVING`, `STALLED`, `HUNG`), phase notes,
//!   supervision actions, CPU starvation planning, machine janitor (`tidy`), watchdog alerts,
//!   cost accounting, and human attention items.

pub mod dispatch;
pub mod land;
pub mod verify;
pub mod watch;

#[cfg(test)]
mod tests;
