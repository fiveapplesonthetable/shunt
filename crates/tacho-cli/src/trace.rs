//! Trace snapshot command (`tacho trace [-o FILE] [--since 2h] [--system] [--stdout]`).

use std::fs::{self, File};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use tacho_system::Perfetto;

use crate::cli::TraceArgs;
use crate::client::DaemonClient;

/// Executes the `tacho trace` command.
pub fn execute_trace(args: TraceArgs) -> io::Result<()> {
    let client = DaemonClient::new();
    let home = crate::paths::resolve_home();

    let dur_ns = args
        .since
        .as_deref()
        .or_else(|| args.from_log.as_ref().and_then(|opt| opt.as_deref()))
        .and_then(parse_since_duration_ns);
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let since_ns = dur_ns.map(|d| now_ns.saturating_sub(d));

    let mut output_bytes = Vec::new();

    // 1. If --system requested, clone active tacho-system session first
    if args.system {
        match Perfetto::locate() {
            Ok(perfetto) => {
                let mut system_bytes = Vec::new();
                match perfetto.clone_into(&mut system_bytes) {
                    Ok(len) => {
                        eprintln!("Cloned {len} bytes of system trace.");
                        output_bytes.extend_from_slice(&system_bytes);
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to clone system trace: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: perfetto binary not located for --system: {e}");
            }
        }
    }

    // 2. Fetch tachograph snapshot or rebuild from daemon
    let from_log = args.from_log.is_some();
    let mut tacho_bytes = Vec::new();
    match client.fetch_trace(since_ns, from_log, &mut tacho_bytes) {
        Ok(_) => {}
        Err(e) => {
            tacho_bytes.clear();
            eprintln!("opening events.sqlite read-only");
            let db_path = {
                let sqlite = home.join("events.sqlite");
                if sqlite.exists() {
                    sqlite
                } else {
                    home.join("events.db")
                }
            };
            let res = if from_log {
                tacho_daemon::rebuild_from_log(&db_path, since_ns, None, &mut tacho_bytes)
            } else {
                tacho_daemon::write_snapshot(&db_path, since_ns, None, &mut tacho_bytes)
            };
            res.map_err(|snap_err| {
                io::Error::other(format!(
                    "failed to fetch trace from daemon ({e}) and fallback to events.sqlite failed ({snap_err})"
                ))
            })?;
        }
    }
    output_bytes.extend_from_slice(&tacho_bytes);

    // 3. Write output to stdout or file
    if args.stdout {
        let mut out = io::stdout();
        out.write_all(&output_bytes)?;
        out.flush()?;
    } else {
        let target_path = args.out.unwrap_or_else(|| {
            let trace_dir = home.join("trace");
            let _ = crate::paths::ensure_dir_0700(&trace_dir);
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            trace_dir.join(format!("tacho-{ts}.perfetto-trace"))
        });

        if let Some(parent) = target_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut file = File::create(&target_path)?;
        file.write_all(&output_bytes)?;
        file.flush()?;

        eprintln!(
            "Wrote {} bytes into {}",
            output_bytes.len(),
            target_path.display()
        );
    }

    Ok(())
}

/// Parses duration strings like "30s", "10m", "2h", "1d" into nanoseconds.
pub fn parse_since_duration_ns(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_part, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let num: u64 = num_part.parse().ok()?;

    let multiplier = match unit.trim() {
        "s" | "sec" | "" => 1,
        "m" | "min" => 60,
        "h" | "hr" => 3600,
        "d" | "day" => 86400,
        _ => return None,
    };

    Some(num * multiplier * 1_000_000_000)
}
