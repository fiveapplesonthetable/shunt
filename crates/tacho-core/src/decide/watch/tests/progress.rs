//! Parity cases 1–29: Progress observation, CPU sampling, verdict, notes, and chain table.

use std::collections::BTreeMap;

use crate::decide::watch::activity::{compute_activity, has_took_line, Activity};
use crate::decide::watch::chains::{
    agent_row, chain_rows, format_span, health_line, hourly_health_line, rows, table, ChainBatch,
    ChainTodoItem, PrHow, PrRef, PrStanding, RunRecord, RunStatus,
};
use crate::decide::watch::config::WatchConfig;
use crate::decide::watch::notes::{
    current_phase, parse_note, phase_is_long, with_note, Current, Note, NoteAt, Phase,
};
use crate::decide::watch::procs::{below, inside, tree_cpu, Proc, ProcId};
use crate::decide::watch::sample::{take_sample, Sample};
use crate::decide::watch::verdict::{agent_next_deadline, last_work, verdict, Verdict};

fn cfg() -> WatchConfig {
    WatchConfig::default()
}

fn proc_row(pid: u32, ppid: u32, cpu: f64, cwd: &str) -> Proc {
    Proc {
        id: ProcId {
            pid,
            start_ticks: 100,
        },
        ppid,
        pgid: pid,
        name: "cargo".to_string(),
        age: 60,
        cpu,
        cwd: cwd.to_string(),
        starved: false,
        argv: vec!["cargo".to_string(), "test".to_string()],
        cgroup: String::new(),
    }
}

fn base_run(started: i64, status: RunStatus) -> RunRecord {
    RunRecord {
        status,
        started,
        finished: None,
        pid: Some(100),
        worktree: "/wt/a1".to_string(),
        base_sha: "abc".to_string(),
        task: "do work".to_string(),
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
fn case_01_below_returns_root_and_descendants() {
    let table = vec![
        proc_row(10, 1, 1.0, "/wt/a"),
        proc_row(20, 10, 2.0, "/wt/a"),
        proc_row(30, 20, 3.0, "/wt/a"),
        proc_row(99, 1, 9.0, "/wt/b"),
    ];
    let mut pids: Vec<u32> = below(&table, 10).into_iter().map(Proc::pid).collect();
    pids.sort_unstable();
    assert_eq!(pids, vec![10, 20, 30]);
}

#[test]
fn case_02_tree_cpu_sums_only_processes_under_root() {
    let table = vec![
        proc_row(10, 1, 1.5, "/wt/a"),
        proc_row(20, 10, 2.5, "/wt/a"),
        proc_row(30, 20, 3.0, "/wt/a"),
        proc_row(99, 1, 50.0, "/wt/b"),
    ];
    assert!((tree_cpu(&table, 10) - 7.0).abs() < 1e-9);
    assert!((tree_cpu(&table, 999) - 0.0).abs() < 1e-9);
}

#[test]
fn case_03_inside_checks_directory_or_subdirectory_boundary() {
    assert!(inside("/work/wt-1", "/work/wt-1"));
    assert!(inside("/work/wt-1/src/lib.rs", "/work/wt-1"));
    assert!(!inside("/work/wt-10", "/work/wt-1"));
    assert!(!inside("", "/work/wt-1"));
    assert!(!inside("/work/wt-1", ""));
}

#[test]
fn case_04_take_sample_first_observation_records_cpu_at_now() {
    let c = cfg();
    let s = take_sample(None, 12.5, 1_000, &c);
    assert_eq!(
        s,
        Sample {
            cpu: 12.5,
            at: 1_000,
            alive_at: 1_000,
            busy_at: 1_000,
        }
    );
}

#[test]
fn case_05_take_sample_rising_cpu_advances_alive_and_busy() {
    let c = cfg();
    let prev = Sample::first(10.0, 1_000);
    let s = take_sample(Some(&prev), 16.0, 1_060, &c);
    assert_eq!(
        s,
        Sample {
            cpu: 16.0,
            at: 1_060,
            alive_at: 1_060,
            busy_at: 1_060,
        }
    );
}

#[test]
fn case_06_take_sample_trickle_cpu_advances_alive_but_not_busy() {
    let c = cfg();
    let prev = Sample {
        cpu: 10.0,
        at: 1_000,
        alive_at: 900,
        busy_at: 900,
    };
    let s = take_sample(Some(&prev), 10.5, 1_060, &c);
    assert_eq!(
        s,
        Sample {
            cpu: 10.5,
            at: 1_060,
            alive_at: 1_060,
            busy_at: 900,
        }
    );
}

#[test]
fn case_07_take_sample_flat_cpu_keeps_alive_and_busy() {
    let c = cfg();
    let prev = Sample {
        cpu: 50.0,
        at: 1_000,
        alive_at: 900,
        busy_at: 850,
    };
    let s = take_sample(Some(&prev), 50.0, 1_060, &c);
    assert_eq!(
        s,
        Sample {
            cpu: 50.0,
            at: 1_060,
            alive_at: 900,
            busy_at: 850,
        }
    );
}

#[test]
fn case_08_last_work_takes_max_of_commit_change_and_started() {
    let act = Activity {
        commits: 1,
        files: 2,
        last_commit: Some(1_500),
        last_change: Some(2_000),
        last_test: Some(9_000),
    };
    assert_eq!(last_work(&act, 1_000), 2_000);
    assert_eq!(last_work(&Activity::quiet(), 1_000), 1_000);
}

#[test]
fn case_09_verdict_moving_when_recent_worktree_edit() {
    let c = cfg();
    let act = Activity {
        commits: 0,
        files: 1,
        last_commit: None,
        last_change: Some(2_900),
        last_test: None,
    };
    let sample = Sample {
        cpu: 10.0,
        at: 3_000,
        alive_at: 100,
        busy_at: 100,
    };
    assert_eq!(verdict(&act, &sample, 100, 3_000, &c), Verdict::Moving);
}

#[test]
fn case_10_verdict_moving_when_cpu_is_busy_even_without_file_writes() {
    let c = cfg();
    let act = Activity::quiet();
    let sample = Sample {
        cpu: 25.0,
        at: 5_000,
        alive_at: 4_950,
        busy_at: 4_950,
    };
    assert_eq!(verdict(&act, &sample, 100, 5_000, &c), Verdict::Moving);
}

#[test]
fn case_11_verdict_hung_after_30m_no_edit_and_flat_cpu() {
    let c = cfg();
    let act = Activity::quiet();
    let sample = Sample {
        cpu: 5.0,
        at: 1_900,
        alive_at: 100,
        busy_at: 100,
    };
    assert_eq!(verdict(&act, &sample, 100, 1_900, &c), Verdict::Hung);
}

#[test]
fn case_12_verdict_breathing_cpu_prevents_hung_at_30m() {
    let c = cfg();
    let act = Activity::quiet();
    let sample = Sample {
        cpu: 5.1,
        at: 1_900,
        alive_at: 1_850,
        busy_at: 100,
    };
    assert_eq!(verdict(&act, &sample, 100, 1_900, &c), Verdict::Moving);
}

#[test]
fn case_13_verdict_stalled_after_45m_no_work_edit_and_idle_cpu() {
    let c = cfg();
    let act = Activity::quiet();
    let sample = Sample {
        cpu: 5.5,
        at: 2_800,
        alive_at: 2_750,
        busy_at: 100,
    };
    assert_eq!(verdict(&act, &sample, 100, 2_800, &c), Verdict::Stalled);
}

#[test]
fn case_14_verdict_hung_checked_before_stalled_when_both_thresholds_pass() {
    let c = cfg();
    let act = Activity::quiet();
    let sample = Sample {
        cpu: 5.0,
        at: 3_000,
        alive_at: 100,
        busy_at: 100,
    };
    assert_eq!(verdict(&act, &sample, 100, 3_000, &c), Verdict::Hung);
    assert_eq!(
        agent_next_deadline(&act, &Sample::first(1.0, 100), 100, 200, &c),
        Some(100 + c.hung_after)
    );
}

#[test]
fn case_15_compute_activity_and_has_took_line() {
    assert!(has_took_line(b"all tests passed, took 14s\n"));
    assert!(!has_took_line(b"running tests...\n"));
    let mut mtimes = BTreeMap::new();
    mtimes.insert("src/lib.rs".to_string(), 2_000);
    mtimes.insert("src/new.rs".to_string(), 2_500);
    let act = compute_activity(
        true,
        &[1_800, 1_200],
        &["src/lib.rs".to_string()],
        &["src/new.rs".to_string()],
        &mtimes,
        Some(2_100),
        &[(2_400, b"done: took 9s\n".to_vec())],
    );
    assert_eq!(act.commits, 2);
    assert_eq!(act.files, 2);
    assert_eq!(act.last_commit, Some(1_800));
    assert_eq!(act.last_change, Some(2_500));
    assert_eq!(act.last_test, Some(2_400));
    assert_eq!(
        compute_activity(false, &[1], &[], &[], &mtimes, None, &[]),
        Activity::quiet()
    );
}

#[test]
fn case_16_parse_note_validates_known_phase_and_rejects_bad_input() {
    let c = cfg();
    assert_eq!(
        parse_note("test: running full cargo test", &c),
        Some(Note {
            phase: Phase::Test,
            phase_name: "test".to_string(),
            line: "running full cargo test".to_string(),
        })
    );
    assert_eq!(parse_note("unknown: something", &c), None);
    assert_eq!(parse_note("no colon here", &c), None);
    assert_eq!(parse_note("read:   ", &c), None);
    assert_eq!(parse_note("read: line1\nline2", &c), None);
}

#[test]
fn case_17_with_note_caps_history_at_notes_kept() {
    let c = cfg();
    let mut history: Vec<NoteAt> = Vec::new();
    for i in 0..55 {
        let note = parse_note(&format!("write: step {i}"), &c).unwrap();
        history = with_note(&history, note, 1_000 + i, &c);
    }
    assert_eq!(history.len(), 50);
    assert_eq!(history[0].line, "step 5");
    assert_eq!(history[49].line, "step 54");
}

#[test]
fn case_18_current_phase_measures_since_start_of_unbroken_phase_streak() {
    let history = vec![
        NoteAt::new(100, "read", "reading spec"),
        NoteAt::new(200, "write", "first module"),
        NoteAt::new(350, "write", "second module"),
    ];
    assert_eq!(
        current_phase(&history),
        Some(Current {
            phase: "write".to_string(),
            since: 200,
            line: "second module".to_string(),
        })
    );
}

#[test]
fn case_19_phase_is_long_checks_strictly_over_phase_long() {
    let c = cfg();
    let cur = Current {
        phase: "read".to_string(),
        since: 1_000,
        line: "still reading".to_string(),
    };
    assert!(!phase_is_long(Some(&cur), 1_000 + c.phase_long, &c));
    assert!(phase_is_long(Some(&cur), 1_000 + c.phase_long + 1, &c));
}

#[test]
fn case_20_agent_row_formats_verdict_and_long_phase_flag() {
    let c = cfg();
    let sample = Sample::first(120.0, 9_000);
    let act = Activity {
        commits: 2,
        files: 3,
        last_commit: Some(8_500),
        last_change: Some(8_900),
        last_test: None,
    };
    let notes = vec![NoteAt::new(1_000, "read", "reading crate")];
    let row = agent_row("task-a", "running", &act, &sample, 1_000, &notes, 9_000, &c);
    assert_eq!(row.state, "moving");
    assert_eq!(row.phase, "read");
    assert!(row.phase_long);
    assert_eq!(row.note, "reading crate");
}

#[test]
fn case_21_rows_filters_out_runs_older_than_row_window() {
    let c = cfg();
    let mut agents = BTreeMap::new();
    let mut r_old = base_run(100, RunStatus::Done);
    r_old.finished = Some(200);
    let mut r_recent = base_run(100_000, RunStatus::Done);
    r_recent.finished = Some(100_100);
    agents.insert("old-task".to_string(), r_old);
    agents.insert("recent-task".to_string(), r_recent);

    let out = rows(
        &agents,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        110_000,
        &c,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, "recent-task");
}

#[test]
fn case_22_chain_rows_groups_by_area_and_computes_medians() {
    let todo = vec![
        ChainTodoItem {
            key: "t1".to_string(),
            area: "auth".to_string(),
            ..Default::default()
        },
        ChainTodoItem {
            key: "t2".to_string(),
            area: "auth".to_string(),
            ..Default::default()
        },
    ];
    let mut agents = BTreeMap::new();
    let mut r1 = base_run(1_000, RunStatus::Done);
    r1.finished = Some(1_600);
    r1.pr = Some(PrRef {
        number: 10,
        branch: "b1".to_string(),
        how: PrHow::Stated,
    });
    agents.insert("t1".to_string(), r1);

    let mut standing = BTreeMap::new();
    standing.insert(10, PrStanding::Merged);
    let mut merged_at = BTreeMap::new();
    merged_at.insert(10, 2_200);
    let mut batches = BTreeMap::new();
    batches.insert(
        "b-1".to_string(),
        ChainBatch {
            started: 1_700,
            finished: Some(2_000),
            status: "passed".to_string(),
        },
    );
    let mut passed_of = BTreeMap::new();
    passed_of.insert(10, "b-1".to_string());

    let crows = chain_rows(
        &todo, &agents, &batches, &passed_of, &merged_at, &standing, 3_000,
    );
    assert_eq!(crows.len(), 1);
    assert_eq!(crows[0].area, "auth");
    assert_eq!(crows[0].run_median, Some(600.0));
    assert_eq!(crows[0].batch_median, Some(300.0));
    assert_eq!(crows[0].ready_median, Some(200.0));
    assert_eq!(crows[0].head.as_deref(), Some("t2"));
}

#[test]
fn case_23_chain_rows_computes_owner_and_machine_wait() {
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "core".to_string(),
        ..Default::default()
    }];
    let mut agents = BTreeMap::new();
    let mut r1 = base_run(1_000, RunStatus::Done);
    r1.finished = Some(1_200);
    r1.pr = Some(PrRef::stated(11, "b11"));
    agents.insert("t1".to_string(), r1);

    let mut batches = BTreeMap::new();
    batches.insert(
        "b1".to_string(),
        ChainBatch {
            started: 1_300,
            finished: Some(1_500),
            status: "passed".to_string(),
        },
    );
    let mut passed_of = BTreeMap::new();
    passed_of.insert(11, "b1".to_string());
    let mut standing = BTreeMap::new();
    standing.insert(11, PrStanding::Passed);

    let crows = chain_rows(
        &todo,
        &agents,
        &batches,
        &passed_of,
        &BTreeMap::new(),
        &standing,
        2_000,
    );
    assert_eq!(crows[0].machine_wait, 100);
    assert_eq!(crows[0].owner_wait, 500);
}

#[test]
fn case_24_chain_rows_tracks_head_and_head_for() {
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "net".to_string(),
        ..Default::default()
    }];
    let mut agents = BTreeMap::new();
    agents.insert("t1".to_string(), base_run(1_000, RunStatus::Running));
    let crows = chain_rows(
        &todo,
        &agents,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        1_600,
    );
    assert_eq!(crows[0].head.as_deref(), Some("t1"));
    assert_eq!(crows[0].head_for, Some(600));
}

#[test]
fn case_25_chain_rows_reports_none_head_when_all_merged() {
    let todo = vec![ChainTodoItem {
        key: "t1".to_string(),
        area: "net".to_string(),
        ..Default::default()
    }];
    let mut agents = BTreeMap::new();
    let mut r1 = base_run(1_000, RunStatus::Done);
    r1.finished = Some(1_500);
    r1.no_change = true;
    agents.insert("t1".to_string(), r1);

    let crows = chain_rows(
        &todo,
        &agents,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        2_000,
    );
    assert_eq!(crows[0].head, None);
}

#[test]
fn case_26_format_span_formats_none_minutes_and_hours() {
    assert_eq!(format_span(None), "-");
    assert_eq!(format_span(Some(45.0)), "0 min");
    assert_eq!(format_span(Some(120.0)), "2 min");
    assert_eq!(format_span(Some(7_200.0)), "2 h");
}

#[test]
fn case_27_health_line_and_hourly_health_line_with_cpu() {
    let c = cfg();
    assert_eq!(
        health_line(&[], &[]),
        "progress: no agents running; no chains"
    );
    let line = hourly_health_line(Some(42.0), 3.25, 180, 2, 1, &[], &[], &c);
    assert_eq!(
        line,
        "health: cpu 42% of 80%, 180 GB free, 2 agents running, 1 stuck; progress: no agents running; no chains"
    );
}

#[test]
fn case_28_hourly_health_line_falls_back_to_load_when_cpu_none() {
    let c = cfg();
    let line = hourly_health_line(None, 2.0, 95, 0, 0, &[], &[], &c);
    assert!(line.starts_with("health: load 2, 95 GB free, 0 agents running, 0 stuck;"));
}

#[test]
fn case_29_table_renders_agents_long_phase_banner_and_chains() {
    let c = cfg();
    let sample = Sample::first(120.0, 9_000);
    let notes = vec![NoteAt::new(1_000, "read", "deep dive")];
    let row = agent_row(
        "t1",
        "running",
        &Activity::quiet(),
        &sample,
        1_000,
        &notes,
        9_000,
        &c,
    );
    let rendered = table(&[row], &[]);
    assert!(rendered.contains("AGENT"));
    assert!(rendered.contains("over 2 h in read: deep dive"));
    assert!(rendered.contains("CHAIN"));
}
