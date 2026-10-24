//! Tests for `guard` (R60–R61), `budget` (R62–R68), `diag` (R69–R71), and `proptest` invariants.

use std::collections::BTreeMap;

use proptest::prelude::*;

use crate::decide::watch::activity::Activity;
use crate::decide::watch::budget::{
    agent_unit_name, busy, configured_owner_quota, container_flags, container_job_argv,
    cpu_limit_setting, docker_flags, find_budget, parse_android_caps, parse_ci_cpus, parse_cpu_max,
    parse_cpus, parse_reading, parse_usage, percent, scoped_argv, stop_unit_argv, Budget, Meter,
    SliceKind,
};
use crate::decide::watch::config::WatchConfig;
use crate::decide::watch::diag::{
    classify_transcript, codex_runs, consume_transcript_bytes, lock_holders, lock_waits,
    model_rates, price_transcript, project_slug, redact_text, scrub, summarize_transcripts, tail,
    waits, TokenUsage, TranscriptCacheEntry, TranscriptPlace,
};
use crate::decide::watch::guard::{background_refusal, burner_refusal};
use crate::decide::watch::jobs::valid_job_name;
use crate::decide::watch::procs::{below, tree_cpu, Proc, ProcId};
use crate::decide::watch::sample::{take_sample, Sample};
use crate::decide::watch::starve::{count_cpus, format_cpu_ids, valid_cpu_list};
use crate::decide::watch::supervise::{step, Step};
use crate::decide::watch::verdict::{verdict, Verdict};

#[test]
fn guard_refuses_direct_cpu_burners_and_allows_starve_or_pipe_yes() {
    let c = WatchConfig::default();
    assert!(burner_refusal("yes > /dev/null", &c).is_some());
    assert!(burner_refusal("timeout 30 stress --cpu 4", &c).is_some());
    assert!(burner_refusal("yes | cargo install foo", &c).is_none());
    assert!(burner_refusal("tacho starve -- cargo test", &c).is_none());
    assert!(background_refusal(true, &c).is_some());
    assert!(background_refusal(false, &c).is_none());
}

#[test]
fn budget_cgroup_meter_busy_and_container_wrappers() {
    let c = WatchConfig::default();
    assert_eq!(parse_cpus("500000 100000\n"), Some(5.0));
    assert_eq!(parse_cpu_max("400000 100000\n"), Some(4.0));
    assert_eq!(parse_cpu_max("max 100000\n"), None);
    assert_eq!(
        parse_usage("usage_usec 2500000\nuser_usec 100\n"),
        Some(2_500_000)
    );
    assert!((percent(2_000_000, 1.0, 4.0) - 50.0).abs() < 1e-6);

    let b = find_budget(
        "/sys/fs/cgroup",
        "0::/user.slice/user-1000.slice/session.scope\n",
        |p| {
            if p == "/sys/fs/cgroup/user.slice/user-1000.slice/cpu.max" {
                Some("800000 100000".to_string())
            } else {
                Some("max 100000".to_string())
            }
        },
    )
    .unwrap();
    assert_eq!(b.cpus, 8.0);

    let mut meter = Meter::new(Some(b), 10.0);
    assert_eq!(meter.sample(100.0, "usage_usec 1000000\n"), None);
    let pct = meter.sample(110.0, "usage_usec 41000000\n").unwrap();
    assert!((pct - 50.0).abs() < 1e-6);

    assert!(busy(&[10.0], Some(20.0), 1.0, 1.0, 16, &c));
    assert!(busy(&[100.0], Some(90.0), 1.0, 1.0, 16, &c));
    assert!(busy(&[100.0], None, 20.0, 20.0, 8, &c));
    assert!(!busy(&[100.0], Some(20.0), 1.0, 1.0, 16, &c));

    assert_eq!(
        cpu_limit_setting(Some("75"), None, 16, None, 80.0, "TACHO_"),
        (75.0, None)
    );
    assert_eq!(
        agent_unit_name("task/1", 1000, "tacho"),
        "tacho-agent-task-1-1000"
    );
    assert_eq!(
        stop_unit_argv("tacho-agent-task-1-1000"),
        vec![
            "systemctl",
            "--user",
            "stop",
            "tacho-agent-task-1-1000.scope"
        ]
    );
    let wrapped = scoped_argv(
        SliceKind::Agent,
        &["cargo".to_string(), "test".to_string()],
        "tacho-agent-t1",
        true,
        true,
        &c,
    );
    assert!(wrapped.contains(&"--unit=tacho-agent-t1".to_string()));

    assert_eq!(configured_owner_quota("CPUQuota=1200%\n"), Ok(12));
    assert_eq!(
        docker_flags(
            "unix:///run/docker.sock",
            "systemd",
            &[],
            "1200000 100000",
            4,
            12,
            true,
            &c
        ),
        Ok(vec![
            "--cgroup-parent=user-1000.slice".to_string(),
            "--cpus=4".to_string()
        ])
    );
    assert_eq!(
        container_flags(
            2,
            "systemd",
            &["name=rootless".to_string()],
            "unix:///run/user/1000/docker.sock",
            None,
            Ok(12),
            false,
            true,
            &c
        ),
        Ok(vec!["--cpus=2".to_string()])
    );

    let reading = parse_reading(
        [4.5, 3.25, 2.0],
        16,
        "MemTotal: 67108864 kB\nMemAvailable: 33554432 kB\n",
        &[("/".to_string(), 50)],
        &["55000"],
    );
    assert_eq!(reading.cores, 16);
    assert_eq!(reading.mem_total_gb, 64);
    assert_eq!(reading.mem_free_gb, 32);
    assert_eq!(reading.temp_c, Some(55.0));
    assert_eq!(reading.disk[0].free_gb, 50);

    assert_eq!(
        parse_ci_cpus("core", "${{ vars.CI_RUNS_ON && '--cpus=4' || '' }}"),
        Ok(4)
    );
    assert!(parse_ci_cpus("core", "--cpus=4").is_err());

    let mut android_env = BTreeMap::new();
    android_env.insert(
        "JAVA_TOOL_OPTIONS".to_string(),
        "${{ vars.CI_RUNS_ON && '-XX:ActiveProcessorCount=6' || '' }}".to_string(),
    );
    android_env.insert(
        "GRADLE_OPTS".to_string(),
        "${{ vars.CI_RUNS_ON && '-Dorg.gradle.workers.max=6' || '' }}".to_string(),
    );
    assert_eq!(parse_android_caps(&android_env).unwrap().len(), 2);

    let c_argv = container_job_argv(
        "u:24.04",
        "job-1",
        "bridge",
        "/work",
        "1000:1000",
        &["/work".to_string()],
        &["cargo test".to_string()],
        "tacho",
    );
    assert!(c_argv.contains(&"--init".to_string()));
    assert!(c_argv
        .iter()
        .any(|s| s.contains("trap 'interrupt 143' TERM")));
}

#[test]
fn diag_waits_scrub_and_transcript_cost_accounting() {
    let procs = vec![
        Proc {
            id: ProcId {
                pid: 10,
                start_ticks: 1,
            },
            ppid: 1,
            pgid: 10,
            name: "claude".to_string(),
            age: 10,
            cpu: 1.0,
            cwd: "/wt/a".to_string(),
            starved: false,
            argv: vec!["claude".to_string()],
            cgroup: String::new(),
        },
        Proc {
            id: ProcId {
                pid: 99,
                start_ticks: 1,
            },
            ppid: 1,
            pgid: 99,
            name: "cargo".to_string(),
            age: 20,
            cpu: 5.0,
            cwd: "/wt/b".to_string(),
            starved: false,
            argv: vec!["cargo".to_string()],
            cgroup: String::new(),
        },
    ];
    let mut wchans = BTreeMap::new();
    wchans.insert(10, vec!["futex_do_wait".to_string(), "0".to_string()]);
    let mut fds = BTreeMap::new();
    fds.insert(10, vec![("/target/.cargo-lock".to_string(), 55555)]);
    let locks =
        "1: FLOCK ADVISORY WRITE 99 08:01:55555 0 EOF\n2: -> FLOCK ADVISORY WRITE 10 08:01:55555 0 EOF\n";
    let w = waits(&procs, 10, &wchans, &fds, locks);
    assert_eq!(w.waiting_on, vec!["futex_do_wait".to_string()]);
    assert_eq!(w.lock_holder, "cargo");

    let secret_line = "Bearer abcdefghijklmnop token=my_secret_123 https://api.example.com/v1 192.168.1.50:8080 +1 (415) 555-2671 /home/alice/project/main.rs";
    let scrubbed = scrub(secret_line);
    assert!(!scrubbed.contains("abcdefghijklmnop"));
    assert!(!scrubbed.contains("my_secret_123"));
    assert!(scrubbed.contains("<url>"));
    assert!(scrubbed.contains("<address>"));
    assert!(scrubbed.contains("<number>"));
    assert!(scrubbed.contains("<path>/main.rs"));
    assert_eq!(tail("a\n\nb\n", 1).len(), 1);
    assert_eq!(redact_text("custom_tok", &["custom_tok"]), "[redacted]");
    assert_eq!(
        lock_waits("Blocking waiting for file lock on package cache\n"),
        vec!["package cache".to_string()]
    );
    assert_eq!(
        lock_holders(locks, |_pid| "cargo".to_string()),
        vec!["cargo".to_string()]
    );

    assert_eq!(project_slug("/work/repo.1"), "-work-repo-1");
    let mut slugs = BTreeMap::new();
    slugs.insert("-wt-a1".to_string(), "task-a".to_string());
    assert_eq!(
        classify_transcript("-wt-a1/subagents/s1.jsonl", "-repo", &slugs),
        (TranscriptPlace::Subagents, "task-a".to_string())
    );
    assert_eq!(model_rates("unknown-model"), (0.0, 0.0, 0.0));
    let u = TokenUsage {
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    assert!((price_transcript("claude-opus-5", &u) - 7.5).abs() < 1e-6);

    let jsonl = b"{\"type\":\"assistant\",\"at\":1000.0,\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n{\"type\":\"assistant\",\"at\":1005.0,\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n{\"partial\":true";
    let entry =
        consume_transcript_bytes(jsonl, &TranscriptCacheEntry::default(), "workers", "task-a");
    assert_eq!(entry.records.len(), 2);
    let mut pr_of = BTreeMap::new();
    pr_of.insert("task-a".to_string(), 42);
    let sum = summarize_transcripts(&entry.records, 1_100.0, 500.0, &pr_of);
    assert_eq!(sum.hour, 5.0);
    assert_eq!(sum.per_pr.get("42"), Some(&5.0));
    assert_eq!(
        codex_runs(&[("codex".to_string(), 1050.0)], 1100.0, 500.0),
        (1, 1)
    );
}

proptest! {
    #[test]
    fn prop_take_sample_timestamps_never_decrease(
        cpu1 in 0.0f64..10_000.0,
        cpu2 in 0.0f64..10_000.0,
        t1 in 1i64..1_000_000,
        dt in 1i64..10_000,
    ) {
        let c = WatchConfig::default();
        let s1 = take_sample(None, cpu1, t1, &c);
        let s2 = take_sample(Some(&s1), cpu2, t1 + dt, &c);
        prop_assert!(s2.alive_at >= s1.alive_at);
        prop_assert!(s2.busy_at >= s1.busy_at);
        prop_assert!(s2.alive_at <= t1 + dt);
    }

    #[test]
    fn prop_verdict_and_step_total_and_deterministic(
        started in 1i64..100_000,
        elapsed in 0i64..10_000,
        work_edit in 0i64..110_000,
        alive_at in 0i64..110_000,
        busy_at in 0i64..110_000,
        nudged in any::<bool>(),
    ) {
        let c = WatchConfig::default();
        let now = started + elapsed;
        let act = Activity {
            commits: 0,
            files: 0,
            last_commit: None,
            last_change: Some(work_edit),
            last_test: None,
        };
        let s = Sample { cpu: 10.0, at: now, alive_at, busy_at };
        let v1 = verdict(&act, &s, started, now, &c);
        let v2 = verdict(&act, &s, started, now, &c);
        prop_assert_eq!(v1, v2);
        let st = step(v1, nudged);
        if v1 == Verdict::Moving {
            prop_assert_eq!(st, Step::None);
        }
    }

    #[test]
    fn prop_tree_cpu_matches_below_sum(
        cpus in prop::collection::vec(0.0f64..100.0, 1..10)
    ) {
        let mut procs = Vec::new();
        for (i, &cpu) in cpus.iter().enumerate() {
            let pid = (i as u32) + 10;
            let ppid = if i == 0 { 1 } else { 10 };
            procs.push(Proc {
                id: ProcId { pid, start_ticks: 1 },
                ppid,
                pgid: 10,
                name: "p".to_string(),
                age: 10,
                cpu,
                cwd: "/wt".to_string(),
                starved: false,
                argv: Vec::new(),
                cgroup: String::new(),
            });
        }
        let sum_below: f64 = below(&procs, 10).iter().map(|p| p.cpu).sum();
        let total = tree_cpu(&procs, 10);
        prop_assert!((sum_below - total).abs() < 1e-9);
    }

    #[test]
    fn prop_cpu_list_roundtrip_and_job_name_validation(
        ids in prop::collection::btree_set(0usize..32, 1..8),
        candidate in "[a-zA-Z0-9._/-]{0,50}",
    ) {
        let vec_ids: Vec<usize> = ids.into_iter().collect();
        let formatted = format_cpu_ids(&vec_ids);
        prop_assert!(valid_cpu_list(&formatted));
        prop_assert_eq!(count_cpus(&formatted), vec_ids.len());

        let is_ok = valid_job_name(&candidate);
        let expected = !candidate.is_empty()
            && candidate.len() <= 40
            && candidate.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'));
        prop_assert_eq!(is_ok, expected);
    }

    #[test]
    fn prop_budget_percent_non_negative(
        used_usec in 0u64..1_000_000_000,
        seconds in 0.1f64..3600.0,
        cpus in 0.5f64..64.0,
    ) {
        let p = percent(used_usec, seconds, cpus);
        prop_assert!(p >= 0.0);
        let b = Budget { path: "/sys/fs/cgroup".to_string(), cpus };
        prop_assert!(b.cpus > 0.0);
    }
}
