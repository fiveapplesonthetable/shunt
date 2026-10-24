//! Per-agent flight recorder `report`, `--diff-claims` verification, and live `watch` stream.

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tacho_core::Scrubber;
use tacho_daemon::{render_report, EventLog, Recorder};

use crate::cli::{ReportArgs, WatchArgs};
use crate::client::DaemonClient;

/// Resolves the SQLite WAL database path (`$TACHO_HOME/events.sqlite`).
pub(crate) fn resolve_events_db() -> PathBuf {
    let home = crate::paths::resolve_home();
    let sqlite = home.join("events.sqlite");
    if sqlite.exists() {
        sqlite
    } else {
        let db = home.join("events.db");
        if db.exists() {
            db
        } else {
            sqlite
        }
    }
}

/// Executes `tacho report <agent> [--since 30m] [--diff-claims]`.
pub fn execute_report(args: ReportArgs) -> io::Result<()> {
    let scrubber = Scrubber::new();
    let client = DaemonClient::new();
    let since_ns = args
        .since
        .as_deref()
        .and_then(crate::trace::parse_since_duration_ns);

    let resp = match client.send_request(&serde_json::json!({
        "cmd": "report",
        "agent": args.agent,
        "since": since_ns,
        "diff_claims": args.diff_claims,
    })) {
        Ok(r) => r,
        Err(_) => {
            let db_path = resolve_events_db();
            let events = if db_path.exists() {
                EventLog::open(&db_path)
                    .and_then(|log| log.load_all())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let recorder = Recorder::restore(events.iter());
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let text = render_report(recorder.live_state(), &args.agent, since_ns, now_ns);
            let agent_state = recorder.live_state().agents.get(&args.agent);
            serde_json::json!({
                "status": "ok",
                "output": text,
                "agent_state": agent_state,
            })
        }
    };

    if args.diff_claims {
        print_diff_claims(&args.agent, &resp);
        return Ok(());
    }

    if let Some(output) = resp.get("output").and_then(|v| v.as_str()) {
        println!("{}", scrubber.scrub(output));
    } else {
        let raw = serde_json::to_string_pretty(&resp).unwrap_or_default();
        println!("{}", scrubber.scrub(&raw));
    }

    Ok(())
}

/// Prints a compact <100 token claim verification summary table.
fn print_diff_claims(agent_id: &str, resp: &serde_json::Value) {
    let agent_state = resp.get("agent_state");

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

    let out = format!(
        "=== Claim Verification: {agent_id} ===\n\
         Edits:                {edits_count}\n\
         Test After Last Edit: {test_after_edit}\n\
         Commits Succeeded:    {commits_succeeded}\n\
         Pushes Succeeded:     {pushes_succeeded}\n\
         Claim Stall Flags:    {claim_str}\n\
         Git Worktree Status:  {git_status}"
    );
    println!("{}", Scrubber::new().scrub(&out));
}

/// Executes `tacho watch [--json] [--until stalled|idle|exit]`.
pub fn execute_watch(args: WatchArgs) -> io::Result<()> {
    let scrubber = Scrubber::new();
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
            println!("{}", scrubber.scrub(trimmed));
        } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(state_text) = val.get("state").and_then(|v| v.as_str()) {
                println!("{}", scrubber.scrub(state_text));
            } else if let Some(matched) = val.get("matched").and_then(|v| v.as_str()) {
                println!("Target state reached: {}", scrubber.scrub(matched));
                break;
            }
        } else {
            println!("{}", scrubber.scrub(trimmed));
        }
    }

    Ok(())
}
