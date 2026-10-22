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

/// Executes `tacho report <agent> [--since 30m]`.
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
