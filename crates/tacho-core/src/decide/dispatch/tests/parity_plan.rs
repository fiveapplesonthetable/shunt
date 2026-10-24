//! Parity cases 104..=126 (`test_plan.py` part 1).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::hands::{in_open_train, next_hands};
use crate::decide::dispatch::model::Base;
use crate::decide::dispatch::queue::compact;
use crate::decide::dispatch::select::{is_conflicting, next_work};

#[test]
fn parity_104_picks_first_ready_task() {
    let cfg = test_cfg();
    let a = make_todo("a", "ta", "");
    let mut b = make_todo("b", "tb", "");
    b.after = vec!["a".to_string()];
    let c = make_todo("c", "tc", "");
    let todo = vec![a, b, c];
    let p = next_work(
        &todo,
        &BTreeMap::new(),
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "a");
}

#[test]
fn parity_105_skips_running_and_blocked_tasks() {
    let cfg = test_cfg();
    let a = make_todo("a", "ta", "");
    let mut b = make_todo("b", "tb", "");
    b.after = vec!["a".to_string()];
    let c = make_todo("c", "tc", "");
    let todo = vec![a, b, c];
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Running, "ta"));
    let p1 = next_work(
        &todo,
        &runs,
        1,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p1.item.key, "c");

    runs.insert("a".to_string(), make_run(RunStatus::Done, "ta"));
    runs.insert("c".to_string(), make_run(RunStatus::Running, "tc"));
    let p2 = next_work(
        &todo,
        &runs,
        1,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p2.item.key, "b");
}

#[test]
fn parity_106_respects_concurrency_and_load() {
    let cfg = test_cfg();
    let todo = vec![make_todo("a", "ta", "")];
    assert!(next_work(
        &todo,
        &BTreeMap::new(),
        4,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .is_none());
    assert!(next_work(
        &todo,
        &BTreeMap::new(),
        0,
        4,
        true,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .is_none());
}

#[test]
fn parity_107_returns_none_when_all_done() {
    let cfg = test_cfg();
    let todo = vec![make_todo("a", "ta", "")];
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Done, "ta"));
    assert!(next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_108_compact_moves_done_items_into_history() {
    let todo = vec![
        make_todo("finished", "t1", "rust"),
        make_todo("pending", "t2", "rust"),
    ];
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Done, "t1");
    r.finished = Some(500);
    runs.insert("finished".to_string(), r);
    let history = BTreeMap::new();
    let heads = BTreeMap::new();
    let (waiting, archived, heads) = compact(&todo, &runs, &history, &heads);
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].key, "pending");
    assert!(archived.contains_key("finished"));
    assert_eq!(heads.get("rust").map(String::as_str), Some("finished"));
}

#[test]
fn parity_109_hands_picks_lowest_pr_number_first_and_mentions_parent() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(
        12,
        HandsRec {
            sha: "s12".to_string(),
            files: vec!["a.rs".to_string()],
            parent: None,
            sibling: false,
        },
    );
    hands.insert(
        9,
        HandsRec {
            sha: "s9".to_string(),
            files: vec!["b.rs".to_string()],
            parent: Some(7),
            sibling: false,
        },
    );
    let (pr, brief) = next_hands(
        &hands,
        &BTreeMap::new(),
        0,
        4,
        false,
        &BTreeSet::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(pr, 9);
    assert!(brief.contains("onto PR #7 head"));
}

#[test]
fn parity_110_hands_respects_slot_limit() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(9, HandsRec::default());
    assert!(next_hands(
        &hands,
        &BTreeMap::new(),
        4,
        4,
        false,
        &BTreeSet::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_111_hands_waits_when_machine_busy() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(9, HandsRec::default());
    assert!(next_hands(&hands, &BTreeMap::new(), 0, 4, true, &BTreeSet::new(), &cfg).is_none());
}

#[test]
fn parity_112_hands_runs_in_parallel_with_other_work_up_to_limit() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(
        9,
        HandsRec {
            sha: "s9".to_string(),
            ..HandsRec::default()
        },
    );
    hands.insert(
        12,
        HandsRec {
            sha: "s12".to_string(),
            ..HandsRec::default()
        },
    );
    let mut runs = BTreeMap::new();
    runs.insert("hands-9".to_string(), make_run(RunStatus::Running, "t9"));
    let (pr, _) = next_hands(&hands, &runs, 3, 4, false, &BTreeSet::new(), &cfg).expect("hands-12");
    assert_eq!(pr, 12);
    assert!(next_hands(&hands, &runs, 4, 4, false, &BTreeSet::new(), &cfg).is_none());
}

#[test]
fn parity_113_hands_skips_pr_already_attempted_at_same_sha() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(
        9,
        HandsRec {
            sha: "s9".to_string(),
            files: vec!["a.rs".to_string()],
            ..HandsRec::default()
        },
    );
    let (_, brief) = next_hands(
        &hands,
        &BTreeMap::new(),
        0,
        4,
        false,
        &BTreeSet::new(),
        &cfg,
    )
    .unwrap();
    let mut runs = BTreeMap::new();
    runs.insert("hands-9".to_string(), make_run(RunStatus::Done, &brief));
    assert!(next_hands(&hands, &runs, 0, 4, false, &BTreeSet::new(), &cfg).is_none());
}

#[test]
fn parity_114_hands_retries_when_sha_changes() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(
        9,
        HandsRec {
            sha: "new-sha".to_string(),
            files: vec!["a.rs".to_string()],
            ..HandsRec::default()
        },
    );
    let mut runs = BTreeMap::new();
    runs.insert(
        "hands-9".to_string(),
        make_run(RunStatus::Done, "old brief for old-sha"),
    );
    let (pr, _) = next_hands(&hands, &runs, 0, 4, false, &BTreeSet::new(), &cfg).unwrap();
    assert_eq!(pr, 9);
}

#[test]
fn parity_115_hands_skips_prs_barred_from_rebase() {
    let cfg = test_cfg();
    let mut hands = BTreeMap::new();
    hands.insert(9, HandsRec::default());
    hands.insert(12, HandsRec::default());
    let barred = BTreeSet::from([9u64, 12u64]);
    assert!(next_hands(&hands, &BTreeMap::new(), 0, 4, false, &barred, &cfg).is_none());
}

#[test]
fn parity_116_in_open_train_recognises_hands_keys() {
    let cfg = test_cfg();
    let in_flight = BTreeSet::from([9u64]);
    assert!(in_open_train("hands-9", &in_flight, &cfg));
    assert!(!in_open_train("hands-12", &in_flight, &cfg));
}

#[test]
fn parity_117_to_120_is_conflicting_rules() {
    // 117: CONFLICTING -> true
    assert!(is_conflicting(Mergeable::Conflicting, false));
    // 118: MERGEABLE -> false
    assert!(!is_conflicting(Mergeable::Mergeable, true));
    // 119: UNKNOWN keeps true
    assert!(is_conflicting(Mergeable::Unknown, true));
    // 120: UNKNOWN keeps false
    assert!(!is_conflicting(Mergeable::Unknown, false));
}

pub(super) fn area_chain_setup() -> (Vec<TodoItem>, BTreeMap<String, Run>) {
    let t1 = make_todo("t1", "task 1", "tax");
    let t2 = make_todo("t2", "task 2", "tax");
    let k1 = make_todo("k1", "kyc 1", "kyc");
    let free = make_todo("free", "free 1", "");
    (vec![t1, t2, k1, free], BTreeMap::new())
}

#[test]
fn parity_121_second_task_waits_while_first_is_running() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    runs.insert("t1".to_string(), make_run(RunStatus::Running, "task 1"));
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "kyc 1"));
    runs.insert("free".to_string(), make_run(RunStatus::Running, "free 1"));
    assert!(next_work(
        &todo,
        &runs,
        3,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_122_second_task_waits_while_first_pr_has_not_passed_batch() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "first-branch".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1".to_string(), r1);
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "kyc 1"));
    runs.insert("free".to_string(), make_run(RunStatus::Running, "free 1"));
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Waiting);
    assert!(next_work(
        &todo,
        &runs,
        2,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
}

#[test]
fn parity_123_second_task_starts_stacked_on_first_pr_once_it_passes() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "first-branch".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1".to_string(), r1);
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Passed);
    let p = next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "t2");
    assert_eq!(
        p.base,
        Some(Base::Pr {
            number: 7,
            branch: "first-branch".to_string()
        })
    );
}

#[test]
fn parity_124_second_task_starts_on_main_once_first_pr_merges() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "first-branch".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1".to_string(), r1);
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Merged);
    let p = next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "t2");
    assert_eq!(p.base, Some(Base::Main));
}

#[test]
fn parity_125_other_area_and_unassigned_tasks_run_in_parallel() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    runs.insert("t1".to_string(), make_run(RunStatus::Running, "task 1"));
    let p1 = next_work(
        &todo,
        &runs,
        1,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p1.item.key, "k1");
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "kyc 1"));
    let p2 = next_work(
        &todo,
        &runs,
        2,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p2.item.key, "free");
}

#[test]
fn parity_126_unassigned_task_has_no_base_instruction() {
    let cfg = test_cfg();
    let free = vec![make_todo("free", "free 1", "")];
    let p = next_work(
        &free,
        &BTreeMap::new(),
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "free");
    assert!(p.base.is_none());
}
