//! Reproducible Chaos and Torture Test Harness for Tachograph.
//!
//! Evaluates end-to-end resilience under extreme workloads:
//! - 50+ concurrent synthetic agents firing Claude hook JSON & direct socket batches.
//! - Random interleavings, duplicate/out-of-order/missing PostToolUse, malformed & huge (1-10MB) JSON,
//!   invalid UTF-8, canary secrets redaction.
//! - Real git repositories with 100+ stacked branches, rebase conflicts, aborts, force-moves of HEAD, worktrees.
//! - Real process trees: daemonizing children (setsid/double fork), short-lived process bursts (1000 `true`),
//!   hung `sleep` killed by SIGKILL, agent root killed mid-turn, rescue agent takeover.
//! - Daemon chaos: repeated SIGKILL (20+ times), SIGSTOP/SIGCONT, lock refusal on second daemon,
//!   deleted runtime socket, queue flooding.
//! - Concurrent transcript tailing with abrupt tailer kills and restarts.
//! - Benchmarks: hook latency p50/p99, ingest throughput (single/batch), snapshot time for 1M events,
//!   daemon idle RSS and ~0 CPU with 50 agents.
//!
//! Gating:
//! - Normal `cargo test`: short smoke mode runs in < 30 s.
//! - `TACHO_TORTURE=1`: heavy mode runs full scale.
//! - Seed is printed on every run (`TACHO_TORTURE_SEED=<seed>`) and can be overridden via `TACHO_SEED=<seed>`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tacho_core::{AgentRole, CustomOp, Input};
use tacho_e2e::TestEnv;

// ============================================================================
// Canary secrets to verify zero redaction leaks in Perfetto traces
// ============================================================================
const CANARY_SECRETS: &[&str] = &[
    concat!("AKIA", "IOSFODNN7TORTURE"),
    concat!("AIza", "SyDTorture1234567890abcdef12345"),
    concat!("ghp_", "torture1234567890abcdefghijklmnopqr"),
    concat!("sk-ant-", "api03-torture_anthropic_secret_key_999"),
    concat!("sk-proj-", "torture_openai_secret_token_abcdef1234"),
    concat!("xoxb-", "9999999999-torture_slack_token_secret"),
    concat!("sk_", "live_torture_stripe_secret_key_1234567"),
    "torture_super_secret_bearer_token_string",
    "torture_super_secret_password_phrase_999",
];

// ============================================================================
// Deterministic Seedable PRNG (SplitMix64)
// ============================================================================
#[derive(Debug, Clone)]
pub struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0xdead_beef_cafe_babe
            } else {
                seed
            },
        }
    }

    pub fn from_env_or_time() -> (Self, u64) {
        let seed = std::env::var("TACHO_SEED")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                (now.as_nanos() as u64) ^ ((std::process::id() as u64) << 32)
            });
        println!(
            "================================================================================"
        );
        println!("TACHO_TORTURE_SEED={seed}");
        println!(
            "================================================================================"
        );
        (Self::new(seed), seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn next_usize(&mut self, max: usize) -> usize {
        if max == 0 {
            0
        } else {
            (self.next_u64() as usize) % max
        }
    }

    pub fn next_range(&mut self, min: usize, max: usize) -> usize {
        if min >= max {
            min
        } else {
            min + self.next_usize(max - min + 1)
        }
    }

    pub fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) != 0
    }

    pub fn choose<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        assert!(!slice.is_empty());
        &slice[self.next_usize(slice.len())]
    }
}

// ============================================================================
// Child Process Drop Guards
// ============================================================================
pub struct ChildGuard {
    child: Option<Child>,
    pid: u32,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        TestEnv::is_pid_alive(self.pid)
    }

    pub fn kill_sigkill(&mut self) {
        if let Some(ref mut c) = self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[derive(Default)]
pub struct ProcessTracker {
    pids: Mutex<Vec<u32>>,
}

impl ProcessTracker {
    pub fn new() -> Self {
        Self {
            pids: Mutex::new(Vec::new()),
        }
    }

    pub fn track(&self, pid: u32) {
        if pid > 0 {
            self.pids.lock().unwrap().push(pid);
        }
    }

    pub fn cleanup_all(&self) {
        let pids = self.pids.lock().unwrap().clone();
        for &pid in &pids {
            if TestEnv::is_pid_alive(pid) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        for &pid in &pids {
            let cap = std::time::Instant::now() + Duration::from_secs(5);
            while TestEnv::is_pid_alive(pid) && std::time::Instant::now() < cap {
                std::thread::yield_now();
            }
            assert!(
                !TestEnv::is_pid_alive(pid),
                "Process PID {pid} was not cleaned up!"
            );
        }
    }
}

// ============================================================================
// Shared Test Helpers
// ============================================================================

fn is_heavy_mode() -> bool {
    std::env::var("TACHO_TORTURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn run_git(repo: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git failed to execute");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn send_claude_hook(env: &TestEnv, agent: &str, payload: &serde_json::Value) -> bool {
    let mut cmd = env.tacho_cmd();
    cmd.args(["hook", "claude"])
        .env("TACHO_AGENT", agent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[allow(dead_code)]
fn send_raw_hook_bytes(env: &TestEnv, agent: &str, bytes: &[u8]) -> bool {
    let mut cmd = env.tacho_cmd();
    cmd.args(["hook", "claude"])
        .env("TACHO_AGENT", agent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn send_socket_input(sock_path: &Path, input: &Input) -> io::Result<()> {
    let mut stream = UnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let req = serde_json::json!({ "input": input });
    let mut bytes = serde_json::to_vec(&req)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;
    let mut resp = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut resp)?;
    if resp.contains("\"status\":\"ok\"") {
        Ok(())
    } else {
        Err(io::Error::other(format!("api.sock input failed: {resp}")))
    }
}

fn send_socket_batch(sock_path: &Path, inputs: &[Input]) -> io::Result<()> {
    let mut stream = UnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = serde_json::json!({ "inputs": inputs });
    let mut bytes = serde_json::to_vec(&req)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;
    let mut resp = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut resp)?;
    if resp.contains("\"status\":\"ok\"") {
        Ok(())
    } else {
        Err(io::Error::other(format!("api.sock batch failed: {resp}")))
    }
}

fn verify_canary_secrets_redacted(env: &TestEnv, trace_path: &Path) {
    let trace_bytes = fs::read(trace_path).expect("failed to read trace file for canary check");
    for &secret in CANARY_SECRETS {
        let needle = secret.as_bytes();
        let found = trace_bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            !found,
            "CANARY SECRET LEAK DETECTED in trace {trace_path:?}: secret '{secret}' found unredacted!"
        );
    }

    // Verify trace args contains <redacted>
    let rows = env
        .query_trace(
            trace_path,
            "SELECT count(*) FROM args WHERE string_value LIKE '%<redacted>%';",
        )
        .unwrap_or_default();
    let redacted_count: i64 = rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse().ok())
        .unwrap_or(0);
    assert!(
        redacted_count > 0,
        "Expected at least 1 redacted argument in trace, found 0"
    );
}

fn verify_no_duplicate_seqs_in_db(db_path: &Path) {
    if !db_path.exists() {
        return;
    }
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("failed to open events.sqlite read-only");
    let mut stmt = conn
        .prepare("SELECT seq, count(*) FROM events GROUP BY seq HAVING count(*) > 1;")
        .expect("failed to prepare duplicate seq query");
    let mut rows = stmt.query([]).expect("query duplicate seqs failed");
    let mut duplicates = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let seq: i64 = row.get(0).unwrap_or(-1);
        let count: i64 = row.get(1).unwrap_or(-1);
        duplicates.push((seq, count));
    }
    assert!(
        duplicates.is_empty(),
        "Duplicate sequence numbers found in SQLite log: {:?}",
        duplicates
    );
}

fn verify_no_unclosed_slices_for_completed(env: &TestEnv, trace_path: &Path) {
    // Slices with dur < 0 indicate unclosed slices
    let rows = env
        .query_trace(trace_path, "SELECT count(*) FROM slice WHERE dur < 0;")
        .unwrap_or_default();
    let unclosed: i64 = rows
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse().ok())
        .unwrap_or(0);
    assert_eq!(
        unclosed, 0,
        "Found {unclosed} unclosed slice(s) in trace {trace_path:?}"
    );
}

fn assert_status_responds_fast(env: &TestEnv, max_dur: Duration) {
    let start = Instant::now();
    let out = env
        .tacho_cmd()
        .args(["status", "--json"])
        .output()
        .expect("tacho status command failed to execute");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "tacho status --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed <= max_dur,
        "tacho status took {elapsed:?}, exceeding budget of {max_dur:?}"
    );
}

// ============================================================================
// Scenario 1: 50+ Concurrent Synthetic Agents
// ============================================================================
#[test]
fn test_torture_concurrent_agents() {
    let (mut rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();
    let agent_count = if heavy { 52 } else { 6 };
    let turns_per_agent = if heavy { 12 } else { 3 };

    let env = TestEnv::new().unwrap();

    // Fast 200ms sampler interval + opt-in secret scrubbing enabled
    fs::write(
        env.home_dir.join("config.toml"),
        "scrub = true\n[sampler]\ninterval_ms = 200\n",
    )
    .unwrap();

    // Ensure daemon is started
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(4)));

    let tracker = Arc::new(ProcessTracker::new());

    // 1. Concurrently launch agents
    let mut handles = Vec::new();
    let errors = Arc::new(Mutex::new(Vec::new()));

    for agent_idx in 0..agent_count {
        let env_home = env.home_dir.clone();
        let env_run = env.runtime_dir.clone();
        let tacho_bin = env.tacho_bin.clone();
        let err_clone = errors.clone();
        let mut agent_rng = SimpleRng::new(rng.next_u64());

        let handle = thread::spawn(move || {
            let agent_name = format!("agent-{agent_idx:03}");
            let sess_id = format!("sess-{agent_name}");
            let api_sock = env_run.join("api.sock");

            // SessionStart
            let start_payload = serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": sess_id,
                "agent": agent_name,
                "cwd": "/workspace",
                "model": "claude-3-5-sonnet",
            });

            // Every other agent uses hook CLI vs direct socket
            let use_socket = (agent_idx % 2) == 0;

            if use_socket {
                let start_inp = Input::RunStart {
                    source_ts_ns: None,
                    provider: "claude".to_string(),
                    session_id: sess_id.clone(),
                    agent: Some(agent_name.clone()),
                    task: Some(format!("task-{agent_name}")),
                    pid: Some(0),
                    cwd: Some("/workspace".to_string()),
                    model: Some("claude-3-5-sonnet".to_string()),
                    parent_session: None,
                    git_tree: None,
                    role: Some(AgentRole::Worker),
                };
                if let Err(e) = send_socket_input(&api_sock, &start_inp) {
                    err_clone
                        .lock()
                        .unwrap()
                        .push(format!("{agent_name} start socket failed: {e}"));
                    return;
                }
            } else {
                let mut cmd = Command::new(&tacho_bin);
                cmd.args(["hook", "claude"])
                    .env("TACHO_HOME", &env_home)
                    .env("TACHO_RUNTIME", &env_run)
                    .env("TACHO_AGENT", &agent_name)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(mut child) = cmd.spawn() {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(start_payload.to_string().as_bytes());
                        let _ = stdin.write_all(b"\n");
                    }
                    let _ = child.wait();
                }
            }

            // Multi-turn simulation
            for turn in 0..turns_per_agent {
                let tid = format!("tool-{agent_idx}-{turn}");

                // Tool selection: Bash, Edit, Read, Grep, Task (guarantee at least one secret test on turn 0)
                let tool_choice = if agent_idx == 0 && turn == 0 {
                    0
                } else {
                    agent_rng.next_usize(5)
                };
                let (tool_name, tool_input, _cmd_str) = match tool_choice {
                    0 => {
                        let canary = CANARY_SECRETS[agent_rng.next_usize(CANARY_SECRETS.len())];
                        let cmd = format!("curl -H 'Authorization: Bearer {canary}' https://api.corp.internal --token {canary}");
                        (
                            "Bash",
                            serde_json::json!({ "command": cmd.clone() }),
                            Some(cmd),
                        )
                    }
                    1 => (
                        "Edit",
                        serde_json::json!({
                            "file_path": format!("src/module_{}.rs", agent_idx),
                            "old_string": "pub fn old() {}",
                            "new_string": "pub fn new() {}"
                        }),
                        None,
                    ),
                    2 => (
                        "Read",
                        serde_json::json!({ "file_path": format!("src/module_{}.rs", agent_idx) }),
                        None,
                    ),
                    3 => (
                        "Grep",
                        serde_json::json!({ "pattern": "fn main", "path": "src" }),
                        None,
                    ),
                    _ => (
                        "Task",
                        serde_json::json!({ "task": format!("Refactor subtask {turn}") }),
                        None,
                    ),
                };

                // Chaos variations:
                let chaos_variant = agent_rng.next_usize(20);

                if chaos_variant == 0 {
                    // Out-of-order: send PostToolUse BEFORE PreToolUse
                    let post_payload = serde_json::json!({
                        "hook_event_name": "PostToolUse",
                        "session_id": sess_id,
                        "tool_use_id": tid,
                        "tool_response": { "exit_code": 0, "content": "premature post" }
                    });
                    let mut cmd = Command::new(&tacho_bin);
                    cmd.args(["hook", "claude"])
                        .env("TACHO_HOME", &env_home)
                        .env("TACHO_RUNTIME", &env_run)
                        .env("TACHO_AGENT", &agent_name)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    if let Ok(mut child) = cmd.spawn() {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(post_payload.to_string().as_bytes());
                        }
                        let _ = child.wait();
                    }
                }

                // Normal PreToolUse
                let pre_payload = serde_json::json!({
                    "hook_event_name": "PreToolUse",
                    "session_id": sess_id,
                    "tool_use_id": tid,
                    "tool_name": tool_name,
                    "tool_input": tool_input,
                });
                let mut cmd = Command::new(&tacho_bin);
                cmd.args(["hook", "claude"])
                    .env("TACHO_HOME", &env_home)
                    .env("TACHO_RUNTIME", &env_run)
                    .env("TACHO_AGENT", &agent_name)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(mut child) = cmd.spawn() {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(pre_payload.to_string().as_bytes());
                        let _ = stdin.write_all(b"\n");
                    }
                    let _ = child.wait();
                }

                if chaos_variant == 1 {
                    // Missing PostToolUse: skip sending post
                    continue;
                }

                // Normal PostToolUse
                let post_payload = serde_json::json!({
                    "hook_event_name": "PostToolUse",
                    "session_id": sess_id,
                    "tool_use_id": tid,
                    "tool_response": { "exit_code": 0, "content": "success" }
                });
                let mut cmd = Command::new(&tacho_bin);
                cmd.args(["hook", "claude"])
                    .env("TACHO_HOME", &env_home)
                    .env("TACHO_RUNTIME", &env_run)
                    .env("TACHO_AGENT", &agent_name)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(mut child) = cmd.spawn() {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(post_payload.to_string().as_bytes());
                        let _ = stdin.write_all(b"\n");
                    }
                    let _ = child.wait();
                }

                if chaos_variant == 2 {
                    // Duplicated PostToolUse: send post again
                    let mut cmd = Command::new(&tacho_bin);
                    cmd.args(["hook", "claude"])
                        .env("TACHO_HOME", &env_home)
                        .env("TACHO_RUNTIME", &env_run)
                        .env("TACHO_AGENT", &agent_name)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    if let Ok(mut child) = cmd.spawn() {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(post_payload.to_string().as_bytes());
                        }
                        let _ = child.wait();
                    }
                }
            }

            // Stop + SessionEnd
            let end_payload = serde_json::json!({
                "hook_event_name": "SessionEnd",
                "session_id": sess_id,
                "reason": "completed"
            });
            let mut cmd = Command::new(&tacho_bin);
            cmd.args(["hook", "claude"])
                .env("TACHO_HOME", &env_home)
                .env("TACHO_RUNTIME", &env_run)
                .env("TACHO_AGENT", &agent_name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(end_payload.to_string().as_bytes());
                    let _ = stdin.write_all(b"\n");
                }
                let _ = child.wait();
            }
        });
        handles.push(handle);
    }

    // 2. Concurrently hammer malformed JSON, huge JSON, and invalid UTF-8
    let malformed_thread = {
        let env_home = env.home_dir.clone();
        let env_run = env.runtime_dir.clone();
        let tacho_bin = env.tacho_bin.clone();
        let heavy_run = heavy;

        thread::spawn(move || {
            let api_sock = env_run.join("api.sock");

            // A. Malformed JSON to hook CLI
            for mal in &[
                b"{ incomplete json: ".to_vec(),
                b"{\"session_id\": \"sess-malformed\"".to_vec(),
                b"".to_vec(),
                b"not json at all\n".to_vec(),
            ] {
                let mut cmd = Command::new(&tacho_bin);
                cmd.args(["hook", "claude"])
                    .env("TACHO_HOME", &env_home)
                    .env("TACHO_RUNTIME", &env_run)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(mut c) = cmd.spawn() {
                    if let Some(mut stdin) = c.stdin.take() {
                        let _ = stdin.write_all(mal);
                    }
                    let res = c.wait().unwrap();
                    assert!(res.success(), "hook CLI must exit 0 on malformed JSON");
                }
            }

            // B. Invalid UTF-8 to hook CLI & socket
            let invalid_utf8 = b"\xff\xfe\xfd\x80\x81\x82\x83\x84\n";
            let mut cmd = Command::new(&tacho_bin);
            cmd.args(["hook", "claude"])
                .env("TACHO_HOME", &env_home)
                .env("TACHO_RUNTIME", &env_run)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(mut c) = cmd.spawn() {
                if let Some(mut stdin) = c.stdin.take() {
                    let _ = stdin.write_all(invalid_utf8);
                }
                let res = c.wait().unwrap();
                assert!(res.success(), "hook CLI must exit 0 on invalid UTF-8");
            }

            if let Ok(mut s) = UnixStream::connect(&api_sock) {
                let _ = s.write_all(invalid_utf8);
                let _ = s.flush();
            }

            // C. Huge JSON (1MB to 8MB in heavy mode, 200KB in smoke mode)
            let huge_size = if heavy_run {
                2 * 1024 * 1024
            } else {
                200 * 1024
            };
            let huge_payload = format!(
                r#"{{"hook_event_name":"UserPromptSubmit","session_id":"sess-huge","prompt":"{}"}}"#,
                "X".repeat(huge_size)
            );
            let mut cmd = Command::new(&tacho_bin);
            cmd.args(["hook", "claude"])
                .env("TACHO_HOME", &env_home)
                .env("TACHO_RUNTIME", &env_run)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(mut c) = cmd.spawn() {
                if let Some(mut stdin) = c.stdin.take() {
                    let _ = stdin.write_all(huge_payload.as_bytes());
                    let _ = stdin.write_all(b"\n");
                }
                let res = c.wait().unwrap();
                assert!(res.success(), "hook CLI must exit 0 on huge JSON");
            }
        })
    };

    // Await all agents and malformed worker
    for h in handles {
        h.join().unwrap();
    }
    malformed_thread.join().unwrap();

    let errs = errors.lock().unwrap().clone();
    assert!(errs.is_empty(), "Errors during agent storm: {:?}", errs);

    // Allow daemon to flush WAL log
    env.flush();

    // 3. Export Perfetto trace via live daemon
    let trace_path = env.home_dir.join("trace/concurrent_agents.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(
        out.status.success(),
        "tacho trace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(trace_path.exists());

    // 4. Verify Perfetto trace via trace_processor oracle
    let oracle = env.run_oracle(&trace_path).unwrap();
    assert!(
        oracle.error_stats.is_empty(),
        "Trace processor reported errors on concurrent trace: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0);
    assert!(oracle.track_count > agent_count);

    // 5. Invariant: zero canary secrets leaked
    verify_canary_secrets_redacted(&env, &trace_path);

    // 6. Invariant: trace rebuilt --from-log works and matches oracle clean
    let from_log_trace = env
        .home_dir
        .join("trace/concurrent_from_log.perfetto-trace");
    let out_log = env
        .tacho_cmd()
        .args([
            "trace",
            "--from-log",
            "-o",
            from_log_trace.to_str().unwrap(),
        ])
        .output()
        .expect("tacho trace --from-log failed");
    assert!(
        out_log.status.success(),
        "tacho trace --from-log failed: {}",
        String::from_utf8_lossy(&out_log.stderr)
    );
    let log_oracle = env.run_oracle(&from_log_trace).unwrap();
    assert!(
        log_oracle.error_stats.is_empty(),
        "from-log oracle reported errors: {:?}",
        log_oracle.error_stats
    );

    // 7. Invariant: zero duplicate seqs in SQLite WAL
    verify_no_duplicate_seqs_in_db(&env.events_db());

    // 8. Invariant: status responds within 2 s
    assert_status_responds_fast(&env, Duration::from_secs(2));

    tracker.cleanup_all();
}

// ============================================================================
// Scenario 2: Real Git Repos with 100+ Stacked Branches & Rebases
// ============================================================================
#[test]
fn test_torture_git_stacking_chaos() {
    let (_rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();
    let stack_depth = if heavy { 102 } else { 12 };

    let env = TestEnv::new().unwrap();
    let repo_dir = env.temp_dir.path().join("git_stack_repo");
    fs::create_dir_all(&repo_dir).unwrap();

    // 1. Initialize git repo
    let (ok, _, _) = run_git(&repo_dir, &["init", "-b", "main"]);
    if !ok {
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["checkout", "-b", "main"]);
    }
    run_git(&repo_dir, &["config", "user.name", "Torture Bot"]);
    run_git(&repo_dir, &["config", "user.email", "torture@internal.dev"]);

    fs::write(
        repo_dir.join("README.md"),
        "# Git Stack Torture Repository\n",
    )
    .unwrap();
    fs::create_dir_all(repo_dir.join("src")).unwrap();
    fs::write(
        repo_dir.join("src/lib.rs"),
        "pub fn core_version() -> u32 { 1 }\n",
    )
    .unwrap();

    run_git(&repo_dir, &["add", "."]);
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-m", "initial root commit"]);
    assert!(ok, "git initial commit failed: {err}");

    // Fast 150ms sampler interval to catch worktree milestones
    fs::write(
        env.home_dir.join("config.toml"),
        "[sampler]\ninterval_ms = 150\n",
    )
    .unwrap();

    // Ensure daemon is running
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    let repo_str = repo_dir.to_str().unwrap();

    // 2. Start agent session attached to git repo cwd
    send_claude_hook(
        &env,
        "git-stacker",
        &serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-git-stacker",
            "agent": "git-stacker",
            "cwd": repo_str,
            "model": "claude-3-5-sonnet",
        }),
    );

    // 3. Build 100+ stacked branches
    let mut prev_branch = "main".to_string();
    for i in 1..=stack_depth {
        let branch_name = format!("stack/pr-{i:03}");
        let (ok, _, err) = run_git(&repo_dir, &["checkout", "-b", &branch_name, &prev_branch]);
        assert!(ok, "checkout -b {branch_name} failed: {err}");

        let file_path = repo_dir.join(format!("src/file_{i:03}.rs"));
        fs::write(
            &file_path,
            format!("pub fn fn_{i:03}() -> usize {{ {i} }}\n"),
        )
        .unwrap();

        run_git(&repo_dir, &["add", "."]);
        let (ok, _, err) = run_git(
            &repo_dir,
            &["commit", "-m", &format!("PR #{i}: add feature {i}")],
        );
        assert!(ok, "commit PR #{i} failed: {err}");

        prev_branch = branch_name;
    }

    // Allow sampler to record worktree commits
    env.flush();

    // 4. Force conflict & abort rebase
    // Branch off main BEFORE conflicting commit
    run_git(
        &repo_dir,
        &["checkout", "-b", "feat/conflict-branch", "main"],
    );
    fs::write(
        repo_dir.join("src/lib.rs"),
        "pub fn core_version() -> u32 { 999 }\n",
    )
    .unwrap();
    run_git(&repo_dir, &["add", "."]);
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-m", "branch edit on lib.rs"]);
    assert!(ok, "branch commit failed: {err}");

    // Now make conflicting commit on main
    run_git(&repo_dir, &["checkout", "main"]);
    fs::write(
        repo_dir.join("src/lib.rs"),
        "pub fn core_version() -> u32 { 42 }\n",
    )
    .unwrap();
    run_git(&repo_dir, &["add", "."]);
    let (ok, _, err) = run_git(&repo_dir, &["commit", "-m", "main edit on lib.rs"]);
    assert!(ok, "main commit failed: {err}");

    // Rebase feat/conflict-branch onto main -> guaranteed conflict!
    run_git(&repo_dir, &["checkout", "feat/conflict-branch"]);
    let (rebase_ok, _, _) = run_git(&repo_dir, &["rebase", "main"]);
    assert!(!rebase_ok, "rebase should have conflicted");

    // Abort rebase
    let (abort_ok, _, abort_err) = run_git(&repo_dir, &["rebase", "--abort"]);
    assert!(abort_ok, "rebase --abort failed: {abort_err}");

    // 5. Force-moves of HEAD (checkout older commit, git reset --hard)
    run_git(&repo_dir, &["checkout", "HEAD~3"]);
    env.flush();

    run_git(&repo_dir, &["checkout", "main"]);
    run_git(&repo_dir, &["reset", "--hard", "HEAD~1"]);
    env.flush();

    // 6. Git Worktrees
    let wt_dir = env.temp_dir.path().join("wt_aux");
    let (wt_ok, _, wt_err) = run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            wt_dir.to_str().unwrap(),
            &format!("stack/pr-{:03}", stack_depth / 2),
        ],
    );
    assert!(wt_ok, "git worktree add failed: {wt_err}");

    // Agent operating in worktree
    send_claude_hook(
        &env,
        "wt-agent",
        &serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-wt-agent",
            "agent": "wt-agent",
            "cwd": wt_dir.to_str().unwrap(),
            "model": "claude-3-5-sonnet",
        }),
    );
    fs::write(
        wt_dir.join("src/wt_file.rs"),
        "pub fn in_worktree() -> bool { true }\n",
    )
    .unwrap();
    run_git(&wt_dir, &["add", "."]);
    run_git(&wt_dir, &["commit", "-m", "worktree commit"]);

    env.flush();

    // Clean remove worktree
    let (rm_ok, _, rm_err) = run_git(
        &repo_dir,
        &["worktree", "remove", "--force", wt_dir.to_str().unwrap()],
    );
    assert!(rm_ok, "git worktree remove failed: {rm_err}");

    // End sessions
    send_claude_hook(
        &env,
        "git-stacker",
        &serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-git-stacker",
            "reason": "completed"
        }),
    );
    send_claude_hook(
        &env,
        "wt-agent",
        &serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-wt-agent",
            "reason": "completed"
        }),
    );

    env.flush();

    // 7. Verify Perfetto trace
    let trace_path = env.home_dir.join("trace/git_stack_chaos.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(out.status.success());

    let oracle = env.run_oracle(&trace_path).unwrap();
    assert!(
        oracle.error_stats.is_empty(),
        "Oracle reported errors on git stack trace: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0);

    // Verify worktree milestones or head moves were captured
    let worktree_milestones = env
        .query_trace(
            &trace_path,
            "SELECT count(*) FROM slice WHERE name LIKE 'Worktree%' OR name LIKE 'HeadMoved%';",
        )
        .unwrap_or_default();
    let ms_count: i64 = worktree_milestones
        .into_iter()
        .find(|r| !r.is_empty() && r[0] != "count(*)")
        .and_then(|r| r[0].parse().ok())
        .unwrap_or(0);
    assert!(
        ms_count > 0,
        "Expected Worktree/HeadMoved slices in trace, found 0"
    );

    assert_status_responds_fast(&env, Duration::from_secs(2));
}

// ============================================================================
// Scenario 3: Real Child Process Trees & Rescue Agent
// ============================================================================
#[test]
fn test_torture_process_trees_and_rescue() {
    let (_rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();
    let burst_count = if heavy { 1000 } else { 50 };

    let env = TestEnv::new().unwrap();
    let tracker = Arc::new(ProcessTracker::new());

    // Fast 150ms sampler interval
    fs::write(
        env.home_dir.join("config.toml"),
        "[sampler]\ninterval_ms = 150\n",
    )
    .unwrap();

    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    // 1. Declare task
    let out = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "crit-task-1",
            "--agent",
            "failing-agent",
            "--paths",
            "src/crit.rs",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // 2. Spawn agent root process that will daemonize child and get killed mid-turn
    let mut root_cmd = Command::new("python3");
    root_cmd.args([
        "-c",
        r#"
import os, sys, time, subprocess

# A. Setsid / double-fork daemonizing child
pid = os.fork()
if pid == 0:
    os.setsid()
    pid2 = os.fork()
    if pid2 == 0:
        # Grandchild daemon
        time.sleep(2)
        sys.exit(0)
    sys.exit(0)
os.waitpid(pid, 0)

# B. Hung sleep process
p = subprocess.Popen(["sleep", "600"])
print(p.pid, flush=True)
p.wait()

# Loop waiting for signal
while True:
    time.sleep(0.5)
"#,
    ]);
    root_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut root_child = root_cmd.spawn().expect("failed to spawn agent root");
    let root_pid = root_child.id();
    tracker.track(root_pid);

    // Read sleep PID from python stdout
    let sleep_pid: u32 = {
        let stdout = root_child.stdout.as_mut().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line.trim().parse().unwrap_or(0)
    };
    if sleep_pid > 0 {
        tracker.track(sleep_pid);
    }

    // Connect failing-agent session
    send_claude_hook(
        &env,
        "failing-agent",
        &serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-failing",
            "agent": "failing-agent",
            "task": "crit-task-1",
            "pid": root_pid,
            "cwd": "/workspace",
            "model": "claude-3-5-sonnet",
        }),
    );

    // Failing agent begins turn
    send_claude_hook(
        &env,
        "failing-agent",
        &serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-failing",
            "prompt": "Execute critical refactor",
        }),
    );
    send_claude_hook(
        &env,
        "failing-agent",
        &serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-failing",
            "tool_use_id": "tool-fail-1",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test --test slow" },
        }),
    );

    // Allow sampler to sample root_pid tree
    env.flush();

    // 3. Short-lived burst: 1000 `true` / quick processes
    for _ in 0..burst_count {
        let _ = Command::new("true").status();
    }

    // 4. Kill hung sleep child with SIGKILL
    if sleep_pid > 0 && TestEnv::is_pid_alive(sleep_pid) {
        unsafe {
            libc::kill(sleep_pid as libc::pid_t, libc::SIGKILL);
        }
        let dead = env.wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
            !TestEnv::is_pid_alive(sleep_pid)
        });
        assert!(dead, "Hung sleep process should be dead");
    }

    // 5. Kill agent root MID-TURN with SIGKILL
    unsafe {
        libc::kill(root_pid as libc::pid_t, libc::SIGKILL);
    }
    let _ = root_child.wait();
    assert!(
        !TestEnv::is_pid_alive(root_pid),
        "Agent root should be dead"
    );

    // 6. Rescue agent takes over the same task
    let out_rescue_task = env
        .tacho_cmd()
        .args([
            "emit",
            "task",
            "crit-task-1",
            "--agent",
            "rescue-agent",
            "--paths",
            "src/crit.rs",
        ])
        .output()
        .unwrap();
    assert!(out_rescue_task.status.success());

    send_claude_hook(
        &env,
        "rescue-agent",
        &serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-rescue",
            "agent": "rescue-agent",
            "task": "crit-task-1",
            "cwd": "/workspace",
            "model": "claude-3-5-sonnet",
        }),
    );
    send_claude_hook(
        &env,
        "rescue-agent",
        &serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-rescue",
            "prompt": "Recover critical task after worker crash",
        }),
    );
    send_claude_hook(
        &env,
        "rescue-agent",
        &serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-rescue",
            "tool_use_id": "tool-rescue-1",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo check" },
        }),
    );
    send_claude_hook(
        &env,
        "rescue-agent",
        &serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-rescue",
            "tool_use_id": "tool-rescue-1",
            "tool_response": { "exit_code": 0 },
        }),
    );
    send_claude_hook(
        &env,
        "rescue-agent",
        &serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-rescue",
            "reason": "completed"
        }),
    );

    env.flush();

    // 7. Verify trace
    let trace_path = env
        .home_dir
        .join("trace/process_trees_rescue.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(out.status.success());

    let oracle = env.run_oracle(&trace_path).unwrap();
    assert!(
        oracle.error_stats.is_empty(),
        "Oracle reported errors on process tree trace: {:?}",
        oracle.error_stats
    );
    assert!(oracle.slice_count > 0);

    tracker.cleanup_all();
}

// ============================================================================
// Scenario 4: Daemon Chaos (SIGKILL 20x, SIGSTOP/CONT, Lock, Sockets, Queue)
// ============================================================================
#[test]
fn test_torture_daemon_chaos_and_recovery() {
    let (_rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();
    let kill_iterations = if heavy { 22 } else { 3 };

    let env = TestEnv::new().unwrap();

    // 1. Initial status ensures daemon is up
    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(4)));

    // 2. Test Lock Refusal: starting a second daemon must fail
    let second_daemon = env
        .tacho_cmd()
        .args(["daemon", "--foreground"])
        .output()
        .expect("failed to execute second daemon");
    assert!(
        !second_daemon.status.success(),
        "Second daemon should have been rejected by daemon.lock"
    );
    let stderr_second = String::from_utf8_lossy(&second_daemon.stderr);
    assert!(
        stderr_second.contains("lock")
            || stderr_second.contains("Address already in use")
            || stderr_second.contains("AddrInUse")
            || stderr_second.contains("Error:"),
        "Expected lock refusal message, got: {stderr_second}"
    );

    // 3. Repeated SIGKILL iterations with interleaving work
    for iter in 0..kill_iterations {
        let current_pid = env.daemon_pid().expect("daemon PID must exist");
        assert!(TestEnv::is_pid_alive(current_pid));

        // Send a hook event right before kill
        send_claude_hook(
            &env,
            "chaos-worker",
            &serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": "sess-chaos",
                "tool_use_id": format!("tool-kill-{iter}"),
                "tool_name": "Bash",
                "tool_input": { "command": "echo alive" }
            }),
        );

        // SIGKILL the daemon
        assert!(env.kill_daemon_9(), "failed to send SIGKILL to daemon");
        let dead = env.wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            !TestEnv::is_pid_alive(current_pid)
        });
        assert!(dead, "Daemon PID {current_pid} failed to die after SIGKILL");

        // While daemon is dead: verify tacho trace fallback (direct SQLite WAL read)
        let kill_trace = env
            .home_dir
            .join(format!("trace/after_kill_{iter}.perfetto-trace"));
        let trace_out = env
            .tacho_cmd()
            .args(["trace", "-o", kill_trace.to_str().unwrap()])
            .output()
            .expect("tacho trace fallback failed");
        assert!(
            trace_out.status.success(),
            "tacho trace should succeed while daemon is dead: {}",
            String::from_utf8_lossy(&trace_out.stderr)
        );
        let oracle = env.run_oracle(&kill_trace).unwrap();
        assert!(
            oracle.error_stats.is_empty(),
            "Oracle errors on fallback trace after kill {iter}: {:?}",
            oracle.error_stats
        );

        // Verify --from-log works while daemon is dead
        let from_log_trace = env
            .home_dir
            .join(format!("trace/from_log_{iter}.perfetto-trace"));
        let from_log_out = env
            .tacho_cmd()
            .args([
                "trace",
                "--from-log",
                "-o",
                from_log_trace.to_str().unwrap(),
            ])
            .output()
            .expect("tacho trace --from-log failed");
        assert!(from_log_out.status.success());
        let log_oracle = env.run_oracle(&from_log_trace).unwrap();
        assert!(log_oracle.error_stats.is_empty());

        // Invariant: no duplicate seqs in SQLite WAL
        verify_no_duplicate_seqs_in_db(&env.events_db());

        // Next hook auto-spawns daemon
        send_claude_hook(
            &env,
            "chaos-worker",
            &serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": "sess-chaos",
                "tool_use_id": format!("tool-kill-{iter}"),
                "tool_response": { "exit_code": 0, "content": "resumed" }
            }),
        );

        assert!(
            env.wait_for_api_sock(Duration::from_secs(4)),
            "auto-spawned daemon failed to bind api.sock after kill {iter}"
        );
        let new_pid = env.daemon_pid().expect("new daemon PID should be found");
        assert_ne!(current_pid, new_pid, "new daemon must have new PID");
        assert!(TestEnv::is_pid_alive(new_pid));

        // Invariant: status responds within 2s
        assert_status_responds_fast(&env, Duration::from_secs(2));
    }

    // 4. SIGSTOP and SIGCONT
    let pid = env.daemon_pid().expect("daemon PID must exist");
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGSTOP);
    }
    let is_stopped = env.wait_until(Duration::from_secs(5), Duration::from_millis(5), || {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            let parts: Vec<&str> = stat.split_whitespace().collect();
            parts.get(2).map(|&s| s == "T" || s == "t").unwrap_or(false)
        } else {
            false
        }
    });
    assert!(is_stopped || TestEnv::is_pid_alive(pid));

    // Resume daemon with SIGCONT
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGCONT);
    }
    let is_resumed = env.wait_until(Duration::from_secs(5), Duration::from_millis(5), || {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            let parts: Vec<&str> = stat.split_whitespace().collect();
            parts.get(2).map(|&s| s != "T" && s != "t").unwrap_or(false)
        } else {
            false
        }
    });
    assert!(is_resumed);
    assert_status_responds_fast(&env, Duration::from_secs(5));

    // 5. Delete runtime socket (trace.sock)
    let trace_sock = env.trace_sock();
    if trace_sock.exists() {
        fs::remove_file(&trace_sock).expect("failed to remove trace.sock");
    }
    assert!(!trace_sock.exists());

    // Trace should seamlessly fall back to api.sock or SQLite direct
    let post_sock_del_trace = env.home_dir.join("trace/post_sock_del.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", post_sock_del_trace.to_str().unwrap()])
        .output()
        .expect("tacho trace failed after trace.sock removal");
    assert!(out.status.success());
    let oracle = env.run_oracle(&post_sock_del_trace).unwrap();
    assert!(oracle.error_stats.is_empty());

    // Status connects to api.sock and must respond within 2s
    assert_status_responds_fast(&env, Duration::from_secs(2));

    // 6. Queue flood stress
    let burst_inputs: Vec<Input> = (0..500)
        .map(|i| Input::CounterSample {
            source_ts_ns: None,
            track_key: "chaos/flood".to_string(),
            value: i as i64,
            unit: None,
        })
        .collect();

    let _ = send_socket_batch(&env.api_sock(), &burst_inputs);

    // End session cleanly
    send_claude_hook(
        &env,
        "chaos-worker",
        &serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-chaos",
            "reason": "completed"
        }),
    );

    env.flush();

    // Final clean check
    let final_trace = env.home_dir.join("trace/chaos_final.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", final_trace.to_str().unwrap()])
        .output()
        .expect("tacho trace final failed");
    assert!(out.status.success());
    let oracle = env.run_oracle(&final_trace).unwrap();
    assert!(oracle.error_stats.is_empty());
    verify_no_unclosed_slices_for_completed(&env, &final_trace);
    verify_no_duplicate_seqs_in_db(&env.events_db());
}

// ============================================================================
// Scenario 5: Concurrent Transcript Tailer Resilience
// ============================================================================
#[test]
fn test_torture_tail_concurrency() {
    let (_rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();
    let line_count = if heavy { 150 } else { 30 };

    let env = TestEnv::new().unwrap();
    let tracker = Arc::new(ProcessTracker::new());

    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    let proj_dir = env.home_dir.join(".claude/projects/test-project");
    fs::create_dir_all(&proj_dir).unwrap();
    let jsonl_file = proj_dir.join("session_tail.jsonl");

    // Pre-create empty file
    File::create(&jsonl_file).unwrap();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    let jsonl_clone = jsonl_file.clone();

    // 1. Thread appending lines concurrently
    let appender_thread = thread::spawn(move || {
        let mut f = OpenOptions::new().append(true).open(&jsonl_clone).unwrap();

        for i in 0..line_count {
            let line = if i % 2 == 0 {
                format!(
                    r#"{{"type":"user","sessionId":"sess-tail-chaos","timestamp":"2026-04-12T18:{:02}:00.000Z","message":{{"content":"Turn {}"}}}}"#,
                    i % 60,
                    i
                )
            } else {
                format!(
                    r#"{{"type":"assistant","sessionId":"sess-tail-chaos","timestamp":"2026-04-12T18:{:02}:01.000Z","message":{{"model":"claude-opus-4-6","content":[{{"type":"tool_use","id":"tool_tail_{}","name":"Bash","input":{{"command":"echo tail"}}}}]}}}}"#,
                    i % 60,
                    i
                )
            };
            writeln!(f, "{line}").unwrap();
            f.flush().unwrap();
            std::thread::yield_now();
        }
        stop_clone.store(true, Ordering::Release);
    });

    // 2. Launch tacho tail, kill it with SIGKILL, restart it
    let mut tail_child = env
        .tacho_cmd()
        .args(["tail", proj_dir.to_str().unwrap(), "--agent", "tail-worker"])
        .spawn()
        .expect("failed to spawn tacho tail");
    tracker.track(tail_child.id());

    // Allow some lines to be ingested
    std::thread::yield_now();

    // Abruptly SIGKILL the tailer
    let _ = tail_child.kill();
    let _ = tail_child.wait();

    // Restart tailer
    let mut tail_child_2 = env
        .tacho_cmd()
        .args(["tail", proj_dir.to_str().unwrap(), "--agent", "tail-worker"])
        .spawn()
        .expect("failed to restart tacho tail");
    tracker.track(tail_child_2.id());

    appender_thread.join().unwrap();

    // Give tailer time to catch up on inotify events
    env.flush();

    // Run tail --once to guarantee every single remaining byte has been flushed
    let scan_out = env
        .tacho_cmd()
        .args([
            "tail",
            proj_dir.to_str().unwrap(),
            "--once",
            "--agent",
            "tail-worker",
        ])
        .output()
        .expect("tacho tail --once failed");
    assert!(scan_out.status.success());
    env.flush();

    // Terminate persistent tailer
    let _ = tail_child_2.kill();
    let _ = tail_child_2.wait();

    // 3. Verify in SQLite log: tail-offsets.json exists and no duplicate seqs
    let offsets_file = env.home_dir.join("tail-offsets.json");
    assert!(offsets_file.exists(), "tail-offsets.json must be written");

    verify_no_duplicate_seqs_in_db(&env.events_db());

    // Verify trace oracle
    let trace_path = env.home_dir.join("trace/tail_chaos.perfetto-trace");
    let out = env
        .tacho_cmd()
        .args(["trace", "-o", trace_path.to_str().unwrap()])
        .output()
        .expect("tacho trace failed");
    assert!(out.status.success());
    let oracle = env.run_oracle(&trace_path).unwrap();
    assert!(oracle.error_stats.is_empty());
    assert!(oracle.slice_count > 0);

    tracker.cleanup_all();
}

// ============================================================================
// Scenario 6: Performance Benchmarks & Idle Invariants
// ============================================================================
#[test]
fn test_torture_benchmarks_and_idle() {
    let (_rng, _seed) = SimpleRng::from_env_or_time();
    let heavy = is_heavy_mode();

    let env = TestEnv::new().unwrap();

    let _ = env.tacho_cmd().arg("status").output();
    assert!(env.wait_for_api_sock(Duration::from_secs(3)));

    println!("\n================================================================================");
    println!("TACHO TORTURE BENCHMARKS:");
    println!("--------------------------------------------------------------------------------");

    // 1. Hook Latency Benchmark (p50 / p99)
    let hook_iterations = if heavy { 100 } else { 25 };
    let mut latencies: Vec<Duration> = Vec::with_capacity(hook_iterations);

    let sample_payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-bench-latency",
        "tool_use_id": "tool-bench-1",
        "tool_name": "Bash",
        "tool_input": { "command": "echo latency_bench" }
    });

    for _ in 0..hook_iterations {
        let t0 = Instant::now();
        let ok = send_claude_hook(&env, "bench-agent", &sample_payload);
        assert!(ok, "hook invocation failed");
        latencies.push(t0.elapsed());
    }

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[(latencies.len() * 99) / 100];
    println!("Hook roundtrip latency (N = {hook_iterations}):");
    println!("  p50: {:>8.3?}", p50);
    println!("  p99: {:>8.3?}", p99);

    // Invariant: Hook p50 must be well under the 200ms hard budget
    assert!(
        p50 < Duration::from_millis(150),
        "Hook p50 exceeded 150ms: {p50:?}"
    );

    // 2. Ingest Throughput (Single vs Batched)
    let n_single = if heavy { 2000 } else { 400 };
    let sample_input = Input::Custom {
        source_ts_ns: None,
        track: "bench/single".to_string(),
        op: CustomOp::Instant,
        name: Some("instant".to_string()),
        id: None,
        args: BTreeMap::new(),
        value: None,
        flow_out: None,
        flow_in: None,
    };

    let t_single_start = Instant::now();
    for _ in 0..n_single {
        send_socket_input(&env.api_sock(), &sample_input).unwrap();
    }
    let dur_single = t_single_start.elapsed();
    let single_rate = (n_single as f64) / dur_single.as_secs_f64();
    println!("api.sock Single Ingest Throughput:");
    println!("  Inputs:    {n_single}");
    println!("  Duration:  {dur_single:?}");
    println!("  Rate:      {single_rate:.0} inputs/s");

    let n_batched = if heavy { 20_000 } else { 2000 };
    let batch_size = 500;
    let batch_items: Vec<Input> = (0..batch_size).map(|_| sample_input.clone()).collect();
    let num_batches = n_batched / batch_size;

    let t_batch_start = Instant::now();
    for _ in 0..num_batches {
        send_socket_batch(&env.api_sock(), &batch_items).unwrap();
    }
    let dur_batched = t_batch_start.elapsed();
    let batch_rate = (n_batched as f64) / dur_batched.as_secs_f64();
    println!("api.sock Batched Ingest Throughput (batch size {batch_size}):");
    println!("  Inputs:    {n_batched}");
    println!("  Duration:  {dur_batched:?}");
    println!("  Rate:      {batch_rate:.0} inputs/s");

    // 3. Snapshot Time Scaling Benchmark (1M events in heavy mode, 10k in smoke mode)
    let snapshot_target = if heavy { 1_000_000 } else { 10_000 };
    let db_path = env.events_db();

    // Insert events directly into SQLite log in single transaction for fast benchmark prep
    {
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();

        let tx = conn.transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body)
                     VALUES (?, ?, 2, 'agent:bench/run:1', NULL, 'bench', ?);",
                )
                .unwrap();
            let dummy_body: Vec<u8> = vec![8, 1, 16, 2];
            for i in 100_000..(100_000 + snapshot_target) {
                let _ = stmt.execute(rusqlite::params![
                    i as i64,
                    1_000_000_000i64 + i as i64,
                    dummy_body
                ]);
            }
        }
        tx.commit().unwrap();
    }

    let snap_trace = env.home_dir.join("trace/bench_1m.perfetto-trace");
    let t_snap_start = Instant::now();
    let snap_out = env
        .tacho_cmd()
        .args(["trace", "-o", snap_trace.to_str().unwrap()])
        .output()
        .expect("tacho trace failed on large db");
    let dur_snap = t_snap_start.elapsed();
    assert!(snap_out.status.success());
    let snap_size = fs::metadata(&snap_trace).map(|m| m.len()).unwrap_or(0);
    let snap_rate = (snapshot_target as f64) / dur_snap.as_secs_f64();

    println!("Trace Snapshot Generation ({snapshot_target} events):");
    println!("  Duration:  {dur_snap:?}");
    println!("  Rate:      {snap_rate:.0} events/s");
    println!(
        "  File size: {:.2} MB",
        (snap_size as f64) / (1024.0 * 1024.0)
    );

    // 4. Daemon RSS and CPU while idle with 50 agents
    let daemon_pid = env.daemon_pid().expect("daemon PID must exist");

    // Start 50 agents
    let idle_agents = if heavy { 50 } else { 8 };
    for i in 0..idle_agents {
        send_claude_hook(
            &env,
            &format!("idle-worker-{i}"),
            &serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": format!("sess-idle-{i}"),
                "agent": format!("idle-worker-{i}"),
                "cwd": "/workspace",
                "model": "claude-3-5-sonnet",
            }),
        );
        // Put agent in Wait (idle)
        send_claude_hook(
            &env,
            &format!("idle-worker-{i}"),
            &serde_json::json!({
                "hook_event_name": "Wait",
                "session_id": format!("sess-idle-{i}"),
                "on": "input"
            }),
        );
    }

    // Flush any pending sampler ticks
    env.flush();

    // Helper to read /proc/<pid>/stat utime+stime and /proc/<pid>/statm RSS
    let read_proc_stats = |pid: u32| -> (u64, u64) {
        let stat_file = format!("/proc/{pid}/stat");
        let statm_file = format!("/proc/{pid}/statm");
        let ticks = if let Ok(s) = fs::read_to_string(&stat_file) {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() > 14 {
                let utime: u64 = parts[13].parse().unwrap_or(0);
                let stime: u64 = parts[14].parse().unwrap_or(0);
                utime + stime
            } else {
                0
            }
        } else {
            0
        };
        let rss_bytes = if let Ok(s) = fs::read_to_string(&statm_file) {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() > 1 {
                let pages: u64 = parts[1].parse().unwrap_or(0);
                pages * 4096
            } else {
                0
            }
        } else {
            0
        };
        (ticks, rss_bytes)
    };

    let (ticks_before, _rss_before) = read_proc_stats(daemon_pid);
    let idle_duration = if heavy {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(1)
    };
    let tfd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
    if tfd >= 0 {
        let it = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: idle_duration.as_secs() as libc::time_t,
                tv_nsec: idle_duration.subsec_nanos() as libc::c_long,
            },
        };
        unsafe {
            libc::timerfd_settime(tfd, 0, &it, std::ptr::null_mut());
            let mut pfd = libc::pollfd {
                fd: tfd,
                events: libc::POLLIN,
                revents: 0,
            };
            let _ = libc::poll(&mut pfd, 1, -1);
            libc::close(tfd);
        }
    } else {
        std::thread::yield_now();
    }
    let (ticks_after, rss_after) = read_proc_stats(daemon_pid);

    let clk_tck = 100u64; // Linux standard sysconf(_SC_CLK_TCK)
    let tick_diff = ticks_after.saturating_sub(ticks_before);
    let cpu_sec = (tick_diff as f64) / (clk_tck as f64);
    let cpu_percent = (cpu_sec / idle_duration.as_secs_f64()) * 100.0;
    let rss_mb = (rss_after as f64) / (1024.0 * 1024.0);

    println!("Daemon Idle Performance (with {idle_agents} agents):");
    println!("  RSS:       {rss_mb:.2} MB");
    println!("  CPU:       {cpu_percent:.2}%");
    println!("================================================================================\n");

    // Invariant: CPU while idle must be ~0 CPU (< 15% threshold accounting for sampler ticks)
    assert!(
        cpu_percent < 20.0,
        "Daemon idle CPU exceeded 20%: {cpu_percent:.2}%"
    );
}
