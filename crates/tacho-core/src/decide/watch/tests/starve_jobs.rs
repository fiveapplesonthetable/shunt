//! Parity cases 60–87: `starve`, process safety (`parse_proc_stat`, `owns`, `identity`, `leads_group`),
//! and named worktree jobs (`valid_job_name`, `job_start_check`, `wait_for`, `job_wait`, `kill_group`, `job_kill`).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;

use crate::decide::watch::config::{StarveConfig, WatchConfig};
use crate::decide::watch::jobs::{
    job_kill, job_name_problem, job_start_check, job_wait, kill_group, valid_job_name, wait_for,
    WAIT_BEAT_SECS, WAIT_LIMIT_SECS,
};
use crate::decide::watch::procs::{
    group_is_alive, identity, in_agent_cgroup, leads_group, owns, parse_proc_stat, stat_is_alive,
    Proc, ProcId,
};
use crate::decide::watch::starve::{
    bounded_call, count_cpus, format_cpu_ids, plan_starve, run_starve, should_stop_burner,
    valid_cpu_list, within_affinity, StarveCallError,
};

fn scfg() -> StarveConfig {
    StarveConfig::default()
}

fn wcfg() -> WatchConfig {
    WatchConfig::default()
}

#[test]
fn case_60_format_cpu_ids_renders_compact_ranges_and_singles() {
    assert_eq!(format_cpu_ids(&[0, 1, 2, 3, 8]), "0-3,8");
    assert_eq!(format_cpu_ids(&[]), "");
}

#[test]
fn case_61_valid_cpu_list_accepts_singles_and_ascending_ranges() {
    assert!(valid_cpu_list("0"));
    assert!(valid_cpu_list("0-3"));
    assert!(valid_cpu_list("0,2-4,7"));
    assert!(!valid_cpu_list(""));
    assert!(!valid_cpu_list("3-1"));
    assert!(!valid_cpu_list("0--2"));
    assert!(!valid_cpu_list("abc"));
}

#[test]
fn case_62_count_cpus_counts_ids_in_validated_list() {
    assert_eq!(count_cpus("0-3,8"), 5);
}

#[test]
fn case_63_within_affinity_checks_every_cpu_is_available() {
    assert!(within_affinity("2-3,5", &[1, 2, 3, 4, 5]));
    assert!(!within_affinity("0-3", &[2, 3, 4, 5]));
}

#[test]
fn case_64_plan_starve_rejects_zero_size() {
    let p = plan_starve(32, 0, None, true, false, None, &scfg());
    assert!(p.problem.contains("at least one CPU"));
}

#[test]
fn case_65_plan_starve_rejects_invalid_cpus_spec() {
    let p = plan_starve(32, 4, Some("bad"), true, false, None, &scfg());
    assert!(p.problem.contains("is not a CPU list"));
}

#[test]
fn case_66_plan_starve_rejects_cpus_exceeding_one_eighth_limit() {
    let p = plan_starve(32, 4, Some("0-7"), true, false, None, &scfg());
    assert!(p.problem.contains("exceeds the starve limit of 4 CPUs"));
}

#[test]
fn case_67_plan_starve_picks_highest_numbered_cpus_with_taskset() {
    let p = plan_starve(32, 4, None, true, true, None, &scfg());
    assert!(p.problem.is_empty());
    assert_eq!(p.cpus, "28-31");
    assert_eq!(p.burners, 16);
    assert_eq!(
        p.wrap,
        Some(vec![
            "taskset".to_string(),
            "-c".to_string(),
            "28-31".to_string()
        ])
    );
}

#[test]
fn case_68_plan_starve_falls_back_to_systemd_run_scope_when_no_taskset() {
    let p = plan_starve(32, 4, None, false, true, None, &scfg());
    assert!(p.problem.is_empty());
    assert_eq!(p.cpus, "28-31");
    assert!(p
        .wrap
        .as_ref()
        .unwrap()
        .contains(&"AllowedCPUs=28-31".to_string()));
}

#[test]
fn case_69_plan_starve_fails_closed_when_neither_taskset_nor_scope_available() {
    let p = plan_starve(32, 4, None, false, false, None, &scfg());
    assert!(p.wrap.is_none());
    assert!(p
        .problem
        .contains("neither taskset nor a systemd user scope"));
}

#[test]
fn case_70_plan_starve_intersects_default_cpus_with_caller_affinity() {
    let allowed = vec![0, 1, 2, 3];
    let p = plan_starve(32, 2, None, true, false, Some(&allowed), &scfg());
    assert!(p.problem.is_empty());
    assert_eq!(p.cpus, "2-3");
}

#[test]
fn case_71_plan_starve_rejects_explicit_cpus_outside_affinity() {
    let allowed = vec![0, 1];
    let p = plan_starve(32, 2, Some("4-5"), true, false, Some(&allowed), &scfg());
    assert!(p
        .problem
        .contains("outside this process's available affinity"));
}

#[test]
fn case_72_run_starve_refuses_when_batch_is_running() {
    let cmd = vec!["cargo".to_string(), "test".to_string()];
    let plan = plan_starve(32, 2, None, true, false, None, &scfg());
    let mut messages = Vec::new();
    let rc = run_starve(
        &cmd,
        &plan,
        true,
        &BTreeMap::new(),
        &wcfg(),
        |_argv, _env| 100,
        |_argv, _env, _t| Ok(0),
        |_pid| {},
        |msg| messages.push(msg.to_string()),
    );
    assert_eq!(rc, 1);
    assert!(messages
        .iter()
        .any(|m| m.contains("refusing to starve while a merge-check batch is running")));
}

#[test]
fn case_73_run_starve_cleans_up_all_burners_on_normal_exit() {
    let cmd = vec!["cargo".to_string(), "test".to_string()];
    let plan = plan_starve(16, 1, None, true, false, None, &scfg());
    let next_pid = Cell::new(100u32);
    let mut stopped = Vec::new();
    let rc = run_starve(
        &cmd,
        &plan,
        false,
        &BTreeMap::new(),
        &wcfg(),
        |_argv, _env| {
            let p = next_pid.get();
            next_pid.set(p + 1);
            p
        },
        |_argv, env, _t| {
            assert_eq!(env.get("TACHO_STARVE").map(String::as_str), Some("1"));
            assert_eq!(env.get("STARVE").map(String::as_str), Some("1"));
            Ok(0)
        },
        |pid| stopped.push(pid),
        |_msg| {},
    );
    assert_eq!(rc, 0);
    assert_eq!(stopped, vec![100, 101, 102, 103]);
}

#[test]
fn case_74_run_starve_cleans_up_burners_and_returns_124_on_timeout() {
    let cmd = vec!["cargo".to_string(), "test".to_string()];
    let plan = plan_starve(16, 1, None, true, false, None, &scfg());
    let next_pid = Cell::new(200u32);
    let mut stopped = Vec::new();
    let mut messages = Vec::new();
    let rc = run_starve(
        &cmd,
        &plan,
        false,
        &BTreeMap::new(),
        &wcfg(),
        |_argv, _env| {
            let p = next_pid.get();
            next_pid.set(p + 1);
            p
        },
        |_argv, _env, _t| Err(StarveCallError::TimedOut),
        |pid| stopped.push(pid),
        |msg| messages.push(msg.to_string()),
    );
    assert_eq!(rc, 124);
    assert_eq!(stopped, vec![200, 201, 202, 203]);
    assert!(messages
        .iter()
        .any(|m| m.contains("exceeded the 300-second starve limit")));
}

#[test]
fn case_75_run_starve_cleans_up_burners_and_returns_127_on_start_error() {
    let cmd = vec!["missing-bin".to_string()];
    let plan = plan_starve(16, 1, None, true, false, None, &scfg());
    let next_pid = Cell::new(300u32);
    let mut stopped = Vec::new();
    let rc = run_starve(
        &cmd,
        &plan,
        false,
        &BTreeMap::new(),
        &wcfg(),
        |_argv, _env| {
            let p = next_pid.get();
            next_pid.set(p + 1);
            p
        },
        |_argv, _env, _t| Err(StarveCallError::CannotStart("No such file".to_string())),
        |pid| stopped.push(pid),
        |_msg| {},
    );
    assert_eq!(rc, 127);
    assert_eq!(stopped, vec![300, 301, 302, 303]);
}

#[test]
fn case_76_bounded_call_kills_process_group_with_now_true_on_timeout() {
    let mut killed = Vec::new();
    let mut waited = Vec::new();
    let res = bounded_call(
        444,
        30,
        |_t| Err(StarveCallError::TimedOut),
        |pid, now| killed.push((pid, now)),
        |pid| waited.push(pid),
    );
    assert_eq!(res, Err(StarveCallError::TimedOut));
    assert_eq!(killed, vec![(444, true)]);
    assert_eq!(waited, vec![444]);
}

#[test]
fn case_77_should_stop_burner_verifies_pgid_env_and_yes_cmdline() {
    let c = wcfg();
    let env = vec!["TACHO_STARVE=1".to_string()];
    let cmd = vec!["timeout".to_string(), "300".to_string(), "yes".to_string()];
    assert!(should_stop_burner(500, 500, &env, &cmd, &c));
    assert!(!should_stop_burner(500, 1, &env, &cmd, &c));
    assert!(!should_stop_burner(500, 500, &[], &cmd, &c));
    assert!(!should_stop_burner(
        500,
        500,
        &env,
        &["cargo".to_string()],
        &c
    ));
}

#[test]
fn case_78_parse_proc_stat_handles_spaces_parens_and_cutime_cstime() {
    let c = wcfg();
    let raw =
        "1234 (my (weird) prog) S 1 1234 1234 0 -1 4194304 100 0 0 0 150 50 25 25 20 0 1 0 1000 0 0\n";
    let parsed = parse_proc_stat(
        1234,
        raw,
        "/wt/a",
        b"TACHO_STARVE=1\0PATH=/bin\0",
        b"my\0--flag\0",
        "0::/user.slice/agent.slice\n",
        100,
        50.0,
        &c,
    )
    .unwrap();
    assert_eq!(parsed.pid(), 1234);
    assert_eq!(parsed.name, "my (weird) prog");
    assert_eq!(parsed.ppid, 1);
    assert_eq!(parsed.pgid, 1234);
    assert!((parsed.cpu - 2.5).abs() < 1e-9);
    assert_eq!(parsed.age, 40);
    assert!(parsed.starved);
    assert_eq!(parsed.id.start_ticks, 1000);
}

#[test]
fn case_79_stat_is_alive_treats_zombie_and_dead_as_not_alive() {
    let running = "10 (sh) R 1 10 10 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 500\n";
    let zombie = "10 (sh) Z 1 10 10 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 500\n";
    assert!(stat_is_alive(running));
    assert!(!stat_is_alive(zombie));
    assert!(!stat_is_alive("malformed"));
    assert!(group_is_alive(10, &[running]));
    assert!(!group_is_alive(10, &[zombie]));
}

#[test]
fn case_80_owns_requires_path_within_worktree_boundary() {
    assert!(owns("/wt/a", "/wt/a"));
    assert!(owns("/wt/a/sub", "/wt/a"));
    assert!(!owns("/wt/ab", "/wt/a"));
    assert!(!owns("", "/wt/a"));
}

#[test]
fn case_81_leads_group_and_identity_prevent_pid_reuse_collateral() {
    let id = ProcId {
        pid: 500,
        start_ticks: 12345,
    };
    let p_leader = Proc {
        id,
        ppid: 1,
        pgid: 500,
        name: "sh".to_string(),
        age: 10,
        cpu: 1.0,
        cwd: "/wt".to_string(),
        starved: false,
        argv: vec!["sh".to_string()],
        cgroup: String::new(),
    };
    assert_eq!(identity(std::slice::from_ref(&p_leader), 500), Some(id));
    assert!(leads_group(std::slice::from_ref(&p_leader), id));
    let reused = ProcId {
        pid: 500,
        start_ticks: 99999,
    };
    assert!(!leads_group(&[p_leader], reused));
}

#[test]
fn case_82_valid_job_name_and_job_start_check_enforce_constraints() {
    assert!(valid_job_name("cargo-test_1.0"));
    assert!(!valid_job_name(""));
    assert!(!valid_job_name("BadName"));
    assert!(!valid_job_name("a/b"));
    assert!(!valid_job_name(&"a".repeat(41)));
    assert!(job_name_problem("Bad").is_some());

    let res_running = job_start_check(
        "build",
        Some((4242, true)),
        Path::new("/wt/.tacho-run/build.exit"),
        "tacho",
    );
    assert!(res_running
        .unwrap_err()
        .contains("already running (pid 4242)"));

    let res_ok = job_start_check(
        "build",
        Some((4242, false)),
        Path::new("/wt/.tacho-run/build.exit"),
        "tacho",
    );
    assert_eq!(
        res_ok.unwrap(),
        "\"$@\"; echo $? > /wt/.tacho-run/build.exit"
    );
}

#[test]
fn case_83_wait_for_emits_60s_heartbeats_and_returns_3_at_9m_limit() {
    let now = Cell::new(1_000i64);
    let mut beats = Vec::new();
    let rc = wait_for(
        || true,
        |elapsed| beats.push(elapsed),
        || now.get(),
        |dt| now.set(now.get() + dt),
        WAIT_LIMIT_SECS,
        WAIT_BEAT_SECS,
    );
    assert_eq!(rc, 3);
    assert_eq!(beats, vec![60, 120, 180, 240, 300, 360, 420, 480]);
}

#[test]
fn case_84_job_wait_reports_exit_code_and_log_tail_on_completion() {
    let now = Cell::new(0i64);
    let (rc, lines) = job_wait(
        "test",
        Some(555),
        |_pid| now.get() < 120,
        || now.get() >= 120,
        || Some("compiling\nrunning 1 test\ntest result: FAILED\n".to_string()),
        || Some("101\n".to_string()),
        || now.get(),
        |dt| now.set(now.get() + dt),
        "tacho",
    );
    assert_eq!(rc, 0);
    assert!(lines
        .iter()
        .any(|l| l.contains("still running after 1 min")));
    assert!(lines
        .iter()
        .any(|l| l == "test ended with exit code 101  <- it FAILED"));
    assert!(lines
        .iter()
        .any(|l| l.contains("--- last lines of the log")));
}

#[test]
fn case_85_job_wait_returns_1_when_named_job_was_never_started() {
    let (rc, lines) = job_wait(
        "unknown",
        None,
        |_| false,
        || false,
        || None,
        || None,
        || 0,
        |_| {},
        "tacho",
    );
    assert_eq!(rc, 1);
    assert!(lines[0].contains("nothing named unknown was started here"));
}

#[test]
fn case_86_kill_group_sends_sigterm_or_sigkill_and_handles_esrch() {
    let mut signals = Vec::new();
    let dead = kill_group(
        777,
        false,
        |pid, sig| {
            signals.push((pid, sig));
            Ok(())
        },
        |_pid| false,
    );
    assert!(dead);
    assert_eq!(signals, vec![(777, 15)]);

    let gone = kill_group(777, true, |_pid, _sig| Err(()), |_pid| true);
    assert!(gone);
}

#[test]
fn case_87_job_kill_refuses_foreign_worktree_or_non_leader_and_suggests_now_on_timeout() {
    let c = wcfg();
    assert!(in_agent_cgroup(
        "/user.slice/agent-1.slice/run-u12.scope",
        &c
    ));
    let refused = job_kill(
        "srv",
        Path::new("/wt/a"),
        false,
        Some(900),
        Some((Path::new("/wt/b"), 900)),
        |_| true,
        |_| true,
        |_, _| Ok(()),
        || 0,
        |_| {},
        "tacho",
    );
    assert!(refused.contains("not a process this worktree started"));

    let now = Cell::new(0i64);
    let timed_out = job_kill(
        "srv",
        Path::new("/wt/a"),
        false,
        Some(900),
        Some((Path::new("/wt/a/sub"), 900)),
        |_| true,
        |_| true,
        |_, _| Ok(()),
        || now.get(),
        |dt| now.set(now.get() + dt),
        "tacho",
    );
    assert!(timed_out.contains("tacho kill srv --now"));
}
