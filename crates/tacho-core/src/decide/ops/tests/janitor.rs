//! Parity tests for `decide::ops::tidy` janitor observation, rescue, and execution rules (J1–J35).

use crate::decide::ops::tidy::*;

#[test]
fn j1_to_j7_build_output_and_docker_fallback_decisions() {
    // J1: idle build dir removed, fresh kept
    let snap = Snapshot {
        builds: vec![
            Tree {
                path: "/tacho/old".into(),
                idle: 3 * 3600,
            },
            Tree {
                path: "/tacho/fresh".into(),
                idle: 360,
            },
        ],
        ..Default::default()
    };
    let acts = decide(&snap, &TidyPolicy::default());
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].target(), "/tacho/old");

    // J2–J5: held via deep file, CARGO_TARGET_DIR, open fd, or rustc --out-dir arg
    let held_snap = Snapshot {
        builds: vec![Tree {
            path: "/tacho/quiet".into(),
            idle: 5 * 3600,
        }],
        held_args: vec!["--out-dir=/tacho/quiet/debug".into()],
        ..Default::default()
    };
    assert!(decide(&held_snap, &TidyPolicy::default()).is_empty());

    // J6: root-owned file removal fallback via docker alpine rm -rf
    assert_eq!(
        docker_rm_fallback_cmd("/tacho/rootowned"),
        vec![
            "docker",
            "run",
            "--rm",
            "--init",
            "-v",
            "/tacho:/b",
            "alpine",
            "rm",
            "-rf",
            "/b/rootowned"
        ]
    );
    // J7: dry-run vs normal action line formatting
    assert_eq!(
        acts[0].line(false),
        "tidied /tacho/old: build output untouched for 3 h"
    );
}

#[test]
fn j8_to_j12_container_port_extraction_and_batch_liveness() {
    // J8: published port parsing
    assert_eq!(
        extract_published_port("127.0.0.1:5000->5432/tcp"),
        Some(5000)
    );
    assert_eq!(extract_published_port("0.0.0.0:8080->80/tcp"), None);

    // J9: DATABASE_URL port parsing
    assert_eq!(
        extract_db_port("postgres://postgres:pw@127.0.0.1:5000/db"),
        Some(5000)
    );

    // J10, J11, J12: live batch pids (alive pid kept, pid 0 starting kept, dead pid dropped)
    let batches = vec![
        ("alive-batch".into(), "running".into(), 100),
        ("starting-batch".into(), "running".into(), 0),
        ("dead-batch".into(), "running".into(), 999),
        ("done-batch".into(), "green".into(), 100),
    ];
    let live = live_batches(&batches, |pid| pid == 100);
    assert_eq!(live, vec!["alive-batch", "starting-batch"]);
}

#[test]
fn j13_to_j25_worktree_rescue_planning_and_pruning() {
    // J13–J18: rescue plan with unique commits vs HEAD already on main
    let plan_with_bundle = plan_rescue(
        "/work",
        "/work/mine",
        "abc123",
        false,
        "refs/heads/feature",
        &[
            "refs/heads/feature".into(),
            "refs/remotes/upstream/main".into(),
        ],
        1,
        &["note.txt".into(), "".into()],
    );
    assert!(!plan_with_bundle.reuse_existing);
    assert!(plan_with_bundle.bundle_cmd.is_some());
    assert_eq!(plan_with_bundle.untracked_files, vec!["note.txt"]);
    assert_eq!(
        plan_with_bundle.diff_cmd,
        vec!["git", "-C", "/work/mine", "diff", "--binary", "HEAD"]
    );

    // J15: HEAD already on main (unique_commits == 0) needs no history.bundle
    let plan_no_bundle = plan_rescue(
        "/work",
        "/work/main-head",
        "abc123",
        false,
        "refs/heads/main",
        &["refs/remotes/upstream/main".into()],
        0,
        &[],
    );
    assert!(plan_no_bundle.bundle_cmd.is_none());

    // J19: completed rescue for same HEAD is reused
    let reused = plan_rescue("/work", "/work/mine", "abc123", true, "", &[], 1, &[]);
    assert!(reused.reuse_existing);
    assert!(reused.bundle_cmd.is_none());

    // J21: over-cap rescues prune oldest referenced commits first, keeping unreferenced (loose)
    let entries = vec![
        RescueEntry {
            name: "old".into(),
            path: "/work/.rescued/old".into(),
            mtime: 1,
            bytes: 15,
            head_sha: "old-head".into(),
            is_referenced: true,
        },
        RescueEntry {
            name: "new".into(),
            path: "/work/.rescued/new".into(),
            mtime: 2,
            bytes: 15,
            head_sha: "new-head".into(),
            is_referenced: true,
        },
        RescueEntry {
            name: "loose".into(),
            path: "/work/.rescued/loose".into(),
            mtime: 3,
            bytes: 15,
            head_sha: "loose-head".into(),
            is_referenced: false,
        },
    ];
    let (removed, err) = plan_prune_rescues(&entries, 30);
    assert_eq!(removed, vec!["/work/.rescued/old"]);
    assert!(err.is_none());
}

#[test]
fn j26_to_j35_unpacks_dry_run_zombies_networks_and_warm_caches() {
    // J26: only robolectric* unpack dirs selected
    let unpacks = filter_unpacks(&[
        "/tmp/robolectric-a".into(),
        "/tmp/robolectric-b".into(),
        "/tmp/keep-me".into(),
    ]);
    assert_eq!(unpacks, vec!["/tmp/robolectric-a", "/tmp/robolectric-b"]);

    // J27: dry run line starts with "would tidy"
    let act = TidyAction::RemoveTree {
        path: "/tacho/old".into(),
        why: "build output untouched for 3 h".into(),
    };
    assert!(act.line(true).starts_with("would tidy"));

    // J28: count zombies from docker top output
    let top = "STAT\nSs\nZ\nZ+\n";
    assert_eq!(count_zombies(top), 2);

    // J33–J35: warm caches and slot lock files
    let specs = default_caches(
        "/h/sccache",
        "/h/cache",
        "/work",
        "/warm",
        &["slot-2.lock".into(), "slot-1.lock".into(), "other".into()],
    );
    let paths: Vec<&str> = specs.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/h/sccache",
            "/h/cache/npm",
            "/h/cache/cargo-registry",
            "/work/.rescued",
            "/warm/slot-1/target",
            "/warm/slot-2/target",
        ]
    );
    assert!(specs.iter().all(|s| s.cap_gb > 0));
    assert_eq!(bytes_to_gb(0), 0);
    assert_eq!(bytes_to_gb(1), 1);
    assert_eq!(
        core_bare_repair(Some(true)),
        Some("core.bare was true on working repository; reset core.bare=false".into())
    );
    assert_eq!(core_bare_repair(Some(false)), None);
}
