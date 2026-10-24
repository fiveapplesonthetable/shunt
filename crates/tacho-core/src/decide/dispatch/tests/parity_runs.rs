//! Parity cases 28..=59 (`test_runs.py`).

use std::collections::BTreeMap;

use super::*;
use crate::decide::dispatch::model::{RunState, Stuck};
use crate::decide::dispatch::report::{best_pr, pr_problem, unfinished};
use crate::decide::dispatch::resume::{
    needs_resume, resume_brief, stuck_digest_lines, stuck_runs, stuck_sentence,
};
use crate::decide::dispatch::runs::{
    attempt, base_key, latest, run_key, run_state, BACKOFF, RESUME_LIMIT,
};
use crate::decide::dispatch::select::next_work;

#[test]
fn parity_028_empty_log_is_unfinished() {
    let cfg = test_cfg();
    assert!(unfinished("", &cfg));
    assert!(unfinished("   \n\n  ", &cfg));
}

#[test]
fn parity_029_warning_only_is_unfinished() {
    let cfg = test_cfg();
    assert!(unfinished(
        "[claude] Warning: token rate near limit\n",
        &cfg
    ));
}

#[test]
fn parity_030_api_error_is_unfinished() {
    let cfg = test_cfg();
    assert!(unfinished(
        "Working on task\nAPI Error: 529 Overloaded\n",
        &cfg
    ));
}

#[test]
fn parity_031_mid_work_log_is_unfinished() {
    let cfg = test_cfg();
    assert!(unfinished(
        "I left cargo test going in the background.",
        &cfg
    ));
}

#[test]
fn parity_032_final_summary_with_pr_is_finished() {
    let cfg = test_cfg();
    assert!(!unfinished(
        "Done.\nPR #830 opens the draft with the fix.",
        &cfg
    ));
}

#[test]
fn parity_033_stated_no_pr_is_finished() {
    let cfg = test_cfg();
    assert!(!unfinished("Nothing needed changing.\nPR: none\n", &cfg));
}

#[test]
fn parity_034_summary_with_cleanup_note_is_finished() {
    let cfg = test_cfg();
    let log = "Opened PR #859 with the migration.\n[fleet] killed 1 stray process";
    assert!(!unfinished(log, &cfg));
}

#[test]
fn parity_035_pr_mentioned_only_far_from_end_is_unfinished() {
    let cfg = test_cfg();
    let mut lines = vec!["Looking at PR #1 first".to_string()];
    for i in 0..35 {
        lines.push(format!("still investigating step {i}"));
    }
    assert!(unfinished(&lines.join("\n"), &cfg));
}

#[test]
fn parity_036_lineage_keys() {
    assert_eq!(base_key("a-b~3"), "a-b");
    assert_eq!(attempt("a-b~3"), 3);
    assert_eq!(base_key("a-b"), "a-b");
    assert_eq!(attempt("a-b"), 1);
    assert_eq!(run_key("a-b", 1), "a-b");
    assert_eq!(run_key("a-b", 3), "a-b~3");
}

#[test]
fn parity_037_lineage_order_stops_at_gap() {
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t"));
    runs.insert("a~2".to_string(), make_run(RunStatus::Done, "t"));
    runs.insert("a~4".to_string(), make_run(RunStatus::Done, "t"));
    let (k, _) = latest("a", &runs).unwrap();
    assert_eq!(k, "a~2");
}

#[test]
fn parity_038_run_state_queued_when_not_started() {
    let cfg = test_cfg();
    let runs = BTreeMap::new();
    assert_eq!(run_state("a", &runs, &cfg), RunState::Queued);
}

#[test]
fn parity_039_run_state_resuming_while_retries_remain() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t"));
    assert_eq!(run_state("a", &runs, &cfg), RunState::Resuming);
}

#[test]
fn parity_040_run_state_stuck_when_retries_exhausted() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t"));
    runs.insert("a~2".to_string(), make_run(RunStatus::Unfinished, "t"));
    assert_eq!(run_state("a", &runs, &cfg), RunState::Stuck);
}

#[test]
fn parity_041_resumed_run_completes_base_key_for_dependents() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t-a"));
    let mut a2 = make_run(RunStatus::Done, "t-a");
    a2.pr = PrRecord::NoPr { stated: false };
    runs.insert("a~2".to_string(), a2);
    let mut b = make_todo("b", "t-b", "");
    b.after = vec!["a".to_string()];
    let todo = vec![b];
    let picked = next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .expect("b should be picked");
    assert_eq!(picked.item.key, "b");
}

#[test]
fn parity_042_needs_resume_waits_for_backoff() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Unfinished, "t");
    r.finished = Some(1000);
    runs.insert("a".to_string(), r.clone());
    assert!(!needs_resume("a", &r, &runs, 1000 + 119, &cfg));
    assert!(needs_resume("a", &r, &runs, 1000 + 120, &cfg));
}

#[test]
fn parity_043_resume_limit_is_one_retry() {
    assert_eq!(RESUME_LIMIT, 1);
    assert_eq!(BACKOFF, [120]);
}

#[test]
fn parity_044_needs_resume_skips_once_next_run_exists() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Unfinished, "t");
    r.finished = Some(1000);
    runs.insert("a".to_string(), r.clone());
    runs.insert("a~2".to_string(), make_run(RunStatus::Running, "t"));
    assert!(!needs_resume("a", &r, &runs, 2000, &cfg));
}

#[test]
fn parity_045_needs_resume_stops_at_limit() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Unfinished, "t");
    r1.finished = Some(1000);
    let mut r2 = make_run(RunStatus::Unfinished, "t");
    r2.finished = Some(1200);
    runs.insert("a".to_string(), r1.clone());
    assert!(needs_resume("a", &r1, &runs, 1200, &cfg));
    runs.insert("a~2".to_string(), r2.clone());
    assert!(!needs_resume("a~2", &r2, &runs, 2000, &cfg));
}

#[test]
fn parity_046_nudged_run_resumes_without_waiting_for_backoff() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Unfinished, "t");
    r.finished = Some(1000);
    r.nudge = true;
    runs.insert("a".to_string(), r.clone());
    assert!(needs_resume("a", &r, &runs, 1000, &cfg));
}

#[test]
fn parity_047_done_run_never_resumes() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Done, "t");
    r.finished = Some(1000);
    runs.insert("a".to_string(), r.clone());
    assert!(!needs_resume("a", &r, &runs, 5000, &cfg));
}

#[test]
fn parity_048_resume_brief_includes_worktree_state_and_task() {
    let cfg = test_cfg();
    let where_ = Stopped {
        path: "/w/app".to_string(),
        commits: vec!["abc1234 add widget".to_string()],
        changes: vec![" M src/lib.rs".to_string(), "?? notes.txt".to_string()],
    };
    let text = resume_brief("implement the widget", Some(&where_), false, &cfg);
    assert!(text.contains("/w/app"));
    assert!(text.contains("abc1234 add widget"));
    assert!(text.contains(" M src/lib.rs"));
    assert!(text.contains("?? notes.txt"));
    assert!(text.contains("implement the widget"));
    assert!(text.contains("do not start over"));
}

#[test]
fn parity_049_resume_brief_truncates_huge_worktree_state() {
    let cfg = test_cfg();
    let where_ = Stopped {
        path: "/w/app".to_string(),
        commits: (0..40).map(|i| format!("c{i}")).collect(),
        changes: (0..36).map(|i| format!(" M f{i}.rs")).collect(),
    };
    let text = resume_brief("task", Some(&where_), false, &cfg);
    assert!(text.contains("and 30 more"));
    assert!(text.contains("and 6 more"));
}

#[test]
fn parity_050_nudge_brief_explains_quiet_stop() {
    let cfg = test_cfg();
    let text = resume_brief("task", None, true, &cfg);
    assert!(text.contains("what is blocking"));
}

#[test]
fn parity_051_stuck_runs_finds_exhausted_lineage() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Unfinished, "t");
    r1.worktree = "/w/a".to_string();
    let mut r2 = make_run(RunStatus::Unfinished, "t");
    r2.finished = Some(2);
    r2.summary = "one\n\ntwo\n".to_string();
    runs.insert("a".to_string(), r1);
    runs.insert("a~2".to_string(), r2);
    let stuck = stuck_runs(&runs, 100, &cfg);
    assert_eq!(
        stuck,
        vec![Stuck {
            key: "a".to_string(),
            runs: 2,
            since: 2,
            tail: vec!["one".to_string(), "two".to_string()],
            worktree: "/w/a".to_string(),
        }]
    );
}

#[test]
fn parity_052_stuck_runs_ignores_resumable_run() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t"));
    assert!(stuck_runs(&runs, 100, &cfg).is_empty());
}

#[test]
fn parity_053_stuck_runs_ignores_done_lineage() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Unfinished, "t"));
    runs.insert("a~2".to_string(), make_run(RunStatus::Done, "t"));
    assert!(stuck_runs(&runs, 100, &cfg).is_empty());
}

#[test]
fn parity_054_stuck_sentence_says_what_is_needed() {
    let cfg = test_cfg();
    let stuck = Stuck {
        key: "a".to_string(),
        runs: 2,
        since: 100,
        tail: vec!["one".to_string(), "two".to_string()],
        worktree: "/w/a".to_string(),
    };
    let where_ = Stopped {
        path: "/w/a".to_string(),
        commits: vec!["c1".to_string()],
        changes: vec![" M f1".to_string()],
    };
    let s = stuck_sentence(&stuck, Some(&where_), &cfg);
    assert!(s.contains("/w/a"));
    assert!(s.contains("two"));
    assert!(s.contains("1 commits and 1 changed files"));
    assert!(s.contains("attach a <pr>"));
    assert!(s.contains("will not be resumed again"));
}

#[test]
fn parity_055_stuck_digest_lines_report_age() {
    let stuck = vec![Stuck {
        key: "a".to_string(),
        runs: 2,
        since: 1000,
        tail: Vec::new(),
        worktree: "/w/a".to_string(),
    }];
    let lines = stuck_digest_lines(&stuck, 1000 + 7 * 3600);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("a has been stuck for 7 h"));
}

#[test]
fn parity_056_attached_pr_beats_later_guess() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Unfinished, "t");
    r1.pr = PrRecord::Found(FoundPr {
        number: 5,
        branch: "b5".to_string(),
        how: PrHow::Attached,
    });
    let mut r2 = make_run(RunStatus::Done, "t");
    r2.pr = PrRecord::Found(FoundPr {
        number: 9,
        branch: "b9".to_string(),
        how: PrHow::Guessed,
    });
    runs.insert("a".to_string(), r1);
    runs.insert("a~2".to_string(), r2);
    assert!(pr_problem("a", &runs, &cfg).is_none());
    assert_eq!(best_pr("a", &runs).unwrap().number, 5);
}

#[test]
fn parity_057_guessed_pr_requires_person() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Done, "t");
    r1.pr = PrRecord::Found(FoundPr {
        number: 9,
        branch: "b9".to_string(),
        how: PrHow::Guessed,
    });
    runs.insert("a".to_string(), r1);
    let (prob, _) = pr_problem("a", &runs, &cfg).expect("problem");
    assert!(prob.contains("guessed"));
}

#[test]
fn parity_058_stated_no_pr_cleared_when_no_change_confirmed() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Done, "t");
    r1.pr = PrRecord::NoPr { stated: true };
    r1.no_change = true;
    runs.insert("a".to_string(), r1);
    assert!(pr_problem("a", &runs, &cfg).is_none());
}

#[test]
fn parity_059_stated_no_pr_with_work_in_worktree_needs_person() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r1 = make_run(RunStatus::Done, "t");
    r1.pr = PrRecord::NoPr { stated: true };
    r1.worktree = "/w/a".to_string();
    runs.insert("a".to_string(), r1);
    let (prob, needs_worktree_check) = pr_problem("a", &runs, &cfg).expect("problem");
    assert!(prob.contains("stated `PR: none`"));
    assert!(needs_worktree_check);
}
