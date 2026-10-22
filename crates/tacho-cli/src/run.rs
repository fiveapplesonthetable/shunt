//! Wrapper command for running external processes under tachograph (`tacho run -- <cmd>`).
//!
//! Spawns the command with inherited stdin and stderr, tees stdout in real time,
//! detects structured streams (Claude, Gemini, Codex), parses and feeds events
//! to the daemon, and passes through the exact exit code (signals -> 128 + n).

use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::DaemonClient;

/// Runs a command wrapped by tachograph.
pub fn execute_run(cmd: Vec<String>) -> io::Result<i32> {
    if cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no command specified to run. Usage: tacho run -- <command> [args...]",
        ));
    }

    let client = DaemonClient::new();
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped());

    let mut child = command.spawn()?;
    let pid = child.id();

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let session_id = format!("run-{}", pid);
    let session_key = serde_json::json!({
        "provider": "run",
        "session_id": session_id,
    });

    // Notify daemon of run begin (non-fatal if daemon is starting)
    let _ = client.send_request(&serde_json::json!({
        "input": {
            "type": "run_start",
            "source_ts_ns": now_ns,
            "provider": "run",
            "session_id": session_id,
            "pid": pid,
            "cwd": std::env::current_dir().ok().and_then(|p| p.to_str().map(String::from)),
        }
    }));

    // Tee raw stdout bytes directly without buffering/blocking, sniffing in separate buffer
    if let Some(mut stdout_pipe) = child.stdout.take() {
        let mut chunk = [0u8; 8192];
        let mut sniff_buf = Vec::new();
        let mut first_lines: Vec<String> = Vec::new();
        let mut detected_format: Option<&'static str> = None;

        loop {
            match stdout_pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    // 1. Tee raw bytes immediately to stdout unchanged
                    let mut out = io::stdout();
                    let _ = out.write_all(&chunk[..n]);
                    let _ = out.flush();

                    // 2. Stream sniffing in separate buffer (capped at 1 MiB)
                    if sniff_buf.len() < 1_048_576 {
                        sniff_buf.extend_from_slice(&chunk[..n]);
                        while let Some(pos) = sniff_buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = sniff_buf.drain(..=pos).collect();
                            if let Ok(line_str) = std::str::from_utf8(&line_bytes) {
                                let trimmed = line_str.trim();
                                if detected_format.is_none() && first_lines.len() < 10 {
                                    first_lines.push(trimmed.to_string());
                                    if trimmed.contains(r#""type":"system""#)
                                        || trimmed.contains(r#""subtype":"init""#)
                                    {
                                        detected_format = Some("claude");
                                    } else if trimmed.contains(r#""thread.started""#) {
                                        detected_format = Some("codex");
                                    } else if trimmed.contains(r#""type":"turn_start""#) {
                                        detected_format = Some("gemini");
                                    }
                                }

                                if detected_format.is_some() && !trimmed.is_empty() {
                                    if let Ok(val) =
                                        serde_json::from_str::<serde_json::Value>(trimmed)
                                    {
                                        let _ = client.send_request(&serde_json::json!({
                                            "input": val
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        // Handle trailing un-terminated line in sniff_buf if present
        if detected_format.is_some() && !sniff_buf.is_empty() {
            if let Ok(line_str) = std::str::from_utf8(&sniff_buf) {
                let trimmed = line_str.trim();
                if !trimmed.is_empty() {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        let _ = client.send_request(&serde_json::json!({
                            "input": val
                        }));
                    }
                }
            }
        }
    }

    // Wait for child exit and compute exit code
    let status = child.wait()?;
    let exit_code = match status.code() {
        Some(code) => code,
        None => status.signal().map(|sig| 128 + sig).unwrap_or(1),
    };

    let end_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    // Notify daemon of run end
    let _ = client.send_request(&serde_json::json!({
        "input": {
            "type": "run_end",
            "source_ts_ns": end_ns,
            "session": session_key,
            "exit_code": exit_code,
        }
    }));

    Ok(exit_code)
}
