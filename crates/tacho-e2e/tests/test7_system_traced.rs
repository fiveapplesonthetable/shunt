use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tacho_e2e::{perfetto_bin_path, traced_bin_path, traced_probes_bin_path, TestEnv};

struct TracedDaemons {
    traced: Option<Child>,
    probes: Option<Child>,
}

impl Drop for TracedDaemons {
    fn drop(&mut self) {
        if let Some(mut p) = self.probes.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        if let Some(mut t) = self.traced.take() {
            let _ = t.kill();
            let _ = t.wait();
        }
    }
}

#[test]
fn test_system_tracing_with_traced() {
    let run_test = std::env::var("TACHO_TEST_TRACED").unwrap_or_default() == "1"
        || (perfetto_bin_path().is_some() && traced_bin_path().is_some());

    if !run_test {
        eprintln!("Skipping test_system_tracing_with_traced: TACHO_TEST_TRACED!=1 and binaries not found.");
        return;
    }

    let env = TestEnv::new().unwrap();
    let perfetto_bin = perfetto_bin_path().expect("perfetto binary required");
    let traced_bin = traced_bin_path().expect("traced binary required");
    let probes_bin = traced_probes_bin_path().expect("traced_probes binary required");

    let producer_sock = env.runtime_dir.join("producer.sock");
    let consumer_sock = env.runtime_dir.join("consumer.sock");

    // 1. Start private traced instance
    let mut traced_cmd = Command::new(&traced_bin);
    traced_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let traced_child = traced_cmd.spawn().expect("failed to spawn traced");

    // Wait for private sockets
    let sockets_ready =
        TestEnv::wait_for_path(&producer_sock) && TestEnv::wait_for_path(&consumer_sock);
    assert!(sockets_ready, "traced private sockets failed to bind");

    // 2. Start private traced_probes instance
    let mut probes_cmd = Command::new(&probes_bin);
    probes_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let probes_child = probes_cmd.spawn().expect("failed to spawn traced_probes");

    let _daemons_guard = TracedDaemons {
        traced: Some(traced_child),
        probes: Some(probes_child),
    };

    // Ensure daemon is ready
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    // 3. Start system tracing via `tacho system start`
    let mut start_cmd = env.tacho_cmd();
    start_cmd
        .args(["system", "start"])
        .env("TACHO_PERFETTO", &perfetto_bin)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock);

    let start_status = start_cmd.status().expect("tacho system start failed");
    assert!(start_status.success(), "tacho system start returned error");

    // 4. Record agent slices via tacho emit
    let emit_out = env
        .tacho_cmd()
        .args(["emit", "span", "begin", "agent:sys-agent/turns", "Turn 1"])
        .output()
        .unwrap();
    assert!(emit_out.status.success());

    let emit_out2 = env
        .tacho_cmd()
        .args(["emit", "span", "end", "agent:sys-agent/turns"])
        .output()
        .unwrap();
    assert!(emit_out2.status.success());

    env.flush();

    // 5. Run tacho trace --system
    let sys_trace_path = env.home_dir.join("trace/system.perfetto-trace");
    let mut trace_cmd = env.tacho_cmd();
    trace_cmd
        .args(["trace", "--system", "-o", sys_trace_path.to_str().unwrap()])
        .env("TACHO_PERFETTO", &perfetto_bin)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock);

    let trace_out = trace_cmd.output().expect("tacho trace --system failed");
    assert!(
        trace_out.status.success(),
        "tacho trace --system failed: {}",
        String::from_utf8_lossy(&trace_out.stderr)
    );
    assert!(sys_trace_path.exists());

    // 6. Verify with oracle
    let oracle = env.run_oracle(&sys_trace_path).expect("oracle failed");
    assert!(
        oracle.error_stats.is_empty(),
        "System trace oracle errors: {:?}",
        oracle.error_stats
    );

    // Verify trace contains BOTH process table rows (from traced) and our agent slices
    let proc_rows = env
        .query_trace(&sys_trace_path, "SELECT count(*) FROM process;")
        .unwrap();
    let proc_count = proc_rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        proc_count > 0,
        "Expected process table rows from system trace"
    );

    let slice_rows = env
        .query_trace(&sys_trace_path, "SELECT count(*) FROM slice;")
        .unwrap();
    let slice_count = slice_rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse::<usize>().ok())
        .unwrap_or(0);
    assert!(slice_count > 0, "Expected agent slice rows in merged trace");

    // 7. Stop system session cleanly
    let mut stop_cmd = env.tacho_cmd();
    stop_cmd
        .args(["system", "stop"])
        .env("TACHO_PERFETTO", &perfetto_bin)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock);
    let _ = stop_cmd.status();
}
