//! Status, report, and watch commands querying the live flight recorder state.

use std::io::{self, BufRead, BufReader, Write};

use crate::cli::{ReportArgs, StatusArgs, WatchArgs};
use crate::client::DaemonClient;

/// Executes `tacho status [--json]`.
pub fn execute_status(args: StatusArgs) -> io::Result<()> {
    let client = DaemonClient::new();
    let resp = client.send_request(&serde_json::json!({
        "cmd": "status",
        "json": args.json,
    }))?;

    if let Some(output) = resp.get("output").and_then(|v| v.as_str()) {
        println!("{output}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
    }

    Ok(())
}

/// Executes `tacho report <agent> [--since 30m] [--diff-claims]`.
pub fn execute_report(args: ReportArgs) -> io::Result<()> {
    let client = DaemonClient::new();
    let since_ns = args
        .since
        .as_deref()
        .and_then(crate::trace::parse_since_duration_ns);

    let resp = client.send_request(&serde_json::json!({
        "cmd": "report",
        "agent": args.agent,
        "since": since_ns,
        "diff_claims": args.diff_claims,
    }))?;

    if args.diff_claims {
        print_diff_claims(&args.agent, &resp);
        return Ok(());
    }

    if let Some(output) = resp.get("output").and_then(|v| v.as_str()) {
        println!("{output}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
    }

    Ok(())
}

/// Prints a compact <100 token claim verification summary table.
fn print_diff_claims(agent_id: &str, resp: &serde_json::Value) {
    let agent_state = resp.get("agent_state");

    // 1. Edits count
    let edits_count = agent_state
        .and_then(|s| s.get("edits_count").and_then(|v| v.as_u64()))
        .or_else(|| {
            agent_state.and_then(|s| {
                s.get("timeline").and_then(|t| t.as_array()).map(|arr| {
                    arr.iter()
                        .filter(|item| {
                            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                            let sum = item.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                            kind.contains("Edit") || sum.contains("Edit")
                        })
                        .count() as u64
                })
            })
        })
        .unwrap_or(0);

    // 2. Commits succeeded & Pushes succeeded
    let commits_succeeded = agent_state
        .and_then(|s| s.get("commits_succeeded").and_then(|v| v.as_u64()))
        .or_else(|| {
            agent_state.and_then(|s| {
                s.get("recent_commands")
                    .and_then(|rc| rc.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|c| {
                                let class = c.get("class").and_then(|v| v.as_str()).unwrap_or("");
                                let exit = c.get("exit_code").and_then(|v| v.as_i64());
                                class.eq_ignore_ascii_case("commit") && exit == Some(0)
                            })
                            .count() as u64
                    })
            })
        })
        .unwrap_or(0);

    let pushes_succeeded = agent_state
        .and_then(|s| s.get("pushes_succeeded").and_then(|v| v.as_u64()))
        .or_else(|| {
            agent_state.and_then(|s| {
                s.get("recent_commands")
                    .and_then(|rc| rc.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|c| {
                                let class = c.get("class").and_then(|v| v.as_str()).unwrap_or("");
                                let exit = c.get("exit_code").and_then(|v| v.as_i64());
                                class.eq_ignore_ascii_case("push") && exit == Some(0)
                            })
                            .count() as u64
                    })
            })
        })
        .unwrap_or(0);

    // 3. Flags (especially CLAIM)
    let flags: Vec<String> = agent_state
        .and_then(|s| s.get("flags").and_then(|f| f.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let has_claim_flag = flags.iter().any(|f| f.contains("CLAIM"));
    let claim_str = if has_claim_flag {
        flags
            .iter()
            .filter(|f| f.contains("CLAIM"))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        "NONE".to_string()
    };

    // 4. Test after last edit (VERIFIED vs UNVERIFIED)
    let test_after_edit = if has_claim_flag {
        "UNVERIFIED"
    } else {
        let last_edit_ts =
            agent_state.and_then(|s| s.get("last_edit_ts_ns").and_then(|v| v.as_u64()));
        let last_passing_test_ts =
            agent_state.and_then(|s| s.get("last_passing_test_ts_ns").and_then(|v| v.as_u64()));

        match (last_edit_ts, last_passing_test_ts) {
            (Some(edit_ts), Some(test_ts)) if test_ts >= edit_ts => "VERIFIED",
            (Some(_), _) => "UNVERIFIED",
            (None, _) => {
                let last_test_ts = agent_state.and_then(|s| {
                    s.get("recent_commands")
                        .and_then(|rc| rc.as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .filter(|c| {
                                    let class =
                                        c.get("class").and_then(|v| v.as_str()).unwrap_or("");
                                    let exit = c.get("exit_code").and_then(|v| v.as_i64());
                                    class.eq_ignore_ascii_case("test") && exit == Some(0)
                                })
                                .filter_map(|c| c.get("ts_ns").and_then(|v| v.as_u64()))
                                .max()
                        })
                });
                let last_edit_cmd_ts = agent_state.and_then(|s| {
                    s.get("timeline")
                        .and_then(|tl| tl.as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .filter(|item| {
                                    let kind =
                                        item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                                    kind.contains("Edit")
                                })
                                .filter_map(|item| item.get("ts_ns").and_then(|v| v.as_u64()))
                                .max()
                        })
                });
                match (last_edit_cmd_ts, last_test_ts) {
                    (Some(e), Some(t)) if t >= e => "VERIFIED",
                    (Some(_), _) => "UNVERIFIED",
                    (None, Some(_)) => "VERIFIED",
                    (None, None) => {
                        if edits_count > 0 {
                            "UNVERIFIED"
                        } else {
                            "VERIFIED"
                        }
                    }
                }
            }
        }
    };

    // 5. Git worktree status
    let cwd = agent_state
        .and_then(|s| s.get("cwd").and_then(|v| v.as_str()))
        .unwrap_or(".");
    let git_status = match std::process::Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            let dirty_files = s.lines().filter(|l| !l.trim().is_empty()).count();
            if dirty_files == 0 {
                "CLEAN".to_string()
            } else {
                format!("DIRTY ({dirty_files} uncommitted/untracked files)")
            }
        }
        _ => "N/A (not a git repository)".to_string(),
    };

    println!("=== Claim Verification: {agent_id} ===");
    println!("Edits:                {edits_count}");
    println!("Test After Last Edit: {test_after_edit}");
    println!("Commits Succeeded:    {commits_succeeded}");
    println!("Pushes Succeeded:     {pushes_succeeded}");
    println!("Claim Stall Flags:    {claim_str}");
    println!("Git Worktree Status:  {git_status}");
}

/// Executes `tacho watch [--json] [--until stalled|idle|exit]`.
pub fn execute_watch(args: WatchArgs) -> io::Result<()> {
    let client = DaemonClient::new();
    let mut stream = client.connect_api()?;

    let req = serde_json::json!({
        "cmd": "watch",
        "until": args.until,
    });
    let line =
        serde_json::to_string(&req).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if args.json {
            println!("{trimmed}");
        } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(state_text) = val.get("state").and_then(|v| v.as_str()) {
                println!("{state_text}");
            } else if let Some(matched) = val.get("matched").and_then(|v| v.as_str()) {
                println!("Target state reached: {matched}");
                break;
            }
        } else {
            println!("{trimmed}");
        }
    }

    Ok(())
}
