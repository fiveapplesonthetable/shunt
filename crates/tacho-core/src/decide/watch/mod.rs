//! Pure `watch` concern (Wave 1):
//! - [`sample`]: Process-tree CPU sampling (`Sample`, `take_sample`) — R1–R6
//! - [`activity`]: Worktree activity observation (`Activity`, `compute_activity`, `has_took_line`) — R7–R12
//! - [`verdict`]: Progress verdicts and threshold deadlines (`Verdict`, `last_work`, `verdict`, `agent_next_deadline`) — R13–R17
//! - [`supervise`] & [`tick`]: Supervision steps, pure `decide_watch`, idle chains, and owner mail (`Step`, `WatchAction`, `WatchState`, `WatchObs`, `WatchPlan`, `decide_watch`, `next_deadline`) — R18–R26, R39–R48
//! - [`notes`]: Phase notes (`Phase`, `Note`, `NoteAt`, `Current`, `parse_note`, `with_note`, `current_phase`, `phase_is_long`) — R27–R30
//! - [`chains`]: Agent & area chain rows, lineage helpers, health line, and progress table (`AgentRow`, `ChainRow`, `agent_row`, `rows`, `chain_rows`, `health_line`, `hourly_health_line`, `table`) — R31–R38
//! - [`procs`]: Process table, burner selection, ownership, and duplicate batches (`Proc`, `ProcId`, `below`, `tree_cpu`, `is_burner`, `burners`, `owner`, `duplicate_batches`, `in_agent_cgroup`, `owns`) — R5, R21, R39–R41, R46, R52–R53
//! - [`jobs`]: Named worktree jobs (`valid_job_name`, `job_start_check`, `wait_for`, `job_wait`, `kill_group`, `job_kill`) — R49–R53
//! - [`starve`]: Bounded CPU starvation planner & runner (`StarvePlan`, `plan_starve`, `run_starve`, `bounded_call`, `should_stop_burner`) — R54–R59
//! - [`guard`]: PreToolUse guard hooks (`burner_refusal`, `background_refusal`) — R60–R61
//! - [`budget`]: Machine cgroup v2 CPU/Docker budgets, slices, host telemetry, and container job caps (`Budget`, `Meter`, `busy`, `SliceKind`, `scoped_argv`, `agent_unit_name`, `docker_flags`, `container_flags`, `parse_reading`, `parse_ci_cpus`, `parse_android_caps`, `container_job_argv`) — R62–R68
//! - [`diag`]: Kernel waits, `/proc/locks` lock holders, log scrubbing, and incremental transcript pricing (`Waits`, `waits`, `scrub`, `tail`, `lock_waits`, `lock_holders`, `project_slug`, `classify_transcript`, `consume_transcript_bytes`, `price_transcript`, `summarize_transcripts`) — R69–R71
//! - [`config`]: Configurable thresholds and project parameters (`WatchConfig`, `StarveConfig`)

pub mod activity;
pub mod budget;
pub mod chains;
pub mod config;
pub mod diag;
pub mod guard;
pub mod jobs;
pub mod legacy;
pub mod legacy_notify;
pub mod legacy_ops;
pub mod notes;
pub mod procs;
pub mod sample;
pub mod starve;
pub mod supervise;
pub mod tick;
pub mod verdict;

// Re-export legacy items so `decide/mod.rs`, `decide/tests.rs`, `tacho-cli`, and `tacho-daemon`
// continue compiling without changes during the Wave 1 -> Wave 2 transition.
pub use legacy::*;

// Re-export non-colliding clean Wave 1 APIs at `watch::*` (colliding names like `Verdict`,
// `StarvePlan`, `take_sample`, `verdict`, `parse_note`, `with_note`, `current_phase`,
// `phase_is_long`, `agent_row`, `chain_rows`, `below`, `tree_cpu`, `owner`, `burner_command`,
// `burners`, `duplicate_batches`, `idle_chains`, `plan_starve` are accessed via their submodules).
pub use activity::{compute_activity, has_took_line, Activity, RUN_LOG_TAIL_BYTES};
pub use budget::{
    agent_unit_name, busy, configured_owner_quota, container_flags, container_job_argv,
    cpu_limit_setting, docker_flags, find_budget, local_root_daemon, parse_android_caps,
    parse_ci_cpus, parse_cpu_max, parse_cpus, parse_reading, parse_usage, percent, scoped_argv,
    stop_unit_argv, Budget, DiskReading, HostReading, Meter, SliceKind,
};
pub use chains::{
    format_span, health_line, hourly_health_line, rows, table, AgentRow, ChainBatch, ChainRow,
    ChainTodoItem, PrHow, PrRef, PrStanding, RunRecord, RunStatus,
};
pub use config::{StarveConfig, WatchConfig};
pub use diag::{
    classify_transcript, codex_runs, consume_transcript_bytes, lock_holders, lock_waits,
    model_rates, once_records, parse_transcript_line, price_transcript, proc_lock_holders,
    project_slug, redact_text, scrub, summarize_transcripts, tail, waits, TokenUsage, TopKeySpend,
    TranscriptCacheEntry, TranscriptCostSummary, TranscriptPlace, TranscriptRecord, Waits,
    DAY_SECS, DEFAULT_LOCK_HOLDERS, HOUR_SECS, LINE_CHARS, LOCK_FILES, REDACT_MARK, TAIL_LINES,
    TRANSCRIPT_KEEP_SECS, WEEK_SECS,
};
pub use guard::{background_refusal, burner_refusal, shell_tokens, stage_burner_word};
pub use jobs::{
    job_kill, job_name_problem, job_start_check, job_wait, kill_group, last_log_line,
    tail_non_empty, valid_job_name, wait_for, MAX_JOB_NAME_LEN, WAIT_BEAT_SECS, WAIT_LIMIT_SECS,
};
pub use notes::{Current, Note, NoteAt, Phase};
pub use procs::{
    group_is_alive, identity, in_agent_cgroup, inside, is_burner, leads_group, next_burner_due,
    ours_because, parse_proc_stat, stat_is_alive, timeout_command, BatchObs, Proc, ProcId,
};
pub use sample::Sample;
pub use starve::{
    bounded_call, count_cpus, format_cpu_ids, run_starve, should_stop_burner, valid_cpu_list,
    within_affinity, StarveCallError,
};
pub use supervise::{
    burner_notice, decide_owner_mail, decide_watch, due, next_deadline, nudged_before,
    resume_brief, resume_ready_agents, stalled_sentence, step, Idle, Step, WatchAction, WatchObs,
    WatchPlan, WatchState,
};
pub use verdict::{agent_next_deadline, last_work};

#[cfg(test)]
mod tests;
