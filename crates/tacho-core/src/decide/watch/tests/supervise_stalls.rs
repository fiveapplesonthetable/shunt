//! Parity cases 30–45: Supervision interventions (stalls, hangs, nudges, resumes, burners).

use crate::decide::watch::activity::Activity;
use crate::decide::watch::chains::{PrHow, PrRef, RunRecord, RunStatus};
use crate::decide::watch::config::WatchConfig;
use crate::decide::watch::notes::NoteAt;
use crate::decide::watch::procs::{Proc, ProcId};
use crate::decide::watch::sample::Sample;
use crate::decide::watch::supervise::{
    burner_notice, decide_owner_mail, decide_watch, needs_resume, nudged_before, resume_brief,
    resume_ready_agents, stalled_sentence, step, Step, WatchAction, WatchObs, WatchState,
};
use crate::decide::watch::verdict::Verdict;

fn cfg() -> WatchConfig {
    WatchConfig::default()
}

fn proc_row(pid: u32, ppid: u32, pgid: u32, cpu: f64, age: i64, name: &str, cwd: &str) -> Proc {
    Proc {
        id: ProcId {
            pid,
            start_ticks: 100,
        },
        ppid,
        pgid,
        name: name.to_string(),
        age,
        cpu,
        cwd: cwd.to_string(),
        starved: false,
        argv: vec![name.to_string(), "--run".to_string()],
        cgroup: String::new(),
    }
}

fn running_agent(started: i64, pid: u32, wt: &str) -> RunRecord {
    RunRecord {
        status: RunStatus::Running,
        started,
        finished: None,
        pid: Some(pid),
        worktree: wt.to_string(),
        base_sha: "deadbeef".to_string(),
        task: "implement feature".to_string(),
        model: "claude".to_string(),
        pr: None,
        pr_checked: false,
        stated_no_pr: false,
        no_change: false,
        nudge: false,
        stop_reason: String::new(),
        summary: String::new(),
    }
}

#[test]
fn case_30_step_maps_verdict_and_nudge_history() {
    assert_eq!(step(Verdict::Moving, false), Step::None);
    assert_eq!(step(Verdict::Moving, true), Step::None);
    assert_eq!(step(Verdict::Hung, false), Step::Resume);
    assert_eq!(step(Verdict::Hung, true), Step::Resume);
    assert_eq!(step(Verdict::Stalled, false), Step::Nudge);
    assert_eq!(step(Verdict::Stalled, true), Step::Email);
}

#[test]
fn case_31_first_stall_stops_group_and_schedules_immediate_resume_with_question() {
    let c = cfg();
    let mut st = WatchState::default();
    st.agents
        .insert("task1".to_string(), running_agent(100, 500, "/wt/task1"));
    st.samples.insert(
        "task1".to_string(),
        Sample {
            cpu: 10.0,
            at: 2_800,
            alive_at: 2_750,
            busy_at: 100,
        },
    );
    let mut obs = WatchObs {
        now: 2_900,
        procs: vec![proc_row(500, 1, 500, 10.5, 2_800, "claude", "/wt/task1")],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    obs.live_pids.insert(500);
    obs.activities
        .insert("/wt/task1".to_string(), Activity::quiet());

    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.actions.len(), 1);
    match &plan.actions[0] {
        WatchAction::StopAndResume {
            key,
            backoff,
            question,
            stop_reason,
            ..
        } => {
            assert_eq!(key, "task1");
            assert!(!backoff);
            assert!(*question);
            assert_eq!(
                stop_reason,
                "stopped: nothing written for 45 minutes with the CPU idle"
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
    let a = &plan.next_state.agents["task1"];
    assert_eq!(a.status, RunStatus::Unfinished);
    assert!(a.nudge);

    let mut next_st = plan.next_state;
    let resumed = resume_ready_agents(&mut next_st, 2_900, &c);
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].0, "task1~2");
    assert!(resumed[0]
        .1
        .task
        .contains("Say where you are and what is blocking"));
}

#[test]
fn case_32_second_stall_after_nudge_escalates_to_needs_person_without_killing() {
    let c = cfg();
    let mut st = WatchState::default();
    let mut r1 = running_agent(100, 500, "/wt/task1");
    r1.status = RunStatus::Unfinished;
    r1.finished = Some(2_900);
    r1.nudge = true;
    st.agents.insert("task1".to_string(), r1);
    st.agents.insert(
        "task1~2".to_string(),
        running_agent(2_900, 502, "/wt/task1"),
    );
    st.samples.insert(
        "task1~2".to_string(),
        Sample {
            cpu: 8.0,
            at: 5_600,
            alive_at: 5_550,
            busy_at: 2_900,
        },
    );
    st.notes.insert(
        "task1".to_string(),
        vec![NoteAt::new(3_000, "read", "waiting on credentials")],
    );

    let mut obs = WatchObs {
        now: 5_700,
        procs: vec![proc_row(502, 1, 502, 8.2, 2_800, "claude", "/wt/task1")],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    obs.live_pids.insert(502);

    assert!(nudged_before("task1~2", &st.agents));
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.next_state.agents["task1~2"].status, RunStatus::Running);
    assert!(plan.next_state.needs_person.contains_key("stalled-task1~2"));
    let mut next_st = plan.next_state;
    let mails = decide_owner_mail(&mut next_st, &obs, false, &c);
    assert_eq!(mails.len(), 1);
    let mails_again = decide_owner_mail(&mut next_st, &obs, false, &c);
    assert!(mails_again.is_empty());
}

#[test]
fn case_33_agent_recovery_clears_stalled_need() {
    let c = cfg();
    let mut st = WatchState::default();
    st.agents.insert(
        "task1~2".to_string(),
        running_agent(2_900, 502, "/wt/task1"),
    );
    st.needs_person.insert(
        "stalled-task1~2".to_string(),
        "previously stalled".to_string(),
    );
    let mut obs = WatchObs {
        now: 3_000,
        procs: vec![proc_row(502, 1, 502, 20.0, 100, "claude", "/wt/task1")],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    obs.live_pids.insert(502);

    let plan = decide_watch(&st, &obs, &c);
    assert!(!plan.next_state.needs_person.contains_key("stalled-task1~2"));
    assert!(plan.actions.iter().any(|a| matches!(
        a,
        WatchAction::ClearNeed { need_key, .. } if need_key == "stalled-task1~2"
    )));
}

#[test]
fn case_34_hung_agent_stops_and_waits_for_resume_backoff() {
    let c = cfg();
    let mut st = WatchState::default();
    st.agents
        .insert("task1".to_string(), running_agent(100, 500, "/wt/task1"));
    st.samples.insert(
        "task1".to_string(),
        Sample {
            cpu: 5.0,
            at: 1_900,
            alive_at: 100,
            busy_at: 100,
        },
    );
    let mut obs = WatchObs {
        now: 1_950,
        procs: vec![proc_row(500, 1, 500, 5.0, 1_850, "claude", "/wt/task1")],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    obs.live_pids.insert(500);

    let plan = decide_watch(&st, &obs, &c);
    let mut next_st = plan.next_state;
    assert_eq!(next_st.agents["task1"].status, RunStatus::Unfinished);
    assert!(!next_st.agents["task1"].nudge);

    assert!(resume_ready_agents(&mut next_st, 1_950, &c).is_empty());
    assert!(resume_ready_agents(&mut next_st, 1_950 + c.resume_backoff - 1, &c).is_empty());
    let resumed = resume_ready_agents(&mut next_st, 1_950 + c.resume_backoff, &c);
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].0, "task1~2");
}

#[test]
fn case_35_max_runs_exhausted_is_not_resumed() {
    let c = cfg();
    let mut st = WatchState::default();
    let mut r1 = running_agent(100, 500, "/wt/task1");
    r1.status = RunStatus::Unfinished;
    r1.finished = Some(150);
    st.agents.insert("task1".to_string(), r1);
    let mut r2 = running_agent(200, 501, "/wt/task1");
    r2.status = RunStatus::Unfinished;
    r2.finished = Some(300);
    r2.nudge = true;
    st.agents.insert("task1~2".to_string(), r2);

    assert!(!needs_resume(
        &st.agents["task1~2"],
        &st.agents,
        "task1~2",
        1_000,
        &c
    ));
    assert!(resume_ready_agents(&mut st, 1_000, &c).is_empty());
}

#[test]
fn case_36_resume_brief_differs_for_nudge_vs_hung_and_truncates_lists() {
    let c = cfg();
    let commits: Vec<String> = (1..=12).map(|i| format!("c{i} commit {i}")).collect();
    let changes: Vec<String> = (1..=35).map(|i| format!("M file_{i}.rs")).collect();

    let nudge_text = resume_brief("Fix bug", "/wt/t1", &commits, &changes, true, &c);
    assert!(nudge_text.contains("nothing in its worktree changed for 45 minutes"));
    assert!(nudge_text.contains("  and 2 more"));
    assert!(nudge_text.contains("  and 5 more"));

    let hung_text = resume_brief("Fix bug", "/wt/t1", &[], &[], false, &c);
    assert!(hung_text.contains("ended without its final report"));
    assert!(hung_text.contains("  (no commits)"));
    assert!(hung_text.contains("  (clean)"));
}

#[test]
fn case_37_stalled_sentence_includes_worktree_and_last_note_or_no_note() {
    let c = cfg();
    let no_notes = stalled_sentence("t1~2", "/wt/t1", &[], &c);
    assert!(no_notes.contains("Worktree: /wt/t1. It has left no note."));

    let notes = vec![NoteAt::new(500, "test", "waiting on db")];
    let with_notes = stalled_sentence("t1~2", "/wt/t1", &notes, &c);
    assert!(with_notes.contains("Its last note: test: waiting on db"));
}

#[test]
fn case_38_non_group_leader_agent_is_never_signaled() {
    let c = cfg();
    let mut st = WatchState::default();
    st.agents
        .insert("task1".to_string(), running_agent(100, 500, "/wt/task1"));
    st.samples.insert(
        "task1".to_string(),
        Sample {
            cpu: 5.0,
            at: 1_900,
            alive_at: 100,
            busy_at: 100,
        },
    );
    let mut obs = WatchObs {
        now: 1_950,
        procs: vec![proc_row(500, 1, 1, 5.0, 1_850, "claude", "/wt/task1")],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    obs.live_pids.insert(500);

    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.actions.is_empty());
    assert_eq!(plan.next_state.agents["task1"].status, RunStatus::Running);
}

#[test]
fn case_39_rogue_burner_under_agent_tree_is_stopped_with_notice_and_pr_comment() {
    let c = cfg();
    let mut st = WatchState::default();
    let mut a = running_agent(100, 500, "/wt/task1");
    a.pr = Some(PrRef {
        number: 77,
        branch: "b77".to_string(),
        how: PrHow::Stated,
    });
    st.agents.insert("task1".to_string(), a);

    let obs = WatchObs {
        now: 2_000,
        procs: vec![
            proc_row(500, 1, 500, 1.0, 1_000, "claude", "/wt/task1"),
            proc_row(600, 500, 600, 550.0, 660, "yes", "/wt/task1"),
        ],
        work_root: "/wt".to_string(),
        ..Default::default()
    };

    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.actions.len(), 1);
    match &plan.actions[0] {
        WatchAction::KillBurner {
            id,
            name,
            owner,
            worktree_notice,
            pr_comment,
            ..
        } => {
            assert_eq!(id.pid, 600);
            assert_eq!(name, "yes");
            assert_eq!(owner.as_deref(), Some("task1"));
            assert_eq!(
                worktree_notice.as_ref().map(|(wt, _)| wt.as_str()),
                Some("/wt/task1")
            );
            assert_eq!(pr_comment.as_ref().map(|(n, _)| *n), Some(77));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(burner_notice(1, &c).contains("stopped 1 CPU burner"));
}

#[test]
fn case_40_orphan_burner_in_work_root_or_agent_cgroup_is_stopped() {
    let c = cfg();
    let st = WatchState::default();
    let mut b_cgroup = proc_row(701, 1, 701, 500.0, 700, "stress", "/tmp");
    b_cgroup.cgroup = "/user.slice/agent-1.slice/run-u42.scope".to_string();
    let b_cwd = proc_row(702, 1, 702, 500.0, 700, "yes", "/wt/orphan");

    let obs = WatchObs {
        now: 2_000,
        procs: vec![b_cgroup, b_cwd],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.actions.len(), 2);
}

#[test]
fn case_41_unrelated_host_burner_outside_agent_trees_work_root_and_cgroups_is_ignored() {
    let c = cfg();
    let st = WatchState::default();
    let external = proc_row(800, 1, 800, 900.0, 1_200, "yes", "/home/alice");
    let obs = WatchObs {
        now: 2_000,
        procs: vec![external],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.actions.is_empty());
}

#[test]
fn case_42_young_burner_under_limit_is_not_stopped_yet() {
    let c = cfg();
    let st = WatchState::default();
    let young = proc_row(801, 1, 801, 500.0, 599, "yes", "/wt/task1");
    let obs = WatchObs {
        now: 2_000,
        procs: vec![young],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.actions.is_empty());
}

#[test]
fn case_43_timeout_wrapper_command_unwraps_inner_burner_binary() {
    let c = cfg();
    let st = WatchState::default();
    let mut wrapped = proc_row(802, 1, 802, 550.0, 700, "timeout", "/wt/task1");
    wrapped.argv = vec![
        "timeout".to_string(),
        "-s".to_string(),
        "900".to_string(),
        "/usr/bin/yes".to_string(),
    ];
    let obs = WatchObs {
        now: 2_000,
        procs: vec![wrapped],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert_eq!(plan.actions.len(), 1);
}

#[test]
fn case_44_starved_burner_is_never_stopped_by_watch() {
    let c = cfg();
    let st = WatchState::default();
    let mut starved_yes = proc_row(803, 1, 803, 550.0, 700, "yes", "/wt/task1");
    starved_yes.starved = true;
    let obs = WatchObs {
        now: 2_000,
        procs: vec![starved_yes],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.actions.is_empty());
}

#[test]
fn case_45_normal_compiler_process_is_not_classified_as_burner() {
    let c = cfg();
    let st = WatchState::default();
    let rustc = proc_row(804, 1, 804, 550.0, 700, "rustc", "/wt/task1");
    let obs = WatchObs {
        now: 2_000,
        procs: vec![rustc],
        work_root: "/wt".to_string(),
        ..Default::default()
    };
    let plan = decide_watch(&st, &obs, &c);
    assert!(plan.actions.is_empty());
}
