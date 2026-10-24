//! Parity test suite (cases 1–87 from `watch.md`), unit tests for guard/budget/diag/jobs,
//! and `proptest` property tests for `watch` invariants.
#![allow(clippy::field_reassign_with_default)]

mod budget_diag_props;
mod progress;
mod starve_jobs;
mod supervise_hourly;
mod supervise_stalls;
