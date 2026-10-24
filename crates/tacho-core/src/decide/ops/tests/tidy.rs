//! Parity tests for `decide::ops::tidy` (C1–C3, B1–B7, K1–K9, W1–W4, U1, S1–S4, N1–N4).

use crate::decide::ops::tidy::*;

fn policy() -> TidyPolicy {
    TidyPolicy::default()
}

#[test]
fn c1_due_after_fifteen_minutes() {
    let p = policy();
    assert!(!due(1000, 1000 + 14 * 60, &p));
    assert!(due(1000, 1000 + 15 * 60, &p));
}

#[test]
fn c2_inside_matches_the_root_and_its_children_and_not_a_prefix_neighbour() {
    assert!(inside("/work/foo", "/work/foo"));
    assert!(inside("/work/foo/bar", "/work/foo"));
    assert!(!inside("/work/foobar", "/work/foo"));
}

#[test]
fn c3_a_clean_machine_produces_no_action() {
    let snap = Snapshot::default();
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn b1_an_old_unused_build_tree_is_removed_with_a_reason() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/old".into(),
            idle: 2 * 3600,
        }],
        ..Default::default()
    };
    let acts = decide(&snap, &policy());
    assert_eq!(
        acts,
        vec![TidyAction::RemoveTree {
            path: "/t/old".into(),
            why: "build output untouched for 2 h".into(),
        }]
    );
}

#[test]
fn b2_a_recently_touched_build_tree_is_kept() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/fresh".into(),
            idle: 30 * 60,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn b3_a_build_tree_a_live_process_has_as_its_target_is_kept() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/quiet".into(),
            idle: 4 * 3600,
        }],
        held_paths: vec!["/t/quiet/debug/deps".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn b4_a_build_tree_named_in_a_live_process_argument_is_kept() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/quiet".into(),
            idle: 4 * 3600,
        }],
        held_args: vec!["--out-dir=/t/quiet/debug".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn b5_a_build_tree_of_a_running_batch_is_kept() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/tacho-b-manual-5-r0".into(),
            idle: 4 * 3600,
        }],
        batches: vec!["manual-5".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn b6_a_build_tree_of_a_finished_batch_is_removed() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/tacho-b-manual-5-r0".into(),
            idle: 4 * 3600,
        }],
        ..Default::default()
    };
    assert_eq!(decide(&snap, &policy()).len(), 1);
}

#[test]
fn b7_a_build_tree_of_a_start_worktree_with_a_live_process_is_kept() {
    let snap = Snapshot {
        builds: vec![Tree {
            path: "/t/my-slug".into(),
            idle: 4 * 3600,
        }],
        starts: vec![("my-slug".into(), "/work/my-slug".into())],
        held_paths: vec!["/work/my-slug/src".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k1_an_old_unused_agent_database_is_removed() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "agentdb-old".into(),
            age: 3 * 3600,
            clients: 0,
            port: Some(5432),
            zombies: 0,
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveContainer {
            name: "agentdb-old".into(),
            why: "up 3 h with no client".into(),
        }]
    );
}

#[test]
fn k2_a_database_with_an_open_connection_is_kept() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "agentdb-live".into(),
            age: 3 * 3600,
            clients: 2,
            port: Some(5432),
            zombies: 0,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k3_a_database_whose_port_is_in_a_live_process_is_kept() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "agentdb-port".into(),
            age: 3 * 3600,
            clients: 0,
            port: Some(20001),
            zombies: 0,
        }],
        held_ports: vec![20001],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k4_a_database_of_a_start_worktree_with_a_live_process_is_kept() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "agentdb-my-slug".into(),
            age: 3 * 3600,
            clients: 0,
            port: Some(20001),
            zombies: 0,
        }],
        starts: vec![("my-slug".into(), "/work/my-slug".into())],
        held_paths: vec!["/work/my-slug".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k5_a_container_we_did_not_make_is_never_touched() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "postgres-prod".into(),
            age: 99 * 3600,
            clients: 0,
            port: Some(5432),
            zombies: 0,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k6_a_young_container_is_kept_even_without_clients() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "agentdb-young".into(),
            age: 30 * 60,
            clients: 0,
            port: Some(5432),
            zombies: 0,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k7_a_dead_batchs_database_is_removed_after_twenty_minutes_even_with_leaked_clients() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "tacho-b-manual-5-pg".into(),
            age: 25 * 60,
            clients: 4,
            port: Some(5432),
            zombies: 0,
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveContainer {
            name: "tacho-b-manual-5-pg".into(),
            why: "database of a batch that is no longer running, up 25 min".into(),
        }]
    );
}

#[test]
fn k8_a_running_batchs_database_is_kept_past_two_hours() {
    let snap = Snapshot {
        containers: vec![Container {
            name: "tacho-b-manual-5-pg".into(),
            age: 4 * 3600,
            clients: 0,
            port: Some(5432),
            zombies: 0,
        }],
        batches: vec!["manual-5".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn k9_zombie_processes_inside_a_container_are_reported() {
    let snap = Snapshot {
        containers: vec![
            Container {
                name: "agentdb-a".into(),
                zombies: 1,
                ..Default::default()
            },
            Container {
                name: "agentdb-b".into(),
                zombies: 3,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        zombie_lines(&snap),
        vec![
            "agentdb-a holds 1 zombie process",
            "agentdb-b holds 3 zombie processes"
        ]
    );
}

#[test]
fn w1_a_pushed_clean_old_worktree_is_removed() {
    let snap = Snapshot {
        checkouts: vec![Checkout {
            path: "/work/done".into(),
            idle: 8 * 3600,
            dirty: false,
            unpublished: false,
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveCheckout {
            path: "/work/done".into(),
            why: "untouched for 8 h with no live batch or process".into(),
            rescue: false,
        }]
    );
}

#[test]
fn w2_a_dirty_or_unpublished_old_worktree_is_rescued_then_removed() {
    let snap = Snapshot {
        checkouts: vec![
            Checkout {
                path: "/work/dirty".into(),
                idle: 8 * 3600,
                dirty: true,
                unpublished: false,
            },
            Checkout {
                path: "/work/unpushed".into(),
                idle: 8 * 3600,
                dirty: false,
                unpublished: true,
            },
        ],
        ..Default::default()
    };
    let acts = decide(&snap, &policy());
    assert_eq!(
        acts,
        vec![
            TidyAction::RemoveCheckout {
                path: "/work/dirty".into(),
                why: "untouched for 8 h with no live batch or process".into(),
                rescue: true,
            },
            TidyAction::RemoveCheckout {
                path: "/work/unpushed".into(),
                why: "untouched for 8 h with no live batch or process".into(),
                rescue: true,
            },
        ]
    );
}

#[test]
fn w3_a_throwaway_batch_worktree_is_removed_without_rescue() {
    let snap = Snapshot {
        checkouts: vec![Checkout {
            path: "/work/tacho-b-manual-5-wt".into(),
            idle: 8 * 3600,
            dirty: true,
            unpublished: true,
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveCheckout {
            path: "/work/tacho-b-manual-5-wt".into(),
            why: "untouched for 8 h with no live batch or process".into(),
            rescue: false,
        }]
    );
}

#[test]
fn w4_a_clean_worktree_with_a_live_process_inside_it_is_kept() {
    let snap = Snapshot {
        checkouts: vec![Checkout {
            path: "/work/live".into(),
            idle: 8 * 3600,
            dirty: false,
            unpublished: false,
        }],
        held_paths: vec!["/work/live/crates".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn u1_a_test_runtime_unpacked_a_day_ago_is_removed_unless_open() {
    let snap = Snapshot {
        unpacks: vec![
            Tree {
                path: "/tmp/robolectric-old".into(),
                idle: 25 * 3600,
            },
            Tree {
                path: "/tmp/robolectric-held".into(),
                idle: 25 * 3600,
            },
        ],
        held_paths: vec!["/tmp/robolectric-held/lib.so".into()],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveTree {
            path: "/tmp/robolectric-old".into(),
            why: "test runtime unpacked 25 h ago".into(),
        }]
    );
}

#[test]
fn s1_a_cache_under_its_cap_is_kept() {
    let snap = Snapshot {
        caches: vec![Sized {
            path: "/c/sccache".into(),
            gb: 40,
            cap: 80,
            held: false,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn s2_a_cache_over_its_cap_is_removed_with_its_size_in_the_reason() {
    let snap = Snapshot {
        caches: vec![Sized {
            path: "/c/sccache".into(),
            gb: 95,
            cap: 80,
            held: false,
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveTree {
            path: "/c/sccache".into(),
            why: "cache is 95 GB, over its 80 GB".into(),
        }]
    );
}

#[test]
fn s3_a_cache_over_its_cap_that_a_build_holds_is_kept() {
    let snap = Snapshot {
        caches: vec![Sized {
            path: "/warm/slot-1/target".into(),
            gb: 95,
            cap: 80,
            held: true,
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn s4_the_rescue_store_is_capped_at_twenty_gigabytes() {
    let caches = default_caches("/h/sccache", "/h/cache", "/work", "", &[]);
    let rescue_spec = caches.iter().find(|c| c.path == "/work/.rescued").unwrap();
    assert_eq!(rescue_spec.cap_gb, 20);
}

#[test]
fn n1_a_network_of_a_finished_batch_is_removed_with_its_attached_containers() {
    let snap = Snapshot {
        networks: vec![Network {
            name: "tacho-b-manual-5-net".into(),
            containers: vec!["tacho-b-manual-5-svc".into()],
        }],
        ..Default::default()
    };
    assert_eq!(
        decide(&snap, &policy()),
        vec![TidyAction::RemoveNetwork {
            name: "tacho-b-manual-5-net".into(),
            why: "network of a batch that is no longer running".into(),
            containers: vec!["tacho-b-manual-5-svc".into()],
        }]
    );
}

#[test]
fn n2_a_network_of_a_running_batch_is_kept() {
    let snap = Snapshot {
        networks: vec![Network {
            name: "tacho-b-manual-5-net".into(),
            containers: vec!["tacho-b-manual-5-svc".into()],
        }],
        batches: vec!["manual-5".into()],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn n3_a_network_we_did_not_make_for_a_batch_is_never_touched() {
    let snap = Snapshot {
        networks: vec![Network {
            name: "bridge".into(),
            containers: vec![],
        }],
        ..Default::default()
    };
    assert!(decide(&snap, &policy()).is_empty());
}

#[test]
fn n4_stale_runs_retirement_validates_days_and_filters_live_or_open_pr_runs() {
    assert!(stale_runs(&[], 100_000, 0, &[], &[]).is_err());
    let now = 10 * 86400;
    let runs = vec![
        RunFacts {
            key: "old-abandoned".into(),
            state: RunState::Abandoned,
            started: 100,
            finished: Some(200),
            pr: None,
            branch: None,
        },
        RunFacts {
            key: "has-open-pr".into(),
            state: RunState::Unfinished,
            started: 100,
            finished: Some(200),
            pr: Some(42),
            branch: None,
        },
    ];
    let retired = stale_runs(&runs, now, 3, &[], &["42".into()]).unwrap();
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].key, "old-abandoned");
    assert_eq!(stale_runs_summary(0), "no stale runs");
    assert_eq!(stale_runs_summary(1), "retired 1 stale run(s)");
}
