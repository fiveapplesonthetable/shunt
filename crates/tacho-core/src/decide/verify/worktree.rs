//! Worker slugs, ports, env files.

use super::*;

/// Validate a worker slug (`3..=80` chars, lowercase ASCII alphanumeric separated by single hyphens).
pub fn slug_problem(slug: &str) -> Option<String> {
    if slug.len() < 3 || slug.len() > 80 {
        return Some(format!(
            "slug '{slug}' must be between 3 and 80 characters (got {})",
            slug.len()
        ));
    }
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());
    if !RE.is_match(slug) {
        return Some(format!(
            "slug '{slug}' must match ^[a-z0-9]+(-[a-z0-9]+)*$ (lowercase letters, digits, single hyphens)"
        ));
    }
    None
}

/// Allocate an isolated block of [`PORT_BLOCK_SIZE`] ports in `PORT_RANGE_START..PORT_RANGE_END`
/// that does not overlap any `used_ports` and passes `port_free`.
pub fn allocate_ports<F>(used_ports: &BTreeSet<u16>, mut port_free: F) -> Result<(u16, u16), String>
where
    F: FnMut(u16) -> bool,
{
    let mut start = PORT_RANGE_START;
    while start + PORT_BLOCK_SIZE <= PORT_RANGE_END {
        let last = start + PORT_BLOCK_SIZE - 1;
        let block_used = (start..=last).any(|p| used_ports.contains(&p));
        if !block_used && (start..=last).all(&mut port_free) {
            return Ok((start, last));
        }
        start += PORT_BLOCK_SIZE;
    }
    Err(format!(
        "no free {PORT_BLOCK_SIZE}-port block available in {PORT_RANGE_START}..{PORT_RANGE_END}"
    ))
}

/// Validate a background job name (`tacho job <name>`): 1..=40 chars matching `^[a-z0-9][a-z0-9._-]*$`.
pub fn job_name_problem(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 40 {
        return Some(format!(
            "job name '{name}' must be between 1 and 40 characters"
        ));
    }
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*$").unwrap());
    if !RE.is_match(name) {
        return Some(format!(
            "job name '{name}' must match ^[a-z0-9][a-z0-9._-]*$"
        ));
    }
    None
}

pub(crate) fn sh_quote(val: &str) -> String {
    format!("'{}'", val.replace('\'', "'\\''"))
}

/// Format the worktree `.tachoenv` file contents using POSIX `export KEY='VALUE'` syntax.
pub fn env_file(
    slug: &str,
    base_sha: &str,
    base_pr: Option<u64>,
    port: u16,
    port_last: u16,
    target_dir: Option<&str>,
) -> String {
    let base_pr_str = base_pr.map(|n| n.to_string()).unwrap_or_default();
    let mut lines = vec![
        format!("export SLUG={}", sh_quote(slug)),
        format!("export BASE_SHA={}", sh_quote(base_sha)),
        format!("export BASE_PR={}", sh_quote(&base_pr_str)),
        format!("export PORT={}", sh_quote(&port.to_string())),
        format!("export PORT_LAST={}", sh_quote(&port_last.to_string())),
    ];
    if let Some(td) = target_dir {
        if !td.is_empty() {
            lines.push(format!("export CARGO_TARGET_DIR={}", sh_quote(td)));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Parse a KEY=VALUE or `export KEY='VALUE'` `.tachoenv` file into a map, unescaping `'\''` -> `'`.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let stripped = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((k, v)) = stripped.split_once('=') {
            let raw = v.trim();
            let val = if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
                raw[1..raw.len() - 1]
                    .replace("'\\''", "'")
                    .replace("'\"'\"'", "'")
            } else if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                raw[1..raw.len() - 1].to_string()
            } else {
                raw.to_string()
            };
            out.insert(k.trim().to_string(), val);
        }
    }
    out
}

/// Prepend missing directories from `extra` onto `current_path` without duplicates.
pub fn merge_path(current_path: &str, extra: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for e in extra {
        if !e.is_empty() && !parts.iter().any(|p| p == e) {
            parts.push((*e).to_string());
        }
    }
    for seg in current_path.split(':').filter(|s| !s.is_empty()) {
        if !parts.iter().any(|p| p == seg) {
            parts.push(seg.to_string());
        }
    }
    parts.join(":")
}

fn normalize_lexical(path: &Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Check whether `child` is inside `worktree` (pure lexical normalization of `.` and `..`).
pub fn owns(child: &Path, worktree: &Path) -> bool {
    normalize_lexical(child).starts_with(normalize_lexical(worktree))
}

/// R50: pure fact-driven wait decision (`wait_until`, preview readiness, service health, postgres readiness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitDecision {
    /// Fact has been observed; stop waiting immediately.
    Ready { reason: String },
    /// Fact has not been observed yet and budget remains; poll again after `poll_ms`.
    Continue { poll_ms: u64, reason: String },
    /// Fact was not observed before the deadline or the producer process died.
    Aborted { reason: String },
}

/// R50: evaluate a generic fact-driven wait step (`wait_until`).
pub fn wait_until_fact(
    fact_met: bool,
    elapsed_ms: u64,
    timeout_ms: u64,
    poll_ms: u64,
    what: &str,
) -> WaitDecision {
    if fact_met {
        WaitDecision::Ready {
            reason: format!("fact '{what}' observed at {elapsed_ms}ms"),
        }
    } else if elapsed_ms >= timeout_ms {
        WaitDecision::Aborted {
            reason: format!(
                "timed out after {elapsed_ms}ms waiting for '{what}' (limit {timeout_ms}ms)"
            ),
        }
    } else {
        WaitDecision::Continue {
            poll_ms: poll_ms.max(10),
            reason: format!("waiting for '{what}' ({elapsed_ms}ms / {timeout_ms}ms)"),
        }
    }
}

/// R50: two-phase process-group termination decision (`SIGTERM` then `SIGKILL` on timeout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillPhaseDecision {
    /// Process group already exited.
    Exited { reason: String },
    /// Send `SIGTERM` to `-pgid` and wait for exit.
    SendSigTerm { pgid: u32, reason: String },
    /// Grace period expired while process group is still alive; escalate to `SIGKILL` on `-pgid`.
    SendSigKill { pgid: u32, reason: String },
    /// Still within `SIGTERM` grace period; keep waiting.
    WaitAfterTerm { poll_ms: u64, reason: String },
}

/// R50: decide the next action in two-phase process-group termination (`_kill_group`).
pub fn kill_group_decision(
    pgid: u32,
    alive: bool,
    term_sent: bool,
    elapsed_since_term_ms: u64,
    grace_ms: u64,
) -> KillPhaseDecision {
    if !alive {
        return KillPhaseDecision::Exited {
            reason: format!("process group {pgid} has exited"),
        };
    }
    if !term_sent {
        return KillPhaseDecision::SendSigTerm {
            pgid,
            reason: format!("sending SIGTERM to process group -{pgid}"),
        };
    }
    if elapsed_since_term_ms >= grace_ms {
        KillPhaseDecision::SendSigKill {
            pgid,
            reason: format!(
                "process group -{pgid} still alive {elapsed_since_term_ms}ms after SIGTERM; escalating to SIGKILL"
            ),
        }
    } else {
        KillPhaseDecision::WaitAfterTerm {
            poll_ms: 50,
            reason: format!(
                "waiting for process group -{pgid} to exit after SIGTERM ({elapsed_since_term_ms}ms / {grace_ms}ms)"
            ),
        }
    }
}

/// R50: format the timeout message when a `prove` check exceeds its deadline (`TIMED OUT after Ns`).
pub fn prove_timeout_message(check_name: &str, timeout_secs: u64) -> String {
    format!("check '{check_name}' TIMED OUT after {timeout_secs}s")
}

/// R50: decide whether a preview server is ready, still starting, or failed (`preview` wait).
pub fn preview_wait_decision(
    port_listening: bool,
    process_alive: bool,
    elapsed_ms: u64,
    timeout_ms: u64,
    log_tail: &str,
) -> WaitDecision {
    if port_listening {
        return WaitDecision::Ready {
            reason: format!("preview server listening after {elapsed_ms}ms"),
        };
    }
    if !process_alive {
        return WaitDecision::Aborted {
            reason: format!("preview server exited before listening: {log_tail}"),
        };
    }
    if elapsed_ms >= timeout_ms {
        return WaitDecision::Aborted {
            reason: format!("preview server did not listen within {timeout_ms}ms: {log_tail}"),
        };
    }
    WaitDecision::Continue {
        poll_ms: 100,
        reason: format!("waiting for preview server port ({elapsed_ms}ms / {timeout_ms}ms)"),
    }
}

/// R50: decide whether a Docker service container (`postgres`, etc.) is healthy (`State.Health.Status`).
pub fn service_health_decision(
    health_status: &str,
    elapsed_ms: u64,
    timeout_ms: u64,
) -> WaitDecision {
    match health_status.trim() {
        "healthy" | "none" => WaitDecision::Ready {
            reason: format!("service container health is '{health_status}'"),
        },
        "unhealthy" | "exited" | "dead" => WaitDecision::Aborted {
            reason: format!("service container entered terminal state '{health_status}'"),
        },
        other => {
            if elapsed_ms >= timeout_ms {
                WaitDecision::Aborted {
                    reason: format!(
                        "service container still '{other}' after {elapsed_ms}ms (limit {timeout_ms}ms)"
                    ),
                }
            } else {
                WaitDecision::Continue {
                    poll_ms: 200,
                    reason: format!("service container status is '{other}'; waiting for healthy"),
                }
            }
        }
    }
}
