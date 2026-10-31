use std::collections::{BTreeMap, BTreeSet};
use tacho_core::decide::dispatch::Hold;
use tacho_core::decide::verify::BatchRun;
use tacho_core::decide::watch::{
    attention_notification_key, decide_owner_notifications, ready_delivery_key, ready_train_notice,
    AttentionItem,
};

fn batch() -> BatchRun {
    BatchRun {
        id: "b001".into(),
        prs: vec![7, 8],
        shas: [(7, "a".repeat(40)), (8, "b".repeat(40))].into(),
        main_sha: "base".into(),
        tree: Some("tree".into()),
        status: "passed".into(),
        finished_at: Some(1),
        ..Default::default()
    }
}

#[test]
fn hold_age_does_not_create_another_attention_identity() {
    let holds = BTreeMap::from([(
        "group:core".into(),
        Hold {
            reason: "owner paused this area".into(),
            since: 10,
        },
    )]);
    let mut item = AttentionItem {
        kind: "stale-hold".into(),
        target: "group:core".into(),
        summary: "held for 1h".into(),
        command_hint: "status".into(),
    };
    let first = attention_notification_key(&item, &holds);
    item.summary = "held for 2h".into();
    assert_eq!(attention_notification_key(&item, &holds), first);
    let mut different = holds.clone();
    different.get_mut("group:core").unwrap().since = 20;
    assert_ne!(attention_notification_key(&item, &different), first);
    let notified = BTreeSet::from([first]);
    assert!(decide_owner_notifications(
        &[item],
        &[],
        &[],
        &BTreeMap::new(),
        &[],
        &notified,
        &holds
    )
    .is_empty());
}

#[test]
fn every_head_byte_changes_the_delivery_identity() {
    let mut proof = batch();
    let first = ready_delivery_key(&proof.prs, &proof.shas).unwrap();
    proof.shas.insert(8, format!("{}c", "b".repeat(39)));
    assert_ne!(ready_delivery_key(&proof.prs, &proof.shas).unwrap(), first);
    let notice = ready_train_notice(
        "t001",
        &proof.prs,
        &proof.shas,
        &[proof.clone()],
        &BTreeSet::from([first]),
    )
    .unwrap();
    assert!(notice.announce);
}

#[test]
fn rechecked_heads_publish_the_new_train_without_announcing_again() {
    let mut proof = batch();
    let first = ready_train_notice(
        "t001",
        &proof.prs,
        &proof.shas,
        &[proof.clone()],
        &BTreeSet::new(),
    )
    .unwrap();
    let acknowledged = BTreeSet::from([first.delivery_key]);
    proof.id = "b002".into();
    let current = ready_train_notice(
        "t002",
        &proof.prs,
        &proof.shas,
        &[proof.clone()],
        &acknowledged,
    )
    .unwrap();
    assert!(!current.announce);
    let notices = decide_owner_notifications(
        &[],
        &[proof],
        &[current],
        &BTreeMap::new(),
        &[],
        &acknowledged,
        &BTreeMap::new(),
    );
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices.first().unwrap().ready.as_ref().unwrap().train_id,
        "t002"
    );
}

#[test]
fn ordered_subsets_are_covered_but_reordered_heads_need_an_announcement() {
    let mut proof = batch();
    let acknowledged = BTreeSet::from([ready_delivery_key(&proof.prs, &proof.shas).unwrap()]);
    proof.prs = vec![8];
    proof.shas.remove(&7);
    assert!(
        !ready_train_notice(
            "subset",
            &proof.prs,
            &proof.shas,
            &[proof.clone()],
            &acknowledged
        )
        .unwrap()
        .announce
    );
    proof = batch();
    proof.prs.reverse();
    assert!(
        ready_train_notice(
            "reordered",
            &proof.prs,
            &proof.shas,
            &[proof.clone()],
            &acknowledged
        )
        .unwrap()
        .announce
    );
}

#[test]
fn missing_members_or_an_unproved_tree_cannot_be_published() {
    let mut proof = batch();
    proof.shas.remove(&8);
    assert!(ready_train_notice(
        "t001",
        &proof.prs,
        &proof.shas,
        &[proof.clone()],
        &BTreeSet::new()
    )
    .is_none());
    proof = batch();
    proof.tree = None;
    assert!(ready_train_notice(
        "t001",
        &proof.prs,
        &proof.shas,
        &[proof.clone()],
        &BTreeSet::new()
    )
    .is_none());
}
