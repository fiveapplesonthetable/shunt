//! Parity test suite (spec §3, cases 1–233) and property-based tests (`proptest`)
//! for `decide::verify`.
#![allow(
    clippy::cloned_ref_to_slice_refs,
    clippy::field_reassign_with_default,
    clippy::while_let_loop
)]

use super::*;
use std::collections::BTreeMap;

mod batch_edge_tests;
mod batch_tests;
mod plan_areas_tests;
mod proptests;
mod warm_ci_tests;

pub(super) fn ok_area(tests: u64) -> AreaResult {
    AreaResult {
        rc: 0,
        wall_s: 2,
        cpu_s: 3.0,
        cache_hits: 5,
        cache_misses: 1,
        tests_passed: tests,
    }
}

pub(super) fn fail_area(rc: i32) -> AreaResult {
    AreaResult {
        rc,
        wall_s: 2,
        cpu_s: 3.0,
        cache_hits: 0,
        cache_misses: 1,
        tests_passed: 0,
    }
}

pub(super) fn green_check(merged: &[u64], areas: &[&str], tree: Option<&str>) -> CheckResult {
    let mut stats = BTreeMap::new();
    for &a in areas {
        stats.insert(a.to_string(), ok_area(4));
    }
    CheckResult {
        rc: 0,
        process_rc: None,
        merged: merged.to_vec(),
        skipped: Vec::new(),
        skips: BTreeMap::new(),
        line: format!("passed: {}", areas.join(" ")),
        tree: tree.map(str::to_string),
        stats,
        log: format!(
            "test result: ok. 4 passed; 0 failed\npassed: {}\n",
            areas.join(" ")
        ),
    }
}

pub(super) fn red_check(
    merged: &[u64],
    all_areas: &[&str],
    failed: &[&str],
    log: &str,
    tree: Option<&str>,
) -> CheckResult {
    let mut stats = BTreeMap::new();
    for &a in all_areas {
        if failed.contains(&a) {
            stats.insert(a.to_string(), fail_area(1));
        } else {
            stats.insert(a.to_string(), ok_area(4));
        }
    }
    CheckResult {
        rc: 1,
        process_rc: None,
        merged: merged.to_vec(),
        skipped: Vec::new(),
        skips: BTreeMap::new(),
        line: format!("failed: {}", failed.join(" ")),
        tree: tree.map(str::to_string),
        stats,
        log: format!("{log}\nfailed: {}\n", failed.join(" ")),
    }
}

pub(super) fn demo_area_cfg() -> AreaSelectConfig {
    let mut cfg = AreaSelectConfig::default();
    let all = [
        "gate",
        "rust",
        "typescript",
        "schema",
        "console",
        "android",
        "pixel",
        "browser",
        "specs",
        "fuzz",
    ];
    cfg.all_areas = all.iter().map(|s| s.to_string()).collect();
    cfg.gate = Some("gate".to_string());
    cfg.post_merge = ["fuzz".to_string()].into_iter().collect();
    cfg.pixel_areas = ["pixel".to_string()].into_iter().collect();
    cfg.pixel_files = vec![r"^(apps/console|packages/ui)/".to_string()];
    cfg.change_extra = ["typescript".to_string()].into_iter().collect();
    cfg.train_unplaced_areas = ["gate".to_string(), "rust".to_string()]
        .into_iter()
        .collect();
    cfg.doc_ignore = vec![r"\.md$".to_string(), r"^docs/".to_string()];
    cfg.doc_keep = vec![r"^specs/.*\.md$".to_string()];
    cfg.train_inputs.insert(
        "android".to_string(),
        r"^apps/android/.*\.(kt|kts|xml|gradle|pro)$".to_string(),
    );
    cfg.train_inputs.insert(
        "specs".to_string(),
        r"^specs/.*\.(tla|cfg|lean)$".to_string(),
    );
    cfg.change_inputs.insert(
        "specs".to_string(),
        r"^specs/.*\.(tla|cfg|lean)$".to_string(),
    );
    cfg.generated.insert(
        r"^packages/sdk/src/generated\.ts$".to_string(),
        "schema".to_string(),
    );
    cfg.path_rules = vec![
        PathAreaRule {
            pattern: r"^crates/".to_string(),
            areas: ["gate", "rust"].iter().map(|s| s.to_string()).collect(),
            train_areas: Some(["gate", "rust"].iter().map(|s| s.to_string()).collect()),
        },
        PathAreaRule {
            pattern: r"^apps/console/".to_string(),
            areas: ["gate", "typescript", "console"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            train_areas: Some(
                ["gate", "typescript", "console", "browser"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
        },
        PathAreaRule {
            pattern: r"^apps/android/".to_string(),
            areas: ["gate", "android"].iter().map(|s| s.to_string()).collect(),
            train_areas: Some(["gate", "android"].iter().map(|s| s.to_string()).collect()),
        },
        PathAreaRule {
            pattern: r"^specs/".to_string(),
            areas: ["gate", "specs"].iter().map(|s| s.to_string()).collect(),
            train_areas: Some(["gate", "specs"].iter().map(|s| s.to_string()).collect()),
        },
        PathAreaRule {
            pattern: r"^packages/sdk/".to_string(),
            areas: ["gate", "typescript", "schema"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            train_areas: Some(
                ["gate", "typescript", "schema"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
        },
    ];
    cfg
}
