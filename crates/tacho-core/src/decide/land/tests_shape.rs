use std::collections::{BTreeMap, BTreeSet};

use super::super::assemble::{
    body, ci_proof_pending, eligible, nothing_to_land_reason, red_fix_tasks, resolution_lines,
    settle, solo_ci, title, BodyParts, MainCi, CI_NOTICE,
};
use super::super::decide::{decide_land, LandActionKind, LandEvent, LandView, TrainFailure};
use super::super::rebase::{
    frozen_train_member, keep_in_shape_read, should_replay, Hands, ReplayRecord, ShapePr,
};
use super::super::release::{status_row, TrainBatchSummary};
use super::super::shots::refresh_commit_subject;
use super::super::train::{Member, Train};
use super::{fixture_cfg, pr, CODE, COMPARISON, SHOT};

#[test]
fn a_recent_registered_train_draft_keeps_its_exact_head_in_the_shape_snapshot() {
    let cfg = fixture_cfg();
    let train_rec = Train::make_combined(
        901,
        "train-2026-10-15-1",
        "train_head_1",
        vec![Member::new(1298, "source_sha")],
        vec![],
        vec![],
        false,
    );
    let trains = vec![train_rec];
    let recent_train_draft = ShapePr {
        number: 901,
        sha: "train_head_1".into(),
        branch: "train-2026-10-15-1".into(),
        draft: true,
        updated_age_secs: 5,
        conflicting: false,
        has_merge_commits: false,
    };
    assert!(keep_in_shape_read(
        &recent_train_draft,
        &BTreeMap::new(),
        &trains,
        cfg.recent_draft_secs
    ));

    let member_pr = ShapePr {
        number: 1298,
        sha: "source_sha".into(),
        branch: "feat-1298".into(),
        draft: false,
        updated_age_secs: 3000,
        conflicting: true,
        has_merge_commits: false,
    };
    let mut open_heads = BTreeMap::from([
        (901, "train_head_1".to_string()),
        (1298, "source_sha".to_string()),
    ]);
    let running_batches = vec![(901, "train_head_1".to_string())];
    assert!(!should_replay(
        &member_pr,
        "main0",
        "main0",
        &trains,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &open_heads,
        &running_batches,
        |_, _| true,
        &cfg,
    ));
    open_heads.insert(901, "train_head_force_pushed".into());
    assert!(should_replay(
        &member_pr,
        "main0",
        "main0",
        &trains,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &open_heads,
        &running_batches,
        |_, _| true,
        &cfg,
    ));
}

#[test]
fn a_running_native_train_batch_holds_its_assembled_source_head() {
    let cfg = fixture_cfg();
    let train_rec = Train::make_combined(
        1299,
        "train-2026-10-15-1",
        "train_head",
        vec![Member::new(1298, "source_sha")],
        vec![],
        vec![],
        false,
    );
    assert_eq!(
        train_rec.source_heads,
        BTreeMap::from([(1298, "source_sha".to_string())])
    );
    let trains = vec![train_rec];
    let open_heads = BTreeMap::from([
        (1299, "train_head".to_string()),
        (1298, "source_sha".to_string()),
    ]);
    let running_batches = vec![(1299, "train_head".to_string())];
    assert!(frozen_train_member(
        1298,
        "source_sha",
        &trains,
        &open_heads,
        &running_batches,
        |_, _| true,
    ));
    let edited = ShapePr {
        number: 1298,
        sha: "independently-edited-head".into(),
        branch: "feat-1298".into(),
        draft: false,
        updated_age_secs: 3000,
        conflicting: true,
        has_merge_commits: false,
    };
    assert!(should_replay(
        &edited,
        "main0",
        "main0",
        &trains,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &open_heads,
        &running_batches,
        |_, _| true,
        &cfg,
    ));
}

#[test]
fn screenshots_that_conflict_keep_the_later_copy_for_the_batch() {
    let cfg = fixture_cfg();
    let s = settle(&[SHOT.to_string()], &cfg);
    let res = resolution_lines(2, &s);
    let pr_body = body(&BodyParts {
        members: &[1, 2],
        resolutions: &res,
        ..Default::default()
    });
    assert!(pr_body.contains("#2: 1 screenshot(s) conflicted"));
    assert_eq!(title(2), "2 open changes land together");
}

#[test]
fn a_train_opens_without_capturing_its_screenshots() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    let opened_body = dec
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::OpenPr { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(opened_body.contains("refreshed by the batch"));
    assert!(dec
        .actions
        .iter()
        .any(|a| matches!(&a.kind, LandActionKind::StartBatch { prs } if prs == &[901])));
}

#[test]
fn the_batch_refresh_commits_screenshots_on_the_train_branch() {
    assert_eq!(
        refresh_commit_subject("train-refresh"),
        "Screenshots for train train-refresh"
    );
}

#[test]
fn a_train_that_follows_a_refusal_clears_it() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.train_refused
        .insert("why".into(), "old capture failure".into());
    let dec = decide_land(
        &view,
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    assert!(dec.next.train_refused.is_empty());
}

#[test]
fn a_generated_file_that_conflicts_is_made_again_on_the_merged_tree() {
    let cfg = fixture_cfg();
    let s = settle(&[COMPARISON.to_string()], &cfg);
    let lines = resolution_lines(2, &s);
    assert_eq!(
        lines,
        vec![(
            2,
            format!(
                "{COMPARISON} conflicted; the later copy was kept and its generator ran on the merged tree"
            )
        )]
    );
}

#[test]
fn a_source_conflict_leaves_the_change_out_and_queues_its_rebase_behind_the_train() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1"), Member::new(3, "s3")],
            resolutions: vec![],
            left_out: vec![(2, vec![CODE.to_string()])],
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    let task = dec.next.todo.iter().find(|t| t.key == "rebase-2").unwrap();
    assert_eq!(task.after, vec!["pr:901"]);
    assert!(task.brief.contains(CODE));
    let opened_body = dec
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::OpenPr { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(opened_body.contains("#2"));
}

#[test]
fn a_change_that_breaks_the_build_is_dropped_and_gets_a_fix_task() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1"), Member::new(3, "s3")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![(2, "error[E0425]: cannot find value `x`".into())],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    let task = dec
        .next
        .todo
        .iter()
        .find(|t| t.key == "fix-compile-2")
        .unwrap();
    assert!(task.brief.contains("E0425"));
}

#[test]
fn nothing_to_land_opens_no_pull_request() {
    let cfg = fixture_cfg();
    assert!(eligible(
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &cfg
    )
    .is_empty());
    assert_eq!(
        nothing_to_land_reason(&[]),
        "nothing to land: every change conflicts"
    );
    assert_eq!(
        nothing_to_land_reason(&[(2, "err".into())]),
        "nothing to land: #2 does not compile"
    );
}

#[test]
fn a_new_train_is_checked_but_neither_mailed_nor_made_ready() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(1, "s1")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![],
            screens: vec![],
            ci: false,
        },
        &cfg,
    );
    assert!(dec
        .actions
        .iter()
        .any(|a| matches!(&a.kind, LandActionKind::StartBatch { prs } if prs == &[901])));
    assert!(!dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::SendMail { .. } | LandActionKind::MarkReady { .. }
    )));
}

#[test]
fn a_pipeline_change_travels_without_the_others_and_the_train_says_so() {
    let cfg = fixture_cfg();
    let chosen = solo_ci(
        vec![pr(1, &[CODE]), pr(2, &[".github/workflows/ci.yaml"])],
        &cfg,
    );
    assert_eq!(chosen.iter().map(|c| c.number).collect::<Vec<_>>(), vec![2]);
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::RecordCombinedTrain {
            pr: 901,
            branch: "train-2026-10-15-1".into(),
            head: "sha901".into(),
            members: vec![Member::new(2, "s2")],
            resolutions: vec![],
            left_out: vec![],
            dropped: vec![],
            screens: vec![],
            ci: true,
        },
        &cfg,
    );
    let rec = dec.next.trains.last().unwrap();
    assert_eq!(rec.numbers(), vec![2]);
    assert!(rec.ci);
    let opened_body = dec
        .actions
        .iter()
        .find_map(|a| match &a.kind {
            LandActionKind::OpenPr { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(opened_body.contains(CI_NOTICE));
}

#[test]
fn no_train_forms_while_the_last_pipeline_change_has_no_passing_main_run() {
    let pending = ci_proof_pending(Some(&"c".repeat(40)), &MainCi::default(), |_a, _b| false);
    assert!(pending.is_some());
}

#[test]
fn a_red_verdict_queues_a_fix_task_for_each_failing_area_with_the_log_lines() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.trains.push(Train::make_combined(
        901,
        "train-2026-10-15-1",
        "sha901",
        vec![Member::new(1, "s1")],
        vec![],
        vec![],
        false,
    ));
    view.failed_trains.insert(
        901,
        TrainFailure {
            sha: "abc".into(),
            areas: vec!["rust".into(), "console".into()],
            why: "failed: rust console".into(),
            log: "panicked at a.rs:3".into(),
        },
    );
    let d1 = decide_land(&view, &LandEvent::QueueRedFixes, &cfg);
    let d2 = decide_land(&d1.next, &LandEvent::QueueRedFixes, &cfg);
    assert!(d2.actions.is_empty());
    let keys: Vec<&str> = d2.next.todo.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys, vec!["train-fix-901-rust", "train-fix-901-console"]);
    assert!(d2.next.todo[0].brief.contains("panicked at a.rs:3"));
    assert!(red_fix_tasks(
        901,
        "abc",
        &["rust".into()],
        "why",
        "log",
        Some("abc"),
        &cfg
    )
    .is_empty());
    let _ = ReplayRecord::default();
    let _ = Hands::default();
}

#[test]
fn status_shows_who_is_in_and_the_batch_verdict() {
    let t = Train::make_combined(
        901,
        "train-2026-10-15-1",
        "sha901",
        vec![Member::new(1, "s1")],
        vec![],
        vec![],
        false,
    );
    let row = status_row(
        901,
        &t,
        &[TrainBatchSummary {
            members: vec![901],
            started: 1,
            status: "green".into(),
        }],
    );
    assert_eq!(row.members, vec![1]);
    assert_eq!(row.verdict, "green");
}
