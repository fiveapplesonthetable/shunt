//! Parity cases 127..=152 (`test_plan.py` part 2, `test_tick_loop.py`, `test_shells.py`).

use std::collections::BTreeMap;

use super::parity_plan::area_chain_setup;
use super::*;
use crate::decide::dispatch::engine::{decide_dispatch, DispatchEvent};
use crate::decide::dispatch::model::{Base, ForgePr};
use crate::decide::dispatch::queue::decision_tasks;
use crate::decide::dispatch::report::{find_pr, record_prs, stated_pr};
use crate::decide::dispatch::resume::reap_runs;
use crate::decide::dispatch::select::{area_report, next_work, pr_standing, stacked_task};

#[test]
fn parity_127_done_predecessor_with_no_pr_unblocks_on_main() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::NoPr { stated: false };
    runs.insert("t1".to_string(), r1);
    let p = next_work(
        &todo,
        &runs,
        0,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "t2");
    assert_eq!(p.base, Some(Base::Main));
    let rep = area_report(&todo, &runs, &BTreeMap::new(), &BTreeMap::new(), &cfg);
    assert!(rep["tax"].iter().any(|l| l.contains("no PR to wait for")));
}

#[test]
fn parity_128_done_predecessor_whose_pr_was_never_looked_up_still_waits() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::NotLooked;
    runs.insert("t1".to_string(), r1);
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "k1"));
    runs.insert("free".to_string(), make_run(RunStatus::Running, "free"));
    assert!(next_work(
        &todo,
        &runs,
        2,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
    let rep = area_report(&todo, &runs, &BTreeMap::new(), &BTreeMap::new(), &cfg);
    assert!(rep["tax"]
        .iter()
        .any(|l| l.contains("looking for the PR of t1")));
}

#[test]
fn parity_129_guessed_pr_blocks_chain_until_person_confirms() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "b7".to_string(),
        how: PrHow::Guessed,
    });
    runs.insert("t1".to_string(), r1);
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "k1"));
    runs.insert("free".to_string(), make_run(RunStatus::Running, "free"));
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Passed);
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
    let rep = area_report(&todo, &runs, &standings, &BTreeMap::new(), &cfg);
    assert!(rep["tax"].iter().any(|l| l.contains("guessed")));

    runs.get_mut("t1").unwrap().pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "b7".to_string(),
        how: PrHow::Attached,
    });
    let p = next_work(
        &todo,
        &runs,
        2,
        4,
        false,
        &standings,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg,
    )
    .unwrap();
    assert_eq!(p.item.key, "t2");
}

#[test]
fn parity_130_stated_no_pr_blocks_chain_until_person_confirms() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::NoPr { stated: true };
    runs.insert("t1".to_string(), r1);
    runs.insert("k1".to_string(), make_run(RunStatus::Running, "k1"));
    runs.insert("free".to_string(), make_run(RunStatus::Running, "free"));
    assert!(next_work(
        &todo,
        &runs,
        2,
        4,
        false,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &cfg
    )
    .is_none());
    let rep = area_report(&todo, &runs, &BTreeMap::new(), &BTreeMap::new(), &cfg);
    assert!(rep["tax"].iter().any(|l| l.contains("stated `PR: none`")));
}

#[test]
fn parity_131_resumed_run_completes_predecessor_in_area_chain() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    runs.insert("t1".to_string(), make_run(RunStatus::Unfinished, "task 1"));
    let mut r1_2 = make_run(RunStatus::Done, "task 1");
    r1_2.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "b7".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1~2".to_string(), r1_2);
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
}

#[test]
fn parity_132_stacked_task_tells_agent_which_base_to_branch_from() {
    let cfg = test_cfg();
    let item = make_todo("t2", "do t2", "tax");
    let s_pr = stacked_task(
        &item,
        Some(&Base::Pr {
            number: 7,
            branch: "first-branch".to_string(),
        }),
        &cfg,
    );
    assert!(s_pr.contains("--base 7"));
    assert!(s_pr.contains("stacks on #7"));
    let s_main = stacked_task(&item, Some(&Base::Main), &cfg);
    assert!(s_main.contains("upstream/main"));
}

#[test]
fn parity_133_area_report_shows_chain_state() {
    let cfg = test_cfg();
    let (todo, mut runs) = area_chain_setup();
    let mut r1 = make_run(RunStatus::Done, "task 1");
    r1.pr = PrRecord::Found(FoundPr {
        number: 7,
        branch: "b7".to_string(),
        how: PrHow::Stated,
    });
    runs.insert("t1".to_string(), r1);
    let mut standings = BTreeMap::new();
    standings.insert(7, PrStanding::Waiting);
    let rep = area_report(&todo, &runs, &standings, &BTreeMap::new(), &cfg);
    assert_eq!(
        rep["tax"],
        vec![
            "t1: done, PR #7 waiting".to_string(),
            "t2: queued, waiting on #7 to pass a batch".to_string(),
        ]
    );
    assert_eq!(rep["kyc"], vec!["k1: queued, ready to start".to_string()]);
}

#[test]
fn parity_134_to_137_stated_pr_parsing() {
    let cfg = test_cfg();
    assert_eq!(
        stated_pr("PR: #100\nsome text\nPR: #854\n"),
        Some(Some(854))
    );
    let mut runs = BTreeMap::new();
    let mut r = make_run(RunStatus::Done, "t");
    r.summary = "Checked PR #1\nPR: #9".to_string();
    runs.insert("a".to_string(), r);
    let mut branches = BTreeMap::new();
    branches.insert(9, "branch-9".to_string());
    record_prs(&mut runs, &[], &branches, &cfg);
    assert_eq!(best_pr_num(&runs, "a"), Some((9, PrHow::Stated)));
    assert_eq!(stated_pr("Done.\nPR: none\n"), Some(None));
    assert!(stated_pr("I opened PR: #12 inline here").is_none());
}

fn best_pr_num(runs: &BTreeMap<String, Run>, key: &str) -> Option<(u64, PrHow)> {
    match &runs[key].pr {
        PrRecord::Found(f) => Some((f.number, f.how)),
        _ => None,
    }
}

fn sample_forge_prs() -> Vec<ForgePr> {
    vec![
        ForgePr {
            number: 1,
            branch: "alpha".to_string(),
            owner: "fork".to_string(),
            state: PrState::Open,
            updated: 200,
        },
        ForgePr {
            number: 2,
            branch: "beta".to_string(),
            owner: "fork".to_string(),
            state: PrState::Open,
            updated: 300,
        },
        ForgePr {
            number: 4,
            branch: "gamma".to_string(),
            owner: "fork".to_string(),
            state: PrState::Merged,
            updated: 400,
        },
        ForgePr {
            number: 99,
            branch: "alpha".to_string(),
            owner: "other".to_string(),
            state: PrState::Open,
            updated: 350,
        },
    ]
}

#[test]
fn parity_138_to_144_find_pr_rules() {
    let prs = sample_forge_prs();
    assert_eq!(
        find_pr(&["beta".to_string()], "", &prs, 100, "fork")
            .unwrap()
            .number,
        2
    );
    assert_eq!(
        find_pr(&[], "pushed to alpha now", &prs, 100, "fork")
            .unwrap()
            .number,
        1
    );
    assert!(find_pr(&[], "wrote the alphabet", &prs, 100, "fork").is_none());
    assert_eq!(
        find_pr(&["gamma".to_string()], "", &prs, 100, "fork")
            .unwrap()
            .number,
        4
    );
    let two = vec!["alpha".to_string(), "beta".to_string()];
    assert_eq!(
        find_pr(&two, "opened PR #2", &prs, 100, "fork")
            .unwrap()
            .number,
        2
    );
    assert!(find_pr(&two, "done", &prs, 100, "fork").is_none());
    assert_eq!(
        find_pr(&[], "see #2 for details", &prs, 250, "fork")
            .unwrap()
            .number,
        2
    );
    assert!(find_pr(&[], "see #2 for details", &prs, 500, "fork").is_none());
    assert_eq!(
        find_pr(&["alpha".to_string()], "", &prs, 100, "fork")
            .unwrap()
            .number,
        1
    );
}

#[test]
fn parity_145_pr_standing_reads_open_prs_and_passed_set() {
    assert_eq!(pr_standing("sha3", None, true, false), PrStanding::Merged);
    assert_eq!(
        pr_standing("sha2", Some("sha2"), false, false),
        PrStanding::Passed
    );
    assert_eq!(
        pr_standing("sha1", Some("other"), false, false),
        PrStanding::Waiting
    );
    assert_eq!(pr_standing("", None, false, true), PrStanding::Closed);
}

#[test]
fn parity_146_and_147_live_env_refresh_never_breaks_tick() {
    let cfg = test_cfg();
    let env_text = format!(
        "{}=100000\n{}=3\n",
        cfg.env("CPU_LIMIT"),
        cfg.env("AGENT_SLOTS")
    );
    let (cfg2, err) = cfg.refresh_live(&env_text);
    assert!(err.is_none());
    assert_eq!(cfg2.slots, 3);
    assert_eq!(cfg2.max_cpu_percent, cfg.max_cpu_percent);

    let mut state = DispatchState::default();
    state.todo.push(make_todo("t-live", "do work", ""));
    let mut obs = DispatchObs::default();
    obs.now = 1000;
    obs.env_live = true;
    obs.live_env_text = Some(env_text);
    let (next_state, actions) =
        decide_dispatch(&state, &DispatchEvent::Tick { dry_run: false }, &obs, &cfg);
    assert!(!actions.is_empty());
    assert!(next_state.runs.contains_key("t-live"));
    let _ = decision_tasks(
        &next_state.todo,
        &next_state.task_history,
        &next_state.area_heads,
    );
}

#[test]
fn parity_148_and_149_reap_updates_finished_agents() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    runs.insert("a".to_string(), make_run(RunStatus::Running, "ta"));
    runs.insert("b".to_string(), make_run(RunStatus::Running, "tb"));
    let mut logs = BTreeMap::new();
    logs.insert("a".to_string(), "PR #9 is up.".to_string());
    logs.insert(
        "b".to_string(),
        "still running in the background...".to_string(),
    );
    let (reaped, _) = reap_runs(
        &mut runs,
        &BTreeMap::new(),
        &logs,
        &BTreeMap::new(),
        &BTreeMap::new(),
        1000,
        &cfg,
    );
    assert_eq!(reaped, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(runs["a"].status, RunStatus::Done);
    assert_eq!(runs["b"].status, RunStatus::Unfinished);
}

#[test]
fn parity_150_to_152_reap_attributes_stated_guessed_and_no_pr() {
    let cfg = test_cfg();
    let mut runs = BTreeMap::new();
    let mut r_stated = make_run(RunStatus::Done, "t1");
    r_stated.summary = "pushed alpha\nPR: #854".to_string();
    let mut r_guessed = make_run(RunStatus::Done, "t2");
    r_guessed.summary = "PR #2 is up".to_string();
    r_guessed.branches = vec!["alpha".to_string()];
    let mut r_none = make_run(RunStatus::Done, "t3");
    r_none.summary = "Nothing to change.\nPR: none".to_string();

    runs.insert("s".to_string(), r_stated);
    runs.insert("g".to_string(), r_guessed);
    runs.insert("n".to_string(), r_none);

    let mut branches = BTreeMap::new();
    branches.insert(854, "stated-branch".to_string());
    let forge = vec![ForgePr {
        number: 2,
        branch: "alpha".to_string(),
        owner: "fork".to_string(),
        state: PrState::Open,
        updated: 200,
    }];
    record_prs(&mut runs, &forge, &branches, &cfg);
    assert_eq!(
        runs["s"].pr,
        PrRecord::Found(FoundPr {
            number: 854,
            branch: "stated-branch".to_string(),
            how: PrHow::Stated,
        })
    );
    assert_eq!(
        runs["g"].pr,
        PrRecord::Found(FoundPr {
            number: 2,
            branch: "alpha".to_string(),
            how: PrHow::Guessed,
        })
    );
    assert_eq!(runs["n"].pr, PrRecord::NoPr { stated: true });
}
