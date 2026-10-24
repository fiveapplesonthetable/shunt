//! Parity tests for `decide::ops::kit` (KE1–KE4, KP1–KP7, KR1–KR11, KS1–KS12).

use std::collections::{BTreeMap, BTreeSet};

use crate::decide::ops::kit::*;

#[test]
fn ke1_to_ke4_first_error_extraction() {
    // KE1: rustc error[E0308]
    let log = "Compiling a\nwarning: unused\nerror[E0308]: mismatched types\n --> src/a.rs:3\n\nerror: aborting";
    assert!(first_error(log).starts_with("error[E0308]: mismatched types"));

    // KE2: failed test named
    assert!(
        first_error("running 2 tests\ntest worker::tests::slow ... FAILED\n")
            .contains("worker::tests::slow")
    );

    // KE3: typescript error found
    assert!(first_error("> tsc\nsrc/a.tsx(1,2): error TS2322: nope").contains("TS2322"));

    // KE4: log with no recognizable error gives last non-blank lines
    assert_eq!(first_error("a\nb\n\nc\n"), "a\nb\nc");
}

#[test]
fn kp1_to_kp7_proof_problems_and_check_environment() {
    let ok = ProofRecord {
        head: "c".repeat(40),
        tree: "T".into(),
        ok: true,
        checks: vec![],
    };
    // KP1: no proof is refused
    assert!(proof_problems(None, "T", false, "tacho")[0].contains("tacho prove"));

    // KP2: failed proof names the check and its error
    let failed = ProofRecord {
        head: "c".repeat(40),
        tree: "T".into(),
        ok: false,
        checks: vec![ProofCheckResult {
            name: "rust fmt".into(),
            cmd: vec!["cargo".into(), "fmt".into()],
            ok: false,
            seconds: 1,
            output: String::new(),
            error: "Diff in a.rs".into(),
        }],
    };
    assert!(proof_problems(Some(&failed), "T", false, "tacho")[0]
        .contains("failed at rust fmt: Diff in a.rs"));

    // KP3: proof of other files is older than the commit
    assert!(proof_problems(Some(&ok), "OTHER", false, "tacho")[0]
        .contains("older than the last commit"));

    // KP4: uncommitted changes are refused
    assert!(proof_problems(Some(&ok), "T", true, "tacho")[0].contains("uncommitted"));

    // KP5: proof of exact tree stands
    assert!(proof_problems(Some(&ok), "T", false, "tacho").is_empty());

    // KP6 & KP7: check_environment adds rust_cache to cargo/conservative and gradle_slot to conservative only
    let mut inherited = BTreeMap::new();
    inherited.insert("PATH".into(), "/bin".into());
    let mut rust_cache = BTreeMap::new();
    rust_cache.insert(
        "RUSTC_WRAPPER".into(),
        "/tree/scripts/rustc-wrapper.sh".into(),
    );

    let rust_check = ProofCheck {
        name: "rust tests".into(),
        cmd: vec!["cargo".into(), "test".into()],
        ..Default::default()
    };
    let env1 = check_environment(
        &rust_check,
        &inherited,
        &rust_cache,
        "gradle-slot",
        "TACHO_GRADLE_SLOT",
    );
    assert_eq!(
        env1.get("RUSTC_WRAPPER").map(String::as_str),
        Some("/tree/scripts/rustc-wrapper.sh")
    );
    assert!(!env1.contains_key("TACHO_GRADLE_SLOT"));

    let cons_check = ProofCheck {
        name: "conservative checks".into(),
        cmd: vec!["scripts/checks.sh".into(), "all".into()],
        ..Default::default()
    };
    let env2 = check_environment(
        &cons_check,
        &inherited,
        &rust_cache,
        "gradle-slot",
        "TACHO_GRADLE_SLOT",
    );
    assert_eq!(
        env2.get("RUSTC_WRAPPER").map(String::as_str),
        Some("/tree/scripts/rustc-wrapper.sh")
    );
    assert_eq!(
        env2.get("TACHO_GRADLE_SLOT").map(String::as_str),
        Some("gradle-slot")
    );
}

#[test]
fn kr1_to_kr11_pr_refusals_attribution_and_build_body() {
    let ok = ProofRecord {
        head: "a".repeat(40),
        tree: "T".into(),
        ok: true,
        checks: vec![ProofCheckResult {
            name: "rust fmt".into(),
            cmd: vec!["cargo".into(), "fmt".into()],
            ok: true,
            seconds: 3,
            output: "fine".into(),
            error: String::new(),
        }],
    };
    let clean_commits = vec![("c".repeat(40), "A thing that is true\n\nBecause.".into())];

    // KR1: clean branch is not refused
    assert!(pr_refusals(
        &["a.rs".into()],
        &clean_commits,
        "A thing that is true",
        "Because.",
        &[],
        Some(&ok),
        "T",
        false,
        "tacho"
    )
    .is_empty());

    // KR2: empty diff refused
    assert!(pr_refusals(
        &[],
        &clean_commits,
        "Title",
        "Desc",
        &[],
        Some(&ok),
        "T",
        false,
        "tacho"
    )[0]
    .contains("changes nothing"));

    // KR3: Co-Authored-By in commit refused by short sha
    let co_commits = vec![(
        "abcdef1234".into(),
        "Fix\n\nCo-Authored-By: Someone <a@b>".into(),
    )];
    let got_co = pr_refusals(
        &["a.rs".into()],
        &co_commits,
        "Title",
        "Desc",
        &[],
        Some(&ok),
        "T",
        false,
        "tacho",
    );
    assert!(got_co[0].contains("commit abcdef12") && got_co[0].contains("Co-Authored-By"));

    // KR4 & KR5: Generated with and session link refused
    assert!(!pr_refusals(
        &["a.rs".into()],
        &clean_commits,
        "Title",
        "Done.\n\nGenerated with a tool",
        &[],
        Some(&ok),
        "T",
        false,
        "tacho"
    )
    .is_empty());
    assert!(!pr_refusals(
        &["a.rs".into()],
        &clean_commits,
        "Title",
        "see https://claude.ai/code/session_01abc",
        &[],
        Some(&ok),
        "T",
        false,
        "tacho"
    )
    .is_empty());

    // KR6: merge commits refused
    let got_merge = pr_refusals(
        &["a.rs".into()],
        &clean_commits,
        "Title",
        "Desc",
        &["1234567890".into()],
        Some(&ok),
        "T",
        false,
        "tacho",
    );
    assert!(got_merge[0].contains("12345678") && got_merge[0].contains("plain commits"));

    // KR7, KR8, KR9: missing proof, empty title, and multiple reasons returned at once
    let multi = pr_refusals(
        &[],
        &clean_commits,
        "Title",
        "Desc",
        &["1234567890".into()],
        None,
        "T",
        false,
        "tacho",
    );
    assert_eq!(multi.len(), 3);

    // KR10: ordinary prose about sessions and generation is not attribution
    assert!(
        attribution("Sessions expire; the code generator is rerun and the user is signed in.")
            .is_empty()
    );

    // KR11: build_body carries proof and stack
    let body = build_body("Why.", &ok, Some(816), "tacho");
    assert!(body.contains("Stacks on #816."));
    assert!(body.contains("**rust fmt**: passed in 3s"));
    assert!(body.contains("$ cargo fmt\nfine"));
}

#[test]
fn ks1_to_ks12_setup_slug_ports_migrations_lock_key_node_cache_env_and_path() {
    // KS1: slug validation
    assert_eq!(slug_problem("a-held-rate-can-be-discarded"), "");
    for bad in [
        "",
        "ab",
        "Has-Caps",
        "two--dashes",
        "trailing-",
        "has space",
        &"x".repeat(81),
    ] {
        assert!(!slug_problem(bad).is_empty(), "{bad}");
    }

    // KS2–KS6: port allocation
    assert_eq!(allocate_ports(&BTreeSet::new(), |_| false), Some(20000));
    let mut taken = BTreeSet::new();
    taken.insert(20000);
    assert_eq!(allocate_ports(&taken, |_| false), Some(20020));
    assert_eq!(
        allocate_ports(&BTreeSet::new(), |p| p == 20007),
        Some(20020)
    );
    assert!((PORT_LOW as u32) + (PORT_SIZE as u32) * (PORT_BLOCKS as u32) < 32768);
    let all_taken: BTreeSet<u16> = (0..PORT_BLOCKS).map(|i| PORT_LOW + i * PORT_SIZE).collect();
    assert_eq!(allocate_ports(&all_taken, |_| false), None);

    // KS7: migration order by filename across crates
    let paths = vec![
        "crates/a/migrations/0044_b.sql".to_string(),
        "crates/b/migrations/0043_a.sql".to_string(),
        "crates/a/migrations/20260901000000_c.sql".to_string(),
        "crates/a/migrations/0100_d.sql".to_string(),
    ];
    let ordered = migration_order(&paths);
    let basenames: Vec<&str> = ordered
        .iter()
        .map(|p| p.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        basenames,
        vec![
            "0043_a.sql",
            "0044_b.sql",
            "0100_d.sql",
            "20260901000000_c.sql"
        ]
    );

    // KS8 & KS9: lock_key and node_cache_state
    assert_eq!(lock_key(b"a"), lock_key(b"a"));
    assert_ne!(lock_key(b"a"), lock_key(b"b"));
    assert_eq!(node_cache_state(true, true).as_str(), "ready");
    assert_eq!(node_cache_state(false, true).as_str(), "building");
    assert_eq!(node_cache_state(false, false).as_str(), "absent");

    // KS10: export env file round-trip
    let mut vals = BTreeMap::new();
    vals.insert("A".into(), "x y".into());
    vals.insert("B".into(), "it's".into());
    vals.insert("C".into(), "3".into());
    vals.insert("D".into(), "".into());
    assert_eq!(parse_export_env(&export_env_file(&vals)), vals);

    // KS11 & KS12: merge_path and owns_path
    assert_eq!(
        merge_path("/c/bin:/usr/bin", "/usr/bin:/bin"),
        "/c/bin:/usr/bin:/bin"
    );
    assert!(owns_path("/work/wt/a", "/work/wt"));
    assert!(!owns_path("/work/wt-other", "/work/wt"));
}
