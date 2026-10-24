use std::collections::{BTreeMap, BTreeSet};

use super::super::assemble::{ci_proof_pending, MainCi, CI_NOTICE};
use super::super::decide::{decide_land, LandActionKind, LandEvent, LandView};
use super::super::ready::{land_command, without_train_prs, Passed};
use super::super::release::{
    check_areas, letter, merge_command, redraw_areas, required_areas, status_row, waiting_for,
    Notice, ProvedBatch, Waiting,
};
use super::super::train::{
    after_outside_merge, finished, mismatch, open_for, open_for_id, refusal, reuse_or_make,
    with_train, MailState, Member, Refusal, Train, TrainId, TrainStatus,
};
use super::fixture_cfg;

fn combined_train() -> Train {
    let mut t = Train::make_combined(
        580,
        "train-2026-10-15-1",
        "head580",
        vec![Member::new(501, "s501"), Member::new(502, "s502")],
        vec![],
        vec![],
        false,
    );
    t.reviewed = Some("head580".into());
    t
}

fn green_batch() -> ProvedBatch {
    ProvedBatch {
        status: "green".into(),
        skipped: vec![],
        skips: vec![],
        flaky: vec![],
        proved_tree: Some("tree580".into()),
        proved_runs: BTreeMap::from([
            ("compile".into(), "run-1".into()),
            ("rust".into(), "run-2".into()),
        ]),
    }
}

fn passed_580() -> Passed {
    Passed::new("head580", "main0", &[580], "b580")
}

#[test]
fn a_train_is_not_ready_while_an_area_its_members_touch_has_no_passing_run() {
    let (cfg, t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    let req = BTreeSet::from(["compile".to_string(), "console".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::MissingAreas {
            areas: vec!["console".into()]
        })
    );
}

#[test]
fn a_train_is_ready_once_each_required_area_names_the_run_that_proved_it() {
    let (cfg, t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    let req = BTreeSet::from(["compile".to_string(), "rust".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        None
    );
}

#[test]
fn an_area_listed_without_a_run_does_not_count() {
    let (cfg, t, p, mut b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    b.proved_runs.insert("rust".into(), String::new());
    let req = BTreeSet::from(["compile".to_string(), "rust".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::MissingAreas {
            areas: vec!["rust".into()]
        })
    );
}

#[test]
fn a_complete_pass_and_a_release_mail_one_command_and_make_it_ready() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.trains.push(Train::make_combined(
        580,
        "train-2026-10-15-1",
        "head580",
        vec![Member::new(501, "s501"), Member::new(502, "s502")],
        vec![],
        vec![],
        false,
    ));
    view.passed.insert(580, passed_580());
    view.proved_batches.insert("b580".into(), green_batch());
    let req = BTreeSet::from(["compile".to_string(), "rust".to_string()]);
    let dec = decide_land(
        &view,
        &LandEvent::ReleaseRequested {
            pr: 580,
            head: "head580".into(),
            tree_now: "tree580".into(),
            required: req,
        },
        &cfg,
    );
    assert!(dec
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::MarkReady { pr: 580 })));
    assert!(dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::SendMail { pr: 580, body, .. } if body.contains("gh pr merge 580")
    )));
}

#[test]
fn a_train_is_mailed_only_once() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    let mut t = combined_train();
    t.mail = MailState::Sent {
        head: "head580".into(),
    };
    view.trains.push(t);
    view.passed.insert(580, passed_580());
    view.proved_batches.insert("b580".into(), green_batch());
    let dec = decide_land(
        &view,
        &LandEvent::MailCheck {
            pr: 580,
            head: "head580".into(),
            tree_now: "tree580".into(),
            required: BTreeSet::from(["compile".to_string()]),
        },
        &cfg,
    );
    assert!(!dec
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::SendMail { .. })));
}

#[test]
fn ready_and_mail_happen_only_after_the_review_and_tree_gate() {
    let (cfg, mut t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    t.reviewed = None;
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::WaitingForReview {
            tool: "tacho".into()
        })
    );
}

#[test]
fn an_interrupted_notification_is_not_sent_a_second_time_automatically() {
    let (cfg, mut t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    t.mail = MailState::Intent {
        head: "head580".into(),
    };
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::AlreadyMailed)
    );
    assert_eq!(
        status_row(580, &t, &[]).notice,
        Notice::MailOutcomeUncertain
    );
}

#[test]
fn a_failed_mail_can_be_retried_without_marking_the_train_mailed() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    let mut t = combined_train();
    t.mail = MailState::Intent {
        head: "head580".into(),
    };
    view.trains.push(t);
    view.passed.insert(580, passed_580());
    view.proved_batches.insert("b580".into(), green_batch());
    let req = BTreeSet::from(["compile".to_string()]);

    let failed = decide_land(
        &view,
        &LandEvent::MailResult {
            pr: 580,
            head: "head580".into(),
            ok: false,
            err: "err".into(),
        },
        &cfg,
    );
    assert!(matches!(
        failed.next.trains[0].mail,
        MailState::Failed { .. }
    ));

    let retried = decide_land(
        &failed.next,
        &LandEvent::MailCheck {
            pr: 580,
            head: "head580".into(),
            tree_now: "tree580".into(),
            required: req,
        },
        &cfg,
    );
    assert!(retried
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::SendMail { pr: 580, .. })));
}

#[test]
fn a_pass_without_a_release_mails_nothing() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    let mut t = combined_train();
    t.reviewed = None;
    view.trains.push(t);
    view.passed.insert(580, passed_580());
    view.proved_batches.insert("b580".into(), green_batch());
    let dec = decide_land(
        &view,
        &LandEvent::MailCheck {
            pr: 580,
            head: "head580".into(),
            tree_now: "tree580".into(),
            required: BTreeSet::from(["compile".to_string()]),
        },
        &cfg,
    );
    assert!(!dec
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::SendMail { .. })));
}

#[test]
fn a_release_without_a_pass_mails_nothing() {
    let (cfg, t, b) = (fixture_cfg(), combined_train(), green_batch());
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, None, Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::NoPassAtHead)
    );
}

#[test]
fn a_failed_or_pending_batch_is_not_mailed() {
    let (cfg, t, p, mut b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    b.status = "running".into();
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::BatchNotGreen {
            status: "running".into()
        })
    );
}

#[test]
fn a_skipped_member_is_not_mailed() {
    let (cfg, t, p, mut b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    b.skipped = vec![502];
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::BatchSkipped)
    );
}

#[test]
fn a_configured_skipped_required_area_blocks_release() {
    let mut cfg = fixture_cfg();
    cfg.skip_areas.insert("console".into());
    let selected = BTreeSet::from(["console".to_string(), "compile".to_string()]);
    let req = required_areas(&["frontend/admin/src/App.tsx".to_string()], &selected, &cfg);
    assert!(req.contains("console"));
    let (t, p, b) = (combined_train(), passed_580(), green_batch());
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::MissingAreas {
            areas: vec!["console".into()]
        })
    );
}

#[test]
fn a_head_that_changed_after_the_pass_is_not_mailed() {
    let (cfg, t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "newer_head", "tree580", &req, &cfg),
        Some(Waiting::NoPassAtHead)
    );
}

#[test]
fn a_release_of_an_older_head_does_not_cover_a_newer_one() {
    let (cfg, mut t, b) = (fixture_cfg(), combined_train(), green_batch());
    t.reviewed = Some("older_head".into());
    let p = Passed::new("newer_head", "main0", &[580], "b580");
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "newer_head", "tree580", &req, &cfg),
        Some(Waiting::WaitingForReview {
            tool: "tacho".into()
        })
    );
}

#[test]
fn a_pass_on_another_tree_is_not_mailed() {
    let (cfg, t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "other_tree", &req, &cfg),
        Some(Waiting::TreeMoved)
    );
}

#[test]
fn a_green_batch_with_a_flake_waits_for_a_fresh_stable_pass() {
    let (cfg, t, p, mut b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    b.flaky = vec!["test_flaky".into()];
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        Some(Waiting::BatchFlaky)
    );
}

#[test]
fn a_frontend_train_needs_both_frontends_checked_on_the_tree() {
    let cfg = fixture_cfg();
    let selected = BTreeSet::from(["console".to_string(), "android".to_string()]);
    let files = ["frontend/admin/src/a.tsx", "frontend/android/app/Main.kt"];
    let req = required_areas(&files, &selected, &cfg);
    assert!(req.contains("console") && req.contains("android") && req.contains("compile"));
}

#[test]
fn the_general_ready_queue_does_not_offer_a_train() {
    assert_eq!(
        without_train_prs(&[501, 580, 502], &BTreeSet::from([580])),
        vec![501, 502]
    );
}

#[test]
fn the_train_release_command_does_not_replace_held_work_release() {
    let cfg = fixture_cfg();
    let dec = decide_land(
        &LandView::default(),
        &LandEvent::ReleaseRequested {
            pr: 610,
            head: "h610".into(),
            tree_now: "t610".into(),
            required: BTreeSet::new(),
        },
        &cfg,
    );
    assert!(dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::RefuseRelease { pr: 610, reason } if reason.contains("not a train pull request")
    )));
    assert_eq!(land_command("tacho", "abc12345"), "tacho land abc12345");
}

#[test]
fn the_mail_names_exactly_one_command() {
    let cfg = fixture_cfg();
    let (_, body) = letter(
        580,
        &[501, 502],
        &[],
        &[],
        false,
        &cfg.upstream,
        &cfg.project,
        "head580",
    );
    assert_eq!(body.matches("gh pr merge ").count(), 1);
}

#[test]
fn a_merge_command_pins_the_reviewed_head() {
    let cfg = fixture_cfg();
    assert_eq!(
        merge_command(580, &cfg.upstream, "deadbeef1234"),
        "gh pr merge 580 -R acme-org/acme --merge --match-head-commit deadbeef1234"
    );
}

#[test]
fn the_letter_of_such_a_train_says_one_main_run_must_pass_first() {
    let cfg = fixture_cfg();
    let (_, body) = letter(
        580,
        &[501],
        &[],
        &[],
        true,
        &cfg.upstream,
        &cfg.project,
        "head580",
    );
    assert!(body.contains(CI_NOTICE));
}

#[test]
fn an_open_postsubmit_failure_allows_a_repair_train_and_proved_release() {
    let (cfg, t, p, b) = (fixture_cfg(), combined_train(), passed_580(), green_batch());
    assert_eq!(
        ci_proof_pending(
            None,
            &MainCi {
                sha: "m1".into(),
                ok: false
            },
            |_, _| true
        ),
        None
    );
    let req = BTreeSet::from(["compile".to_string()]);
    assert_eq!(
        waiting_for(&t, Some(&p), Some(&b), "head580", "tree580", &req, &cfg),
        None
    );
}

fn all_areas() -> BTreeSet<String> {
    BTreeSet::from([
        "compile".into(),
        "rust".into(),
        "machine".into(),
        "console".into(),
        "console-crawl".into(),
        "android".into(),
        "formal".into(),
    ])
}

#[test]
fn a_docs_only_train_still_compiles_tests_and_drives_the_machine() {
    let cfg = fixture_cfg();
    let req = required_areas(&["docs/spec.md"], &all_areas(), &cfg);
    assert!(req.contains("compile") && req.contains("rust") && req.contains("machine"));
    assert!(!req.contains("console") && !req.contains("android"));
}

#[test]
fn a_server_train_runs_neither_client_suite() {
    let cfg = fixture_cfg();
    let req = required_areas(&["crates/acme-server/src/lib.rs"], &all_areas(), &cfg);
    assert!(!req.contains("console") && !req.contains("android"));
}

#[test]
fn an_app_change_runs_android_and_not_the_console() {
    let cfg = fixture_cfg();
    let files = ["frontend/android/app/src/Main.kt"];
    let req = required_areas(&files, &all_areas(), &cfg);
    assert!(req.contains("android") && !req.contains("console"));
    assert_eq!(
        redraw_areas(&files, &cfg),
        BTreeSet::from(["android".into()])
    );
}

#[test]
fn a_console_change_runs_the_console_and_not_android() {
    let cfg = fixture_cfg();
    let files = ["frontend/admin/src/App.tsx"];
    let req = required_areas(&files, &all_areas(), &cfg);
    assert!(req.contains("console") && !req.contains("android"));
    assert_eq!(
        redraw_areas(&files, &cfg),
        BTreeSet::from(["console".into()])
    );
}

#[test]
fn a_route_the_app_reads_runs_android_and_not_the_console() {
    let cfg = fixture_cfg();
    let req = required_areas(
        &["crates/acme-platform/src/api/mobile.rs"],
        &all_areas(),
        &cfg,
    );
    assert!(req.contains("android") && !req.contains("console"));
}

#[test]
fn a_route_the_console_reads_runs_the_console_and_not_android() {
    let cfg = fixture_cfg();
    let req = required_areas(
        &["crates/acme-operator-web/src/routes.rs"],
        &all_areas(),
        &cfg,
    );
    assert!(req.contains("console") && !req.contains("android"));
}

#[test]
fn a_screenshot_runs_only_the_frontend_it_belongs_to() {
    let cfg = fixture_cfg();
    let a = required_areas(
        &["frontend/android/docs/screenshots/Home.png"],
        &all_areas(),
        &cfg,
    );
    let c = required_areas(&["frontend/admin/screenshots/Home.png"], &all_areas(), &cfg);
    assert!(a.contains("android") && !a.contains("console"));
    assert!(c.contains("console") && !c.contains("android"));
}

#[test]
fn a_change_to_the_shared_checks_runs_no_client_suite() {
    let cfg = fixture_cfg();
    let req = required_areas(&["scripts/checks.sh"], &all_areas(), &cfg);
    assert!(!req.contains("console") && !req.contains("android"));
}

#[test]
fn a_workflow_that_publishes_shots_leaves_the_sweep_to_main() {
    let cfg = fixture_cfg();
    let req = required_areas(&[".github/workflows/shots.yaml"], &all_areas(), &cfg);
    assert!(!req.contains("console") && !req.contains("android"));
}

#[test]
fn a_proof_change_waits_for_main() {
    let cfg = fixture_cfg();
    let req_train = required_areas(&["specs/Model.tla"], &all_areas(), &cfg);
    assert!(!req_train.contains("formal"));
    let req_change = check_areas(
        &all_areas(),
        &["specs/Model.tla"],
        &BTreeSet::new(),
        false,
        &cfg,
    );
    assert!(req_change.contains("formal"));
}

#[test]
fn a_formal_input_runs_after_merge_not_on_a_change() {
    let cfg = fixture_cfg();
    let req_change = check_areas(
        &all_areas(),
        &["crates/acme-platform/src/lib.rs"],
        &BTreeSet::new(),
        false,
        &cfg,
    );
    assert!(!req_change.contains("formal"));
}

#[test]
fn the_areas_a_train_needs_before_its_mail_are_the_ones_it_runs() {
    let cfg = fixture_cfg();
    let selected = all_areas();
    for path in [
        "docs/spec.md",
        "frontend/admin/src/App.tsx",
        "frontend/android/app/Main.kt",
        "crates/acme-platform/src/api/mobile.rs",
        "crates/acme-operator-web/src/routes.rs",
    ] {
        let files = [path];
        let mut expected = check_areas(&selected, &files, &BTreeSet::new(), true, &cfg);
        expected.extend(redraw_areas(&files, &cfg));
        assert_eq!(required_areas(&files, &selected, &cfg), expected);
    }
}

#[test]
fn sequential_train_merges_each_member_and_verifies_main_tree_at_end() {
    let cfg = fixture_cfg();
    let members = vec![Member::new(101, "sha101"), Member::new(102, "sha102")];
    let train = reuse_or_make(&[], "base000", &members, Some("verified_tree")).unwrap();
    let id = train.id.clone();
    let trains = with_train(&[], train);
    assert_eq!(open_for_id(&trains, &[101, 102]), id.0);
    assert!(open_for(&trains, &[101, 102]).is_some());

    let view = LandView {
        main_sha: "base000".into(),
        open_prs: BTreeMap::from([(101, "sha101".into()), (102, "sha102".into())]),
        trains,
        ..Default::default()
    };

    let d1 = decide_land(&view, &LandEvent::LandRequested { id: id.clone() }, &cfg);
    assert!(d1.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::MergePr { pr: 101, match_head, .. } if match_head == "sha101"
    )));

    let d2 = decide_land(
        &d1.next,
        &LandEvent::MergeResult {
            id: id.clone(),
            pr: 101,
            ok: true,
            err: String::new(),
            now: 100,
        },
        &cfg,
    );
    assert!(d2.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::MergePr { pr: 102, match_head, .. } if match_head == "sha102"
    )));

    let d3 = decide_land(
        &d2.next,
        &LandEvent::MergeResult {
            id: id.clone(),
            pr: 102,
            ok: true,
            err: String::new(),
            now: 110,
        },
        &cfg,
    );
    assert!(d3.actions.iter().any(
        |a| matches!(&a.kind, LandActionKind::VerifyMainTree { train_id } if train_id == &id)
    ));

    let d4 = decide_land(
        &d3.next,
        &LandEvent::MainTreeVerified {
            id: id.clone(),
            main_sha: "main_after".into(),
            main_tree: "verified_tree".into(),
            now: 120,
        },
        &cfg,
    );
    assert_eq!(d4.next.trains[0].status, TrainStatus::Landed);
    assert_eq!(d4.next.merged_prs, BTreeSet::from([101, 102]));
}

#[test]
fn sequential_train_tree_mismatch_and_refusal_checks() {
    let cfg = fixture_cfg();
    let mut train = Train::make("base000", vec![Member::new(101, "sha101")], "verified_tree");
    let id = train.id.clone();

    assert_eq!(
        refusal(
            &train,
            "other_main",
            &BTreeMap::from([(101, "sha101".into())])
        ),
        Some(Refusal::MainMoved {
            main: "other_main".into(),
            base: "base000".into()
        })
    );
    assert!(mismatch(&train, "wrong_tree").is_some());
    assert!(matches!(
        after_outside_merge(&[train.clone()])[0].status,
        TrainStatus::Invalidated { .. }
    ));

    train.status = TrainStatus::VerifyingTree;
    train.merged = vec![101];
    let view = LandView {
        main_sha: "base000".into(),
        trains: vec![train],
        ..Default::default()
    };
    let dec = decide_land(
        &view,
        &LandEvent::MainTreeVerified {
            id,
            main_sha: "new_main".into(),
            main_tree: "wrong_tree".into(),
            now: 200,
        },
        &cfg,
    );
    assert!(matches!(
        dec.next.trains[0].status,
        TrainStatus::Mismatch { .. }
    ));
    assert!(dec
        .actions
        .iter()
        .any(|a| matches!(a.kind, LandActionKind::LoudPost { .. })));
    let _ = finished(&dec.next.trains, &TrainId("x".into()), TrainStatus::Landed);
}
