//! Parity test for holds at batch-planning time: a held group starts no batch while every other
//! group still does. Drives the pure [`batches`] planner with a `group:` hold.

use super::*;

fn open_pr(n: u64, file: &str) -> PullFacts {
    PullFacts {
        number: n,
        head: format!("s{n}"),
        base: "main".into(),
        state: "OPEN".into(),
        files: vec![file.into()],
        ..Default::default()
    }
}

#[test]
fn a_held_group_starts_no_batch_and_the_others_still_do() {
    let rules = vec![
        AreaRule {
            area: "rust".into(),
            prefixes: vec!["crates/".into()],
            ..Default::default()
        },
        AreaRule {
            area: "android".into(),
            prefixes: vec!["apps/android/".into()],
            ..Default::default()
        },
    ];
    let open = [
        open_pr(1, "crates/core/src/a.rs"),
        open_pr(2, "apps/android/b.kt"),
    ];
    let holds = BTreeMap::from([("group:rust".to_string(), Hold::default())]);
    let plans = batches(
        &open,
        &holds,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        &rules,
        2,
    );
    let groups: Vec<&str> = plans.iter().map(|p| p.group.as_str()).collect();
    assert_eq!(groups, vec!["android"]);
}
