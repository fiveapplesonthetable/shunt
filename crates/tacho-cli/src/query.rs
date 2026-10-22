//! SQL query runner (`tacho query <name|file|-> [--trace FILE]`).

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::QueryArgs;
use crate::client::DaemonClient;

/// Executes `tacho query <query> [--trace FILE]`.
pub fn execute_query(args: QueryArgs) -> Result<(), String> {
    let tp_bin = locate_trace_processor()?;

    // 1. Resolve trace file
    let temp_dir;
    let trace_file = match args.trace {
        Some(path) => path,
        None => {
            // Snapshot live trace to temporary file
            temp_dir =
                tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;
            let tmp_path = temp_dir.path().join("snapshot.pftrace");
            let mut file = fs::File::create(&tmp_path)
                .map_err(|e| format!("failed to create temp trace: {e}"))?;
            let client = DaemonClient::new();
            client
                .fetch_trace(None, false, &mut file)
                .map_err(|e| format!("failed to snapshot trace from daemon: {e}"))?;
            tmp_path
        }
    };

    // 2. Resolve query SQL string
    let sql = resolve_query_sql(&args.query)?;

    // 3. Execute trace_processor_shell <trace_file> -q - via stdin
    let mut child = Command::new(&tp_bin)
        .arg(&trace_file)
        .arg("-q")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn trace_processor_shell: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(sql.as_bytes())
            .map_err(|e| format!("failed to write query to stdin: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for trace_processor_shell: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !output.status.success() {
        eprint!("{stderr}");
        return Err(format!("query failed with {}", output.status));
    }

    Ok(())
}

fn locate_trace_processor() -> Result<PathBuf, String> {
    if let Ok(path) = env::var("TACHO_TRACE_PROCESSOR").or_else(|_| env::var("TACHO_TP")) {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Ok(p);
        }
    }

    if let Ok(paths) = env::var("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join("trace_processor_shell");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let default_path = PathBuf::from(
        "/usr/local/google/home/zezeozue/dev/perfetto/out/release/trace_processor_shell",
    );
    if default_path.is_file() {
        return Ok(default_path);
    }

    Err(
        "trace_processor_shell binary not found; set $TACHO_TRACE_PROCESSOR, $TACHO_TP, or install Perfetto"
            .to_string(),
    )
}

/// Returns the embedded PerfettoSQL query text for a named query if known.
pub fn get_embedded_query(name: &str) -> Option<&'static str> {
    match name {
        "agents-now" | "agents-now.sql" => Some(include_str!("../../../queries/agents-now.sql")),
        "agents-now.pipe" | "agents-now.pipe.sql" => {
            Some(include_str!("../../../queries/agents-now.pipe.sql"))
        }
        "open-waits" | "open-waits.sql" => Some(include_str!("../../../queries/open-waits.sql")),
        "open-waits.pipe" | "open-waits.pipe.sql" => {
            Some(include_str!("../../../queries/open-waits.pipe.sql"))
        }
        "failing-commands" | "failing-commands.sql" => {
            Some(include_str!("../../../queries/failing-commands.sql"))
        }
        "failing-commands.pipe" | "failing-commands.pipe.sql" => {
            Some(include_str!("../../../queries/failing-commands.pipe.sql"))
        }
        "time-by-class" | "time-by-class.sql" => {
            Some(include_str!("../../../queries/time-by-class.sql"))
        }
        "time-by-class.pipe" | "time-by-class.pipe.sql" => {
            Some(include_str!("../../../queries/time-by-class.pipe.sql"))
        }
        "edits-by-file" | "edits-by-file.sql" => {
            Some(include_str!("../../../queries/edits-by-file.sql"))
        }
        "edits-by-file.pipe" | "edits-by-file.pipe.sql" => {
            Some(include_str!("../../../queries/edits-by-file.pipe.sql"))
        }
        "tokens-by-agent" | "tokens-by-agent.sql" => {
            Some(include_str!("../../../queries/tokens-by-agent.sql"))
        }
        "tokens-by-agent.pipe" | "tokens-by-agent.pipe.sql" => {
            Some(include_str!("../../../queries/tokens-by-agent.pipe.sql"))
        }
        "stalls" | "stalls.sql" => Some(include_str!("../../../queries/stalls.sql")),
        "stalls.pipe" | "stalls.pipe.sql" => Some(include_str!("../../../queries/stalls.pipe.sql")),
        "system-during-builds" | "system-during-builds.sql" => {
            Some(include_str!("../../../queries/system-during-builds.sql"))
        }
        "system-during-builds.pipe" | "system-during-builds.pipe.sql" => Some(include_str!(
            "../../../queries/system-during-builds.pipe.sql"
        )),
        "contention" | "contention.sql" => Some(include_str!("../../../queries/contention.sql")),
        "contention.pipe" | "contention.pipe.sql" => {
            Some(include_str!("../../../queries/contention.pipe.sql"))
        }
        "verified-trees" | "verified-trees.sql" => {
            Some(include_str!("../../../queries/verified-trees.sql"))
        }
        "verified-trees.pipe" | "verified-trees.pipe.sql" => {
            Some(include_str!("../../../queries/verified-trees.pipe.sql"))
        }
        _ => None,
    }
}

pub fn resolve_query_sql(query_arg: &str) -> Result<String, String> {
    if query_arg == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read SQL from stdin: {e}"))?;
        return Ok(buf);
    }

    // 1. Check embedded shipped queries
    if let Some(embedded) = get_embedded_query(query_arg) {
        return Ok(embedded.to_string());
    }

    // 2. Check if it is an existing file path
    let path = Path::new(query_arg);
    if path.is_file() {
        return fs::read_to_string(path)
            .map_err(|e| format!("failed to read query file '{}': {e}", path.display()));
    }

    // 3. Treat as raw SQL string if it looks like SQL
    let trimmed = query_arg.trim();
    let upper = trimmed.to_uppercase();
    if upper.starts_with("SELECT")
        || upper.starts_with("FROM")
        || upper.starts_with("WITH")
        || upper.starts_with("PERFETTO")
        || upper.starts_with("INTERVAL")
        || upper.starts_with("INCLUDE")
    {
        return Ok(query_arg.to_string());
    }

    Err(format!(
        "could not find shipped query or file matching '{query_arg}'. Shipped queries: agents-now, open-waits, failing-commands, time-by-class, edits-by-file, tokens-by-agent, stalls, system-during-builds, contention, verified-trees (or .pipe variants)"
    ))
}
