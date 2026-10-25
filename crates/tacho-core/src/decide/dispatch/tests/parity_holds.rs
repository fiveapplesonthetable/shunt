//! Parity with the reference project's hold and `--after` tests.
//!
//! Covered reference behaviour:
//! - `pr:<n>` entries are met only once merged; task entries once done; every entry must be met;
//! - malformed `pr:` entries are refused with the form to use; the waited-on PRs are collected;
//! - a hold needs a reason, a typo holds nothing, a second hold is refused, releasing what is not
//!   held says so, a release removes only its own hold, and the status line format;
//! - `next_work` skips a held task, starts it again once released, waits on an unmerged PR, lets a
//!   hold beat a met `after`, and a held first item keeps its area waiting;
//! - only batch groups count as held groups.
//!
//! Not covered here: batch planning itself (`verify::plan::batches` takes holds directly and lives
//! outside `decide::dispatch`).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::decide::dispatch::holds::{
    after_met, after_problem, held_groups, hold_lines, hold_problem, pending_prs, release_problem,
    with_hold, without_hold,
};
use crate::decide::dispatch::model::{Holds, PrStanding, Runs};
use crate::decide::dispatch::select::next_work;

/// The queue the reference tests use: two independent tasks.
fn todo() -> Vec<TodoItem> {
    vec![make_todo("a", "x", ""), make_todo("b", "y", "")]
}

/// `a` held because main is red, since t=100.
fn hold_a() -> Holds {
    BTreeMap::from([(
        "a".to_string(),
        Hold {
            why: "main is red".to_string(),
            since: 100,
        },
    )])
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn standing(entries: &[(u64, PrStanding)]) -> BTreeMap<u64, PrStanding> {
    entries.iter().copied().collect()
}

fn done(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|s| s.to_string()).collect()
}

/// The key `next_work` picks with four free slots, or `None`.
fn next_key(
    todo: &[TodoItem],
    standing: &BTreeMap<u64, PrStanding>,
    holds: &Holds,
) -> Option<String> {
    next_work(
        todo,
        &Runs::new(),
        0,
        4,
        false,
        standing,
        holds,
        &BTreeMap::new(),
        &test_cfg(),
    )
    .map(|w| w.item.key)
}

#[test]
fn pr_entry_is_met_only_once_the_pr_has_merged() {
    let after = strings(&["pr:954"]);
    assert!(!after_met(
        &after,
        &done(&[]),
        &standing(&[(954, PrStanding::Passed)])
    ));
    assert!(!after_met(&after, &done(&[]), &standing(&[])));
    assert!(after_met(
        &after,
        &done(&[]),
        &standing(&[(954, PrStanding::Merged)])
    ));
}

#[test]
fn task_entry_is_met_once_the_task_is_done() {
    let after = strings(&["a"]);
    assert!(!after_met(&after, &done(&[]), &standing(&[])));
    assert!(after_met(&after, &done(&["a"]), &standing(&[])));
}

#[test]
fn every_after_entry_must_be_met() {
    let after = strings(&["a", "pr:7"]);
    assert!(!after_met(
        &after,
        &done(&["a"]),
        &standing(&[(7, PrStanding::Waiting)])
    ));
    assert!(after_met(
        &after,
        &done(&["a"]),
        &standing(&[(7, PrStanding::Merged)])
    ));
}

#[test]
fn malformed_pr_entry_is_refused_with_the_form_to_use() {
    for bad in ["pr:954a", "pr:"] {
        let why = after_problem(bad).expect("malformed entry is refused");
        assert!(why.contains("pr:<number>"), "{why}");
    }
    assert_eq!(after_problem("pr:954"), None);
    assert_eq!(after_problem("some-task"), None);
}

#[test]
fn prs_queued_tasks_wait_on_are_collected() {
    let mut a = make_todo("a", "", "");
    a.after = strings(&["pr:5", "x"]);
    let mut b = make_todo("b", "", "");
    b.after = strings(&["pr:9"]);
    let c = make_todo("c", "", "");
    assert_eq!(pending_prs(&[a, b, c]), BTreeSet::from([5, 9]));
}

#[test]
fn hold_needs_a_reason() {
    let why = hold_problem(
        "core",
        "  ",
        &todo(),
        &Holds::new(),
        &Runs::new(),
        &test_cfg(),
    )
    .expect("a blank reason is refused");
    assert!(why.contains("--why"), "{why}");
}

#[test]
fn typo_does_not_hold_nothing() {
    let why = hold_problem(
        "cor",
        "red",
        &todo(),
        &Holds::new(),
        &Runs::new(),
        &test_cfg(),
    )
    .expect("an unknown target is refused");
    assert!(
        why.contains("neither a queued task, a running agent"),
        "{why}"
    );
}

#[test]
fn task_or_group_can_be_held() {
    let cfg = test_cfg();
    for target in ["a", "core"] {
        assert_eq!(
            hold_problem(target, "red", &todo(), &Holds::new(), &Runs::new(), &cfg),
            None,
            "{target}"
        );
    }
}

#[test]
fn second_hold_on_one_target_is_refused() {
    let why = hold_problem("a", "again", &todo(), &hold_a(), &Runs::new(), &test_cfg())
        .expect("a second hold is refused");
    assert!(why.contains("main is red"), "{why}");
}

#[test]
fn releasing_what_is_not_held_says_so() {
    assert_eq!(
        release_problem("b", &hold_a()).as_deref(),
        Some("b is not held")
    );
    assert_eq!(release_problem("a", &hold_a()), None);
}

#[test]
fn release_removes_only_its_own_hold() {
    let both = with_hold(&hold_a(), "core", " main is red ", 5);
    assert_eq!(
        both["core"],
        Hold {
            why: "main is red".to_string(),
            since: 5
        }
    );
    let left: Vec<String> = without_hold(&both, "core").into_keys().collect();
    assert_eq!(left, ["a"]);
}

#[test]
fn status_says_what_is_held_for_how_long_and_why() {
    assert_eq!(
        hold_lines(&hold_a(), 100 + 600),
        ["a                        held 10 min: main is red"]
    );
}

#[test]
fn held_task_is_skipped_for_the_next_one() {
    assert_eq!(
        next_key(&todo(), &standing(&[]), &hold_a()).as_deref(),
        Some("b")
    );
}

#[test]
fn released_task_starts_again() {
    let released = without_hold(&hold_a(), "a");
    assert_eq!(
        next_key(&todo(), &standing(&[]), &released).as_deref(),
        Some("a")
    );
}

#[test]
fn task_after_an_unmerged_pr_waits() {
    let mut a = make_todo("a", "x", "");
    a.after = strings(&["pr:954"]);
    let todo = [a];
    let none = Holds::new();
    assert_eq!(
        next_key(&todo, &standing(&[(954, PrStanding::Passed)]), &none),
        None
    );
    assert_eq!(
        next_key(&todo, &standing(&[(954, PrStanding::Merged)]), &none).as_deref(),
        Some("a")
    );
}

#[test]
fn hold_wins_over_an_after_that_has_been_met() {
    let mut a = make_todo("a", "x", "q");
    a.after = strings(&["pr:954"]);
    assert_eq!(
        next_key(&[a], &standing(&[(954, PrStanding::Merged)]), &hold_a()),
        None
    );
}

#[test]
fn held_first_item_keeps_the_rest_of_its_area_waiting() {
    let todo = [make_todo("a", "x", "q"), make_todo("b", "y", "q")];
    assert_eq!(next_key(&todo, &standing(&[]), &hold_a()), None);
}

#[test]
fn only_batch_groups_are_held_groups() {
    let holds = with_hold(&hold_a(), "core", "red", 0);
    assert_eq!(
        held_groups(&holds, &test_cfg()),
        BTreeSet::from(["core".to_string()])
    );
}
