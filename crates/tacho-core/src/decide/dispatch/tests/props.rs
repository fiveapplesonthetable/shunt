//! Property-based tests (`proptest`) for the pure dispatch engine invariants.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;
use crate::decide::dispatch::engine::{decide_dispatch, DispatchEvent};
use crate::decide::dispatch::model::{ActionKind, RunKey};
use crate::decide::dispatch::runs::{base_key, lineage};

#[derive(Debug, Clone)]
enum SimStep {
    QueueTask {
        idx: usize,
        area_idx: usize,
        now: bool,
    },
    DispatchHands {
        pr: u64,
        head_idx: usize,
    },
    ExitAllRunning {
        finished_ok: bool,
    },
    HoldBase {
        idx: usize,
    },
    ReleaseBase {
        idx: usize,
    },
    AdvanceTime {
        delta: i64,
    },
}

fn sim_step_strategy() -> impl Strategy<Value = SimStep> {
    prop_oneof![
        (0..4usize, 0..3usize, any::<bool>())
            .prop_map(|(idx, area_idx, now)| { SimStep::QueueTask { idx, area_idx, now } }),
        (1..4u64, 0..3usize).prop_map(|(pr, head_idx)| SimStep::DispatchHands { pr, head_idx }),
        any::<bool>().prop_map(|finished_ok| SimStep::ExitAllRunning { finished_ok }),
        (0..4usize).prop_map(|idx| SimStep::HoldBase { idx }),
        (0..4usize).prop_map(|idx| SimStep::ReleaseBase { idx }),
        (30..300i64).prop_map(|delta| SimStep::AdvanceTime { delta }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_run_key_roundtrip(
        base in "[a-z][a-z0-9-]{0,12}",
        attempt in 1..20u32,
    ) {
        let rk = RunKey { base: base.clone(), attempt };
        let formatted = rk.to_string();
        let parsed = RunKey::parse(&formatted);
        prop_assert_eq!(parsed.base, base);
        prop_assert_eq!(parsed.attempt, attempt);
    }

    #[test]
    fn prop_dispatch_invariants_hold_over_any_event_sequence(
        steps in prop::collection::vec(sim_step_strategy(), 1..25),
        resume_limit in 0..=2u32,
    ) {
        let mut cfg = test_cfg();
        cfg.resume_limit = resume_limit;
        cfg.slots = 3;

        let mut state = DispatchState::default();
        let mut state_replay = DispatchState::default();
        let mut now = 1_000i64;
        let areas = ["", "area-a", "area-b"];
        let heads = [
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            "3333333333333333333333333333333333333333",
        ];

        // Track every StartSpec emitted across the entire trace.
        let mut starts_by_base_and_head: BTreeSet<(String, String)> = BTreeSet::new();
        let mut launches_per_base_at_head: BTreeMap<(String, Option<String>), u32> = BTreeMap::new();

        for step in steps {
            let mut obs = DispatchObs::default();
            obs.now = now;
            obs.env_live = true;

            match step {
                SimStep::QueueTask { idx, area_idx, now: is_now } => {
                    let key = format!("task-{idx}");
                    if !state.todo.iter().any(|t| t.key == key) && !state.runs.contains_key(&key) {
                        let mut item = make_todo(&key, &format!("do {key}"), areas[area_idx]);
                        item.now = is_now;
                        state.todo.push(item.clone());
                        state_replay.todo.push(item);
                    }
                }
                SimStep::DispatchHands { pr, head_idx } => {
                    let sha = heads[head_idx].to_string();
                    let rec = HandsRec {
                        sha: sha.clone(),
                        files: vec!["src/lib.rs".to_string()],
                        parent: None,
                        sibling: false,
                    };
                    state.hands.insert(pr, rec.clone());
                    state_replay.hands.insert(pr, rec);
                }
                SimStep::ExitAllRunning { finished_ok } => {
                    for (k, r) in &state.runs {
                        if r.status == RunStatus::Running {
                            let log = if finished_ok {
                                "Done.\nPR: none\n".to_string()
                            } else {
                                "died mid-stream".to_string()
                            };
                            obs.logs.insert(k.clone(), log);
                        }
                    }
                }
                SimStep::HoldBase { idx } => {
                    let key = format!("task-{idx}");
                    let h = Hold { why: "hold".to_string(), since: now };
                    state.holds.insert(key.clone(), h.clone());
                    state_replay.holds.insert(key, h);
                }
                SimStep::ReleaseBase { idx } => {
                    let key = format!("task-{idx}");
                    state.holds.remove(&key);
                    state_replay.holds.remove(&key);
                }
                SimStep::AdvanceTime { delta } => {
                    now += delta;
                    obs.now = now;
                }
            }

            // Keep alive procs for any running run not scheduled to exit in this step.
            for (k, r) in &state.runs {
                if r.status == RunStatus::Running && !obs.logs.contains_key(k) {
                    obs.procs.insert(
                        k.clone(),
                         crate::decide::dispatch::resume::ProcObs {
                            alive: true,
                            zombie: false,
                            process_start: r.process_start.clone(),
                        },
                    );
                }
            }

            // Provide open PR facts for any hands entry.
            let mut open_prs = Vec::new();
            for (&pr_num, rec) in &state.hands {
                open_prs.push(OpenPr {
                    number: pr_num,
                    head: rec.sha.clone(),
                    branch: format!("branch-{pr_num}"),
                    draft: false,
                    mergeable: Mergeable::Conflicting,
                });
                obs.pull_facts.insert(
                    pr_num,
                    Ok(open_facts(&rec.sha, &format!("branch-{pr_num}"))),
                );
            }
            obs.open_prs = Some(open_prs);

            let (next_state1, actions1) = decide_dispatch(
                &state,
                &DispatchEvent::Tick { dry_run: false },
                &obs,
                &cfg,
            );
            let (next_state2, actions2) = decide_dispatch(
                &state_replay,
                &DispatchEvent::Tick { dry_run: false },
                &obs,
                &cfg,
            );
            state = next_state1;
            state_replay = next_state2;

            // Invariant (d): determinism and exact replay equivalence.
            prop_assert_eq!(&actions1, &actions2);
            prop_assert_eq!(&state, &state_replay);

            // Every action has a non-empty human reason.
            for act in &actions1 {
                prop_assert!(!act.reason.trim().is_empty());
                if let ActionKind::Start(spec) = &act.kind {
                    let count = launches_per_base_at_head
                        .entry((spec.base_key.clone(), spec.head.clone()))
                        .or_insert(0);
                    *count += 1;
                    // Invariant (a): launches per base key (at a given head) <= 1 + resume_limit.
                    prop_assert!(
                        *count <= 1 + resume_limit,
                        "base {} launched {} times > 1 + {}",
                        spec.base_key,
                        *count,
                        resume_limit
                    );

                    // Invariant (b): never two attempt-1 runs at the same (base, head).
                    if spec.key == spec.base_key {
                        if let Some(h) = &spec.head {
                            let inserted = starts_by_base_and_head
                                .insert((spec.base_key.clone(), h.clone()));
                            prop_assert!(
                                inserted,
                                "duplicate attempt-1 launch for ({}, {})",
                                spec.base_key,
                                h
                            );
                        }
                    }
                }
            }

            // Also verify lineage length in state.runs never exceeds 1 + resume_limit.
            let all_bases: BTreeSet<String> = state.runs.keys().map(|k| base_key(k)).collect();
            for b in all_bases {
                let lin = lineage(&b, &state.runs);
                prop_assert!(lin.len() <= (1 + resume_limit) as usize);
            }

            // Invariant (c): at most one non-`now` running task per area at a time.
            let mut running_by_area: BTreeMap<String, usize> = BTreeMap::new();
            for item in &state.todo {
                if item.now || item.area.is_empty() {
                    continue;
                }
                let is_running = lineage(&item.key, &state.runs)
                    .iter()
                    .any(|(_, r)| r.status == RunStatus::Running);
                if is_running {
                    let c = running_by_area.entry(item.area.clone()).or_insert(0);
                    *c += 1;
                    prop_assert!(
                        *c <= 1,
                        "area {} has {} concurrent non-now runs",
                        item.area,
                        *c
                    );
                }
            }
        }
    }
}
