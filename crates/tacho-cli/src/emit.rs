//! Event emission command (`tacho emit`).

use std::fs;
use std::io::{self, BufRead};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::{EmitArgs, EmitSubcommands, SpanAction};
use crate::client::DaemonClient;

/// Executes `tacho emit`.
pub fn execute_emit(args: EmitArgs) -> Result<(), String> {
    let client = DaemonClient::new();
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    if args.json {
        let stdin = io::stdin();
        let reader = stdin.lock();
        let mut count = 0;
        for line in reader.lines() {
            let line = line.map_err(|e| format!("failed reading stdin: {e}"))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let input: serde_json::Value =
                serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON line: {e}"))?;

            client
                .send_request(&serde_json::json!({ "input": input }))
                .map_err(|e| format!("failed to send input to daemon: {e}"))?;
            count += 1;
        }
        println!("Emitted {count} events from stdin.");
        return Ok(());
    }

    let sub = match args.subcommand {
        Some(s) => s,
        None => {
            return Err(
                "must specify a subcommand or --json. See `tacho emit --help`.".to_string(),
            );
        }
    };

    let input = match sub {
        EmitSubcommands::Task {
            slug,
            paths,
            brief_file,
        } => {
            let description = if let Some(bf) = brief_file {
                fs::read_to_string(&bf).ok()
            } else {
                None
            };
            serde_json::json!({
                "type": "task_update",
                "source_ts_ns": now_ns,
                "task": slug,
                "status": "in_progress",
                "paths": paths.map(|p| p.split(',').map(String::from).collect::<Vec<_>>()),
                "description": description,
            })
        }
        EmitSubcommands::State {
            target,
            state,
            because,
        } => {
            serde_json::json!({
                "type": "custom",
                "source_ts_ns": now_ns,
                "track": target,
                "op": "state",
                "name": state,
                "args": because.map(|b| vec![serde_json::json!({"key": "because", "value": b})]),
            })
        }
        EmitSubcommands::Span { action } => match action {
            SpanAction::Begin {
                track,
                name,
                id,
                args,
            } => {
                let parsed_args = parse_kv_args(&args);
                serde_json::json!({
                    "type": "custom",
                    "source_ts_ns": now_ns,
                    "track": track,
                    "op": "begin",
                    "name": name,
                    "id": id,
                    "args": parsed_args,
                })
            }
            SpanAction::End { track, id, args } => {
                let parsed_args = parse_kv_args(&args);
                serde_json::json!({
                    "type": "custom",
                    "source_ts_ns": now_ns,
                    "track": track,
                    "op": "end",
                    "id": id,
                    "args": parsed_args,
                })
            }
        },
        EmitSubcommands::Instant { track, name, args } => {
            let parsed_args = parse_kv_args(&args);
            serde_json::json!({
                "type": "custom",
                "source_ts_ns": now_ns,
                "track": track,
                "op": "instant",
                "name": name,
                "args": parsed_args,
            })
        }
        EmitSubcommands::Counter { track, value, unit } => {
            serde_json::json!({
                "type": "custom",
                "source_ts_ns": now_ns,
                "track": track,
                "op": "counter",
                "counter_value": value,
                "unit": unit,
            })
        }
        EmitSubcommands::Message {
            from,
            to,
            bytes,
            id,
        } => {
            serde_json::json!({
                "type": "message",
                "source_ts_ns": now_ns,
                "from": from,
                "to": to,
                "bytes": bytes,
                "id": id,
            })
        }
        EmitSubcommands::Finding {
            task,
            severity,
            path,
            line,
            title,
        } => {
            serde_json::json!({
                "type": "finding",
                "source_ts_ns": now_ns,
                "task": task,
                "severity": severity,
                "path": path,
                "line": line,
                "title": title,
            })
        }
    };

    client
        .send_request(&serde_json::json!({ "input": input }))
        .map_err(|e| format!("failed to send input to daemon: {e}"))?;

    Ok(())
}

fn parse_kv_args(args: &[String]) -> Vec<serde_json::Value> {
    args.iter()
        .filter_map(|s| {
            let mut parts = s.splitn(2, '=');
            let k = parts.next()?.trim();
            let v = parts.next()?.trim();
            Some(serde_json::json!({ "key": k, "value": v }))
        })
        .collect()
}
