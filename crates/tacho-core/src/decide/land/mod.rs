//! Ready queue, verified merge trains, combined train PR assembly, conflict settlement,
//! compile-breaker bisection, two-phase release mailer, post-landing settlement, rebase
//! hand-offs, and screenshot pixel tolerance.
//!
//! Two layers live here:
//! - the typed, event-driven Wave 1 modules ([`config`], [`ready`], [`train`], [`assemble`],
//!   [`release`], [`landed`], [`rebase`], [`shots`], [`decide`]), where every [`decide::LandAction`]
//!   carries a human-readable `because` reason;
//! - the legacy helpers in [`legacy`] (re-exported at this level) used by `decide/mod.rs`,
//!   `tacho-cli`, and `tacho-daemon` until Wave 2 migrates the hands.

pub mod assemble;
pub mod config;
pub mod decide;
pub mod landed;
pub mod legacy;
pub mod ready;
pub mod rebase;
pub mod release;
pub mod shots;
pub mod train;

pub use legacy::*;

// Re-export Wave 1 types and non-colliding functions at `decide::land::*`.
pub use assemble::{
    body, compile_error_excerpt, dropped_line, fix_brief, green_at_head, left_out, left_out_reason,
    next_branch, nothing_to_land_reason, order, rebase_brief, red_fix_tasks, resolution_lines,
    shot_screens, source_heads, title, train_tasks, truncate_max, BatchSummary, BodyParts,
    Candidate, LandTask, MainCi, OpenPr, Settlement, CI_NOTICE,
};
pub use config::LandConfig;
pub use decide::{
    decide_land, LandAction, LandActionKind, LandDecision, LandEvent, LandView, TrainFailure,
};
pub use landed::{
    abandoned, in_flight_members, landed_comment, landed_in, landed_members, main_moved_news,
    merged_prs_from_subjects, newly_landed, resolve_standing, retire_rebases, standing,
    train_abandoned_news, train_landed_news, RetiredRebase, Standing,
};
pub use ready::{
    checked_areas, covered, drafts_to_ready, is_conflicting, land_command, merged_pr_of,
    merged_since, queue_issue_body, ready_queue, sibling_rebases, split_siblings,
    without_train_prs, Conflicted, Passed, ReadyPr, SiblingRebase,
};
pub use rebase::{
    barred, classify_replay_finish, frozen_train_member, hands_task, in_open_train,
    keep_in_shape_read, next_hands, resolve_cherry_pick_conflict, should_replay, BarInputs, BarPr,
    Hands, HandsAgent, ReplayOutcome, ReplayRecord, ShapePr,
};
pub use release::{
    check_areas, letter, merge_command, redraw_areas, required_areas, status_row, Notice,
    ProvedBatch, StatusRow, TrainBatchSummary, Waiting,
};
pub use shots::{
    android_redraw_command, canvas_line, classify_moved_goldens, decode_png, encode_rgba_png,
    format_redraw_failure, golden_names, moved, moved_summary, pixel_moved, read_report_goldens,
    refresh_commit_subject, share_changed, shot_stem, shots_fix_lines, shots_fix_verdict, size_of,
    stale_shots_step, stray_refusal, stray_shots, DecodedPng, MovedGolden, ShotFixKind,
    ShotFixVerdict, ShotRules, StaleShotsStep, DEFAULT_PIXEL_EDGE, DEFAULT_PIXEL_SHARE_PPM,
    GOLDEN_MOVED_MARKER, MOST_CATALOGUE_SHARE, PNG_SIGNATURE,
};
pub use train::{
    did_not_land_subject, landed_line, open_for_id, refusal, reuse_or_make, stopped_loud_line,
    stopped_why, train_id, MailState, Member, Refusal, Sha, Train, TrainId, TrainKind, TrainStatus,
};

#[cfg(test)]
mod tests;
