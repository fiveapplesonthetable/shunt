use std::collections::BTreeMap;
use tacho_core::decide::verify::{reusable_area_sources, AreaCheck, BatchMode, BatchRun};

fn proof() -> BatchRun {
    BatchRun {
        id: "b001".into(),
        prs: vec![7, 8],
        shas: BTreeMap::from([(7, "head-7".into()), (8, "head-8".into())]),
        main_sha: "base".into(),
        tree: Some("tree".into()),
        status: "passed".into(),
        finished_at: Some(20),
        areas: vec!["runtime".into()],
        checks: vec![AreaCheck {
            area: "runtime".into(),
            units: vec!["all".into()],
        }],
        ..Default::default()
    }
}

fn coverage(history: &[BatchRun]) -> BTreeMap<String, String> {
    reusable_area_sources(
        history,
        &[7, 8],
        &proof().shas,
        "base",
        "tree",
        BatchMode::Presubmit,
    )
}

#[test]
fn every_source_identity_and_complete_scope_is_required_for_reuse() {
    assert_eq!(
        coverage(&[proof()]),
        BTreeMap::from([("runtime".into(), "b001".into())])
    );
    let changes: [fn(&mut BatchRun); 10] = [
        |b| b.main_sha = "new-base".into(),
        |b| b.tree = Some("new-tree".into()),
        |b| b.prs.reverse(),
        |b| {
            b.shas.insert(7, "new-head".into());
        },
        |b| b.status = "failed".into(),
        |b| b.finished_at = None,
        |b| b.flakes.push("raced".into()),
        |b| b.skipped.push("runtime".into()),
        |b| b.mode = BatchMode::Postsubmit,
        |b| b.checks[0].units = vec!["one-module".into()],
    ];
    for change in changes {
        let mut batch = proof();
        change(&mut batch);
        assert!(coverage(&[batch]).is_empty());
    }
}

#[test]
fn a_later_failure_prevents_reusing_an_earlier_green_area() {
    let mut red = proof();
    red.id = "b002".into();
    red.status = "failed".into();
    assert!(coverage(&[proof(), red]).is_empty());
}

#[test]
fn composed_areas_keep_their_original_execution_provenance() {
    let mut composed = proof();
    composed.id = "b002".into();
    composed
        .area_sources
        .insert("runtime".into(), "b001".into());
    assert_eq!(coverage(&[proof(), composed])["runtime"], "b001");
}
