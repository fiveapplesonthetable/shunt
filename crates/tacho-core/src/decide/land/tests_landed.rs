use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::super::assemble::{drop_breakers, settle, train_tasks};
use super::super::decide::{decide_land, LandActionKind, LandEvent, LandView};
use super::super::landed::{
    abandoned, in_flight_members, landed_comment, landed_in, landed_members, main_moved_news,
    merged_prs_from_subjects, newly_landed, resolve_standing, retire_rebases, standing,
    train_abandoned_news, train_landed_news, Standing,
};
use super::super::ready::{
    checked_areas, covered, drafts_to_ready, grew, is_conflicting, merged_since, queue_issue_body,
    ready_queue, sibling_rebases, split_siblings, still_ready, Conflicted, Passed, ReadyPr,
};
use super::super::rebase::{
    barred, classify_replay_finish, hands_task, in_open_train, next_hands,
    resolve_cherry_pick_conflict, BarInputs, BarPr, Hands, HandsAgent, ReplayOutcome,
};
use super::super::shots::{
    android_redraw_command, decode_png, encode_rgba_png, format_redraw_failure, golden_names,
    moved, moved_summary, pixel_moved, shot_stem, stale_shots_step, stray_refusal, stray_shots,
    ShotRules, StaleShotsStep,
};
use super::super::train::{Member, Train, TrainKind, TrainStatus};
use super::{fixture_cfg, COMPARISON, SHOT};

fn train_580() -> Train {
    Train::make_combined(
        580,
        "train-2026-10-15-1",
        "head580",
        vec![Member::new(501, "s501"), Member::new(502, "s502")],
        vec![],
        vec![],
        false,
    )
}

// ============================================================================
// test_train_landed.py parity tests
// ============================================================================

#[test]
fn every_member_of_a_landed_train_is_recorded_as_merged() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.trains.push(train_580());
    let dec = decide_land(
        &view,
        &LandEvent::SettleTrains {
            open_prs: BTreeSet::from([501, 502]),
            merged_prs: BTreeSet::from([580]),
            now: 1000,
        },
        &cfg,
    );
    assert!(dec.next.merged_prs.contains(&501));
    assert!(dec.next.merged_prs.contains(&502));
    assert_eq!(dec.next.merged_at.get(&501), Some(&1000));
    assert_eq!(dec.next.merged_at.get(&502), Some(&1000));
    assert_eq!(
        landed_members(&dec.next.trains, &dec.next.merged_prs),
        BTreeSet::from([501, 502])
    );
}

#[test]
fn members_are_closed_with_a_landed_in_comment() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.trains.push(train_580());
    let dec = decide_land(
        &view,
        &LandEvent::SettleTrains {
            open_prs: BTreeSet::from([501, 502]),
            merged_prs: BTreeSet::from([580]),
            now: 1000,
        },
        &cfg,
    );
    assert_eq!(landed_comment(580), "Landed in #580");
    assert!(dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::ClosePrWithComment { pr: 501, comment } if comment == "Landed in #580"
    )));
    assert!(dec.actions.iter().any(|a| matches!(
        &a.kind,
        LandActionKind::ClosePrWithComment { pr: 502, comment } if comment == "Landed in #580"
    )));
}

#[test]
fn a_train_is_settled_once() {
    let cfg = fixture_cfg();
    let mut view = LandView::default();
    view.trains.push(train_580());
    let d1 = decide_land(
        &view,
        &LandEvent::SettleTrains {
            open_prs: BTreeSet::from([501, 502]),
            merged_prs: BTreeSet::from([580]),
            now: 1000,
        },
        &cfg,
    );
    assert_eq!(
        newly_landed(&d1.next.trains, &d1.next.merged_prs),
        Vec::<u64>::new()
    );
    let d2 = decide_land(
        &d1.next,
        &LandEvent::SettleTrains {
            open_prs: BTreeSet::new(),
            merged_prs: BTreeSet::from([580]),
            now: 1010,
        },
        &cfg,
    );
    assert!(d2.actions.is_empty());
}

#[test]
fn a_queued_rebase_of_a_member_is_retired() {
    let cfg = fixture_cfg();
    let tasks = train_tasks(
        579,
        &[(501, vec!["a.rs".into()]), (503, vec!["b.rs".into()])],
        &[],
        &cfg,
    );
    let hands = BTreeMap::from([(501, Hands::default()), (503, Hands::default())]);
    let (kept_todo, kept_hands, retired) =
        retire_rebases(&tasks, &hands, &BTreeSet::from([501, 502]));
    assert_eq!(kept_todo.len(), 1);
    assert_eq!(kept_todo[0].pr, 503);
    assert!(!kept_hands.contains_key(&501));
    assert!(kept_hands.contains_key(&503));
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].why, "rebase for #501: PR merged");
}

#[test]
fn a_train_closed_without_merging_releases_its_members() {
    let trains = vec![train_580()];
    let ab = abandoned(&trains, &BTreeSet::from([501, 502]), &BTreeSet::new(), 200);
    assert_eq!(ab, vec![580]);
    assert_eq!(
        train_abandoned_news(580),
        "#580 was closed without merging; its members are free for another train"
    );
}

#[test]
fn a_train_still_open_keeps_its_members_in_flight() {
    let trains = vec![train_580()];
    assert!(abandoned(&trains, &BTreeSet::from([580]), &BTreeSet::new(), 200).is_empty());
    assert_eq!(in_flight_members(&trains), BTreeSet::from([501, 502]));
    assert!(in_open_train("hands-501", &BTreeSet::from([501, 502])));
    assert!(in_open_train("hands-501-r2", &BTreeSet::from([501, 502])));
    assert!(!in_open_train("hands-503", &BTreeSet::from([501, 502])));
}

#[test]
fn a_member_that_landed_in_a_train_is_retired_as_merged_not_closed() {
    let trains = vec![train_580()];
    let landed = landed_members(&trains, &BTreeSet::from([580]));
    assert!(landed.contains(&501));
    assert_eq!(standing("s501", None, true, true), Standing::Merged);
}

#[test]
fn a_pr_closed_with_a_landed_comment_counts_as_merged() {
    let comments = vec!["some comment", "Landed in #580"];
    assert_eq!(landed_in(&comments), Some(580));
    assert_eq!(
        resolve_standing("s501", None, false, false, false, true, &comments, false),
        Standing::Merged
    );
}

#[test]
fn a_closed_pr_whose_change_is_on_main_counts_as_merged() {
    let empty: Vec<&str> = Vec::new();
    assert_eq!(
        resolve_standing("s501", None, false, false, false, true, &empty, true),
        Standing::Merged
    );
}

#[test]
fn a_closed_pr_that_never_landed_is_closed_not_waiting() {
    let empty: Vec<&str> = Vec::new();
    assert_eq!(
        resolve_standing("s501", None, false, false, false, true, &empty, false),
        Standing::Closed
    );
}

#[test]
fn the_next_task_starts_once_its_predecessor_landed_in_a_train() {
    let subjects = vec![
        "Merge pull request #580 from acme/train-2026-10-15-1",
        "Merge pull request #500 from acme/feat-500",
    ];
    assert_eq!(
        merged_prs_from_subjects(&subjects, 500),
        BTreeSet::from([500, 580])
    );
    assert_eq!(
        train_landed_news(580, &[501, 502]),
        "#580 landed; #501 #502 closed as landed in it"
    );
    assert_eq!(
        main_moved_news("1234567890abcdef", Some(&[580])),
        "main moved to 12345678 with #580"
    );
}

#[test]
fn abandoned_truncation_guard() {
    let trains = vec![train_580()];
    let open: BTreeSet<u64> = (1..=200).collect();
    assert!(abandoned(&trains, &open, &BTreeSet::new(), 200).is_empty());
}

// ============================================================================
// test_plan.py parity tests (Ready, TheCompileGate, HandsWork, Mergeable)
// ============================================================================

#[test]
fn plan_ready_queue_siblings_and_drafts() {
    let pr1 = ReadyPr {
        number: 10,
        head: "h10".into(),
        conflicting: false,
    };
    let pr2 = ReadyPr {
        number: 11,
        head: "h11".into(),
        conflicting: false,
    };
    let pass10 = Passed::new("h10", "m0", &[10, 11], "b1");
    let pass11 = Passed::new("h11", "m0", &[10, 11], "b1");
    assert!(still_ready(&pr1, &pass10, Some(&[])));
    assert!(still_ready(&pr2, &pass11, Some(&[10])));
    assert!(!still_ready(&pr1, &pass10, Some(&[99])));

    let subjects = vec!["Merge pull request #10 from acme/a"];
    assert_eq!(merged_since(Some(&subjects)), Some(vec![10]));

    let touched = BTreeSet::from(["rust".to_string(), "formal".to_string()]);
    assert!(covered(
        &touched,
        &["rust".into()],
        &BTreeSet::from(["formal".into()])
    ));
    let post_merge = vec!["formal".to_string()];
    let batch_areas = vec!["rust".to_string()];
    assert!(checked_areas(None, &batch_areas, &touched, &post_merge).is_empty());

    let ready_map = BTreeMap::from([(11, pass11), (10, pass10)]);
    let started = BTreeMap::from([("b1".to_string(), 100)]);
    let q = ready_queue(&ready_map, &started);
    assert_eq!(q, vec![10, 11]);

    let (kept, sibs) = split_siblings(&q, |ahead, _n| ahead.is_empty());
    assert_eq!(kept, vec![10]);
    assert_eq!(sibs, vec![11]);

    let heads = BTreeMap::from([(10, "h10".into()), (11, "h11".into())]);
    let reb = sibling_rebases(&sibs, &heads, &BTreeMap::new());
    assert_eq!(reb.len(), 1);
    assert_eq!(reb[0].pr, 11);

    assert_eq!(
        drafts_to_ready(&kept, &BTreeSet::from([10]), false),
        vec![10]
    );
    assert!(drafts_to_ready(&[10, 11], &BTreeSet::from([10, 11]), false).is_empty());
    assert!(drafts_to_ready(&kept, &BTreeSet::from([10]), true).is_empty());
    assert!(grew(&[10], &[10, 11]));
    assert!(queue_issue_body("tacho", &[10], "tr1").contains("tacho land tr1"));
}

#[test]
fn plan_hands_work_barred_and_mergeable() {
    let cfg = fixture_cfg();
    assert!(is_conflicting(Conflicted::from_forge("UNKNOWN"), true));
    assert!(!is_conflicting(Conflicted::from_forge("UNKNOWN"), false));
    assert!(is_conflicting(Conflicted::from_forge("CONFLICTING"), false));
    assert!(!is_conflicting(Conflicted::from_forge("MERGEABLE"), true));

    let brief = hands_task(42, &Hands::files(&["src/lib.rs".into()]));
    assert!(brief.contains("PR #42"));
    assert!(brief.contains("src/lib.rs"));

    let open = vec![
        BarPr {
            number: 1,
            draft: true,
            mergeable: false,
            branch: "a".into(),
        },
        BarPr {
            number: 2,
            draft: false,
            mergeable: true,
            branch: "b".into(),
        },
        BarPr {
            number: 3,
            draft: false,
            mergeable: false,
            branch: "train-2026-10-15-1".into(),
        },
    ];
    let in_flight = BTreeSet::from([4]);
    let landed = BTreeSet::from([5]);
    let train_prs = BTreeSet::from([6]);
    let held_hands = BTreeSet::from([7]);
    let siblings = BTreeSet::new();
    let bar = barred(&BarInputs {
        open: &open,
        in_flight: &in_flight,
        landed_members: &landed,
        train_prs: &train_prs,
        held_hands: &held_hands,
        siblings: &siblings,
        train_branch_prefix: "train-",
    });
    for n in 1..=7 {
        assert!(bar.contains_key(&n), "missing {n}");
    }

    let hands_map = BTreeMap::from([(10, Hands::files(&["a.rs".into()]))]);
    let agents = BTreeMap::from([(
        10,
        HandsAgent {
            running: false,
            task: "old task".into(),
        },
    )]);
    assert!(next_hands(&hands_map, &agents, 0, 2, false, &BTreeSet::new()).is_some());
    assert!(next_hands(&hands_map, &agents, 2, 2, false, &BTreeSet::new()).is_none());

    assert_eq!(
        resolve_cherry_pick_conflict(&[SHOT.to_string()], &cfg),
        Ok(vec![SHOT.to_string()])
    );
    assert_eq!(
        resolve_cherry_pick_conflict(&[SHOT.to_string(), "crates/a.rs".to_string()], &cfg),
        Err(vec!["crates/a.rs".to_string(), SHOT.to_string()])
    );
    assert_eq!(
        classify_replay_finish("s2", "tree1", "tree1", false),
        ReplayOutcome::Clean
    );
    assert_eq!(
        classify_replay_finish("s2", "tree2", "tree1", false),
        ReplayOutcome::Replayed {
            new_sha: "s2".into()
        }
    );
}

// ============================================================================
// test_trainbuild.py and test_shots.py parity tests
// ============================================================================

#[test]
fn a_failed_console_redraw_keeps_its_full_log_and_reports_the_tail() {
    let output = (1..=45)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (saved, detail) = format_redraw_failure("npm run shots", &output, "/tmp/shots.log");
    assert!(saved.contains("line 1\n"));
    assert!(saved.contains("line 45"));
    assert!(detail.contains("/tmp/shots.log"));
    assert!(detail.contains("line 45"));
    assert!(!detail.contains("line 1\n"));
}

#[test]
fn android_screenshots_are_recorded_even_when_gradle_cached_the_test() {
    let cmd = android_redraw_command("env SLOT=1");
    assert!(cmd.contains("--rerun"));
    assert!(cmd.contains("-Proborazzi.test.record=true"));
}

#[test]
fn shots_scope_summary_and_pixels() {
    let rules = ShotRules::default();
    let files = vec![
        "frontend/admin/src/screens/HomeScreen.tsx".to_string(),
        "frontend/admin/screenshots/dashboard-dark.png".to_string(),
        "frontend/admin/screenshots/billing.png".to_string(),
    ];
    let strays = stray_shots(&files, &BTreeSet::new(), &rules);
    assert_eq!(
        strays,
        vec!["frontend/admin/screenshots/billing.png".to_string()]
    );
    assert!(stray_refusal(&strays, "tacho", "BASE").contains("billing.png"));

    assert_eq!(
        shot_stem("frontend/admin/screenshots/home-dark-phone.png"),
        "home"
    );
    assert_eq!(
        moved_summary(&[
            "frontend/admin/screenshots/home.png",
            "frontend/admin/screenshots/home-dark.png"
        ]),
        "2 shots moved: home (2)"
    );
    let goldens = golden_names(&["frontend/admin/screenshots.golden"], |_| {
        "# comment\nhome.png\n".into()
    });
    assert!(goldens.contains("frontend/admin/screenshots/home.png"));

    assert_eq!(
        stale_shots_step(true, "m2", Some("m1"), 100, 200, 1800),
        StaleShotsStep::Wait
    );
    assert_eq!(
        stale_shots_step(true, "m2", Some("m1"), 100, 2000, 1800),
        StaleShotsStep::Redraw
    );

    let a = encode_rgba_png(4, 4, |_, _| [10, 20, 30, 255]);
    let b = encode_rgba_png(4, 4, |_, _| [15, 20, 30, 255]); // diff 5 <= 24
    let c = encode_rgba_png(4, 4, |x, y| {
        if x == 0 && y == 0 {
            [200, 20, 30, 255]
        } else {
            [10, 20, 30, 255]
        }
    });
    assert_eq!(decode_png(Some(&a)).unwrap().width, 4);
    assert!(!moved(Some(&a), Some(&b), false));
    assert!(moved(Some(&a), Some(&b), true)); // golden is strict!
    assert!(pixel_moved(Some(&a), Some(&c), false, 24, 500));
}

// ============================================================================
// Proptest Invariants (§8 of land.md)
// ============================================================================

proptest! {
    #[test]
    fn prop_decide_land_is_deterministic_and_every_action_has_because(
        pr in 1u64..1000u64,
        ok in any::<bool>(),
        now in 1_000i64..1_000_000i64,
    ) {
        let cfg = fixture_cfg();
        let mut view = LandView {
            main_sha: "base0".into(),
            open_prs: BTreeMap::from([(pr, "sha1".into())]),
            ..Default::default()
        };
        let train = Train::make("base0", vec![Member::new(pr, "sha1")], "tree1");
        let id = train.id.clone();
        view.trains.push(train);

        let ev = if ok {
            LandEvent::LandRequested { id }
        } else {
            LandEvent::MainMoved {
                new_main: "base1".into(),
                merged_since: Some(vec![pr]),
                now,
            }
        };
        let d1 = decide_land(&view, &ev, &cfg);
        let d2 = decide_land(&view, &ev, &cfg);
        prop_assert_eq!(&d1, &d2);
        for action in &d1.actions {
            prop_assert!(!action.because.trim().is_empty());
        }
        let open_seq = d1
            .next
            .trains
            .iter()
            .filter(|t| matches!(t.kind, TrainKind::Sequential) && t.status == TrainStatus::Open)
            .count();
        prop_assert!(open_seq <= 1);
    }

    #[test]
    fn prop_drop_breakers_preserves_order_and_partitions_candidates(
        prs in prop::collection::vec(1u64..100u64, 1..8),
        bad_set in prop::collection::btree_set(1u64..100u64, 0..4),
    ) {
        let mut unique = Vec::new();
        for p in prs {
            if !unique.contains(&p) {
                unique.push(p);
            }
        }
        let (kept, dropped) = drop_breakers(&unique, |prefix| {
            prefix.iter().find(|n| bad_set.contains(n)).map(|n| format!("err #{n}"))
        });
        for &k in &kept {
            prop_assert!(!bad_set.contains(&k));
        }
        for (d, _) in &dropped {
            prop_assert!(bad_set.contains(d));
        }
        let expected_kept: Vec<u64> = unique.into_iter().filter(|n| !bad_set.contains(n)).collect();
        prop_assert_eq!(kept, expected_kept);
    }

    #[test]
    fn prop_unknown_mergeability_is_fixed_point(was in any::<bool>()) {
        prop_assert_eq!(is_conflicting(Conflicted::Unknown, was), was);
    }

    #[test]
    fn prop_settle_partitions_conflicted_files(
        kinds in prop::collection::vec(0u8..3u8, 0..10),
    ) {
        let cfg = fixture_cfg();
        let files: Vec<String> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| match k {
                0 => SHOT.to_string(),
                1 => COMPARISON.to_string(),
                _ => format!("crates/mod_{i}.rs"),
            })
            .collect();
        let s = settle(&files, &cfg);
        prop_assert_eq!(s.take.len() + s.by_hand.len(), files.len());
    }
}
