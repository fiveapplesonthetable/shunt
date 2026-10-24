use serde::Deserialize;
use std::io::Write;
use std::process::Stdio;
use tacho_e2e::TestEnv;

#[derive(Debug, Deserialize)]
struct StatusAgent {
    agent: String,
    state: String,
    for_seconds: u64,
}

#[test]
fn test_status_vs_agents_now_sql() {
    let env = TestEnv::new().unwrap();

    // Ensure daemon is running
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock());

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // Emit events for a fleet of 3 scripted agents
    let fleet_inputs = [
        // Agent 1: worker-1
        serde_json::json!({
            "type": "run_start",
            "source_ts_ns": now_ns - 10_000_000_000,
            "provider": "claude",
            "session_id": "session-worker-1",
            "agent": "worker-1",
        }),
        serde_json::json!({
            "type": "turn_start",
            "source_ts_ns": now_ns - 8_000_000_000,
            "session": { "provider": "claude", "session_id": "session-worker-1" },
            "turn_number": 1,
        }),
        serde_json::json!({
            "type": "tool_begin",
            "source_ts_ns": now_ns - 5_000_000_000,
            "session": { "provider": "claude", "session_id": "session-worker-1" },
            "tool_use_id": "tc-1",
            "tool_name": "Bash",
            "input": {
                "tool": "other",
                "input_bytes": 10
            }
        }),
        // Agent 2: worker-2
        serde_json::json!({
            "type": "run_start",
            "source_ts_ns": now_ns - 15_000_000_000,
            "provider": "gemini",
            "session_id": "session-worker-2",
            "agent": "worker-2",
        }),
        serde_json::json!({
            "type": "turn_start",
            "source_ts_ns": now_ns - 12_000_000_000,
            "session": { "provider": "gemini", "session_id": "session-worker-2" },
            "turn_number": 1,
        }),
        // Agent 3: worker-3
        serde_json::json!({
            "type": "run_start",
            "source_ts_ns": now_ns - 20_000_000_000,
            "provider": "jetski",
            "session_id": "session-worker-3",
            "agent": "worker-3",
        }),
    ];

    // Emit via tacho emit --json
    let mut child = env
        .tacho_cmd()
        .args(["emit", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tacho emit");

    if let Some(mut stdin) = child.stdin.take() {
        for input in &fleet_inputs {
            let line = serde_json::to_string(input).unwrap();
            stdin.write_all(line.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    // Allow daemon appender to process inputs
    assert!(env.flush());

    // 1. Fetch tacho status --json
    let status_out = env
        .tacho_cmd()
        .args(["status", "--json"])
        .output()
        .expect("tacho status --json failed");
    assert!(status_out.status.success());
    let status_str = String::from_utf8_lossy(&status_out.stdout);
    let mut status_agents: Vec<StatusAgent> =
        serde_json::from_str(&status_str).expect("failed to parse status json");
    status_agents.sort_by(|a, b| a.agent.cmp(&b.agent));
    assert!(
        !status_agents.is_empty(),
        "expected active agents in status"
    );

    // 2. Run tacho query agents-now
    let query_out = env
        .tacho_cmd()
        .args(["query", "agents-now"])
        .output()
        .expect("tacho query agents-now failed");
    assert!(
        query_out.status.success(),
        "query agents-now failed: {}",
        String::from_utf8_lossy(&query_out.stderr)
    );

    let query_str = String::from_utf8_lossy(&query_out.stdout);
    // Parse query CSV output
    let mut sql_rows = Vec::new();
    for line in query_str.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("agent,")
            || trimmed.starts_with("agent\t")
            || trimmed.starts_with("\"agent\"")
            || trimmed.starts_with("Trace loaded:")
            || trimmed.starts_with("Query executed")
            || trimmed.starts_with('[')
        {
            continue;
        }
        let parts: Vec<&str> = if trimmed.contains(',') {
            trimmed.split(',').collect()
        } else {
            trimmed.split_whitespace().collect()
        };
        if parts.len() >= 3 {
            let agent = parts[0].trim().trim_matches('"').to_string();
            let state = parts[1].trim().trim_matches('"').to_string();
            let dur_ns: u64 = parts[2].trim().trim_matches('"').parse().unwrap_or(0);
            sql_rows.push((agent, state, dur_ns));
        }
    }
    sql_rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!sql_rows.is_empty(), "expected sql rows from agents-now");

    // 3. Row-for-row comparison: state, agent, dur within snapshot skew
    assert_eq!(
        status_agents.len(),
        sql_rows.len(),
        "Agent count mismatch between status ({}) and SQL ({})",
        status_agents.len(),
        sql_rows.len()
    );

    for (s_agent, (q_agent, q_state, q_dur_ns)) in status_agents.iter().zip(sql_rows.iter()) {
        assert_eq!(
            &s_agent.agent, q_agent,
            "Agent ID mismatch between status and SQL"
        );
        assert!(
            s_agent.state.eq_ignore_ascii_case(q_state),
            "Agent state mismatch: status='{}' vs sql='{}'",
            s_agent.state,
            q_state
        );
        let q_dur_secs = q_dur_ns / 1_000_000_000;
        let diff = (s_agent.for_seconds as i64 - q_dur_secs as i64).abs();
        assert!(
            diff <= 3,
            "Duration skew exceeds 3s: status for_seconds={} vs sql dur_secs={}",
            s_agent.for_seconds,
            q_dur_secs
        );
    }
}
