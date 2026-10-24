//! Pure rendering for CLI `status` (`status_view`), live terminal `top` (`frame`),
//! HTML status page & owner-issue mirror (`html`, `issue_text`, `issue_due`),
//! HTTP server routing (`serve_request`, `tailnet_address`), and `next_step` advisor.

use serde::{Deserialize, Serialize};

use super::snapshot::{self, Snapshot};
use super::OpsConfig;

/// Formats a duration in seconds as `45s`, `12m`, `2h15m`, `3h`, or `—` when `None`.
pub fn span(seconds: Option<i64>) -> String {
    let Some(s) = seconds else {
        return "—".into();
    };
    let s = s.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    }
}

/// Renders the one-line headline banner for a [`Snapshot`].
pub fn headline(shot: &Snapshot, now: i64) -> String {
    let age = span(Some((now - shot.updated_at).max(0)));
    format!(
        "[{}] main: {} | deploy: {} | agents: {} | batches: {} | needs_you: {} | attention: {} | age: {age}",
        shot.headline.state,
        shot.headline.main,
        shot.headline.deploy,
        shot.headline.agents_running,
        shot.headline.batches_running,
        shot.headline.needs_you,
        shot.headline.attention,
    )
}

/// Renders selected sections (or all sections when `sections` is empty) as human text (R51).
pub fn text(shot: &Snapshot, sections: &[&str], now: i64) -> Result<String, String> {
    let _ = snapshot::section(shot, sections)?;
    let want_all = sections.is_empty();
    let mut expanded = Vec::new();
    for &s in sections {
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "needs" => {
                    expanded.push("needs_you");
                    expanded.push("attention");
                }
                "progress" => {
                    expanded.push("agents");
                    expanded.push("pipeline");
                }
                other => expanded.push(other),
            }
        }
    }
    let want = |name: &str| want_all || expanded.contains(&name);
    let mut out = Vec::new();

    if want("headline") {
        out.push(headline(shot, now));
    }
    if want("needs_you") {
        out.push(format!("needs_you ({}):", shot.needs_you.len()));
        if shot.needs_you.is_empty() {
            out.push("  none".into());
        } else {
            for item in &shot.needs_you {
                out.push(format!("  - {item}"));
            }
        }
    }
    if want("attention") {
        out.push(format!("attention ({}):", shot.headline.attention));
        if shot.attention.is_empty() {
            out.push("  none".into());
        } else {
            for item in &shot.attention {
                out.push(format!("  ! {item}"));
            }
        }
    }
    if want("main") {
        let mut line = format!("main: {}", shot.main.status);
        if let Some(done) = &shot.main.last_completed {
            line.push_str(&format!(" (last: #{} {})", done.id, done.conclusion));
        }
        if let Some(dep) = &shot.main.deploy {
            line.push_str(&format!(" | deploy: #{} {}", dep.id, dep.conclusion));
        }
        out.push(line);
    }
    if want("agents") {
        out.push(format!("agents running: {}", shot.headline.agents_running));
        if let Some(arr) = shot.agents.get("running").and_then(|v| v.as_array()) {
            for a in arr {
                let k = a.get("key").and_then(|v| v.as_str()).unwrap_or("?");
                let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let el = a.get("elapsed").and_then(|v| v.as_i64());
                out.push(format!("  {k} ({kind}) {}", span(el)));
            }
        }
    }
    if want("pipeline") {
        let prs = shot
            .pipeline
            .get("open_prs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let issues = shot
            .pipeline
            .get("open_issues")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let ready = shot
            .pipeline
            .get("ready_prs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push(format!(
            "pipeline: {prs} open PR(s), {ready} ready, {issues} open issue(s)"
        ));
    }
    if want("held") {
        out.push(format!("held: {}", shot.held));
    }
    if want("flakes") {
        out.push(format!(
            "flakes ({}): {}",
            shot.flakes.len(),
            shot.flakes.join(", ")
        ));
    }
    if want("machine") {
        out.push(format!("machine: {}", shot.machine));
    }
    if want("cost") {
        out.push(format!(
            "cost: today ${:.2} ({} runs) | 24h ${:.2} ({} runs)",
            shot.cost.today_usd,
            shot.cost.today_runs,
            shot.cost.last_24h_usd,
            shot.cost.last_24h_runs
        ));
    }
    if want("quotas") {
        out.push(format!(
            "quotas: {}",
            shot.quotas
                .as_ref()
                .map(|q| q.to_string())
                .unwrap_or_else(|| "none".into())
        ));
    }
    if want("updated_at") && !want_all {
        out.push(format!("updated_at: {}", shot.updated_at));
    }
    Ok(out.join("\n"))
}

/// Pure CLI `status` view decision (R51):
/// - Missing snapshot -> exit `1`
/// - Unknown section -> exit `2`
/// - Snapshot older than 300s -> exit `1` (still renders output)
/// - Fresh snapshot -> exit `0`
pub fn status_view(
    shot: Option<&Snapshot>,
    sections: &[&str],
    as_json: bool,
    is_tty: bool,
    now: i64,
    cfg: &OpsConfig,
) -> (i32, String) {
    let Some(shot) = shot else {
        return (
            1,
            format!(
                "no status snapshot found — run `{} tick` or check `{}.service`",
                cfg.tool, cfg.tool
            ),
        );
    };
    let val = match snapshot::section(shot, sections) {
        Ok(v) => v,
        Err(e) => return (2, e),
    };
    let stale = (now - shot.updated_at) > 300;
    let code = if stale { 1 } else { 0 };
    if as_json || (!is_tty && sections.is_empty()) {
        let rendered = serde_json::to_string_pretty(&val).unwrap_or_default();
        (code, rendered)
    } else {
        match text(shot, sections, now) {
            Ok(s) => (code, s),
            Err(e) => (2, e),
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Renders a standalone auto-refreshing HTML status page with HTML escaping and secret scrubbing (R53).
pub fn html(shot: &Snapshot, literals: &[String], tool: &str) -> String {
    let mut body = String::new();
    body.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\">");
    body.push_str("<meta http-equiv=\"refresh\" content=\"30\">");
    body.push_str(&format!(
        "<title>{} status — {}</title></head><body>",
        html_escape(tool),
        html_escape(&shot.headline.state)
    ));
    body.push_str(&format!(
        "<h1>{} — {}</h1>",
        html_escape(tool),
        html_escape(&shot.headline.state)
    ));
    body.push_str(&format!(
        "<p>main: {} | deploy: {} | agents: {} | batches: {}</p>",
        html_escape(&shot.headline.main),
        html_escape(&shot.headline.deploy),
        shot.headline.agents_running,
        shot.headline.batches_running
    ));
    body.push_str("<h2>Needs You</h2><ul>");
    for item in &shot.needs_you {
        body.push_str(&format!("<li>{}</li>", html_escape(item)));
    }
    body.push_str("</ul><h2>Attention</h2><ul>");
    for item in &shot.attention {
        body.push_str(&format!("<li>{}</li>", html_escape(item)));
    }
    body.push_str(&format!(
        "</ul><p>Cost today: ${:.2} ({} runs)</p></body></html>",
        shot.cost.today_usd, shot.cost.today_runs
    ));
    snapshot::redact_text(&body, literals)
}

/// Renders the Markdown status comment for the owner status issue, scrubbed of secrets (R53).
pub fn issue_text(shot: &Snapshot, url: &str, literals: &[String]) -> String {
    let mut lines = vec![
        format!("### Status: `{}`", shot.headline.state),
        format!(
            "- **main**: `{}` | **deploy**: `{}` | **agents**: {} | **batches**: {}",
            shot.headline.main,
            shot.headline.deploy,
            shot.headline.agents_running,
            shot.headline.batches_running
        ),
    ];
    if !url.is_empty() {
        lines.push(format!("- **live page**: {url}"));
    }
    if !shot.needs_you.is_empty() {
        lines.push("\n#### Needs You".into());
        for item in &shot.needs_you {
            lines.push(format!("- {item}"));
        }
    }
    if !shot.attention.is_empty() {
        lines.push("\n#### Attention".into());
        for item in &shot.attention {
            lines.push(format!("- {item}"));
        }
    }
    lines.push(format!("\n_updated_at: {}_", shot.updated_at));
    snapshot::redact_text(&lines.join("\n"), literals)
}

/// Strips volatile `_updated_at: ..._` lines so [`issue_due`] can detect meaningful changes (R53).
pub fn issue_core(body: &str) -> String {
    body.lines()
        .filter(|l| !l.starts_with("_updated_at:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decides whether to post/update the owner status issue mirror (R53).
pub fn issue_due(
    body: &str,
    last_core: Option<&str>,
    last_at: Option<i64>,
    now: i64,
    every: i64,
) -> bool {
    let core = issue_core(body);
    let Some(prev_core) = last_core else {
        return true;
    };
    let Some(prev_at) = last_at else {
        return true;
    };
    if core != prev_core {
        (now - prev_at) >= 60
    } else {
        (now - prev_at) >= every
    }
}

/// Extracts the first Tailscale `100.x.y.z` IPv4 address from `tailscale ip -4` or `ip addr` output (R54).
pub fn tailnet_address(output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        let ip = token.split('/').next().unwrap_or(token).trim();
        let parts: Vec<&str> = ip.split('.').collect();
        if parts.len() == 4 && parts[0] == "100" && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
            return Some(ip.to_string());
        }
    }
    None
}

/// Pure HTTP response for `tacho serve` (R54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

/// Pure HTTP routing for the status server (R54).
pub fn serve_request(method: &str, path: &str, page_html: Option<&str>) -> HttpResponse {
    let m = method.trim().to_ascii_uppercase();
    if m != "GET" && m != "HEAD" {
        return HttpResponse {
            status: 405,
            content_type: "text/plain; charset=utf-8".into(),
            body: if m == "HEAD" {
                String::new()
            } else {
                "method not allowed\n".into()
            },
        };
    }
    let clean_path = path.split('?').next().unwrap_or(path);
    let (status, content_type, full_body) = match clean_path {
        "/" | "/index.html" | "/status" => match page_html {
            Some(h) => (200, "text/html; charset=utf-8", h.to_string()),
            None => (
                503,
                "text/plain; charset=utf-8",
                "status page not ready\n".into(),
            ),
        },
        "/healthz" => (200, "text/plain; charset=utf-8", "ok\n".into()),
        _ => (404, "text/plain; charset=utf-8", "not found\n".into()),
    };
    HttpResponse {
        status,
        content_type: content_type.into(),
        body: if m == "HEAD" {
            String::new()
        } else {
            full_body
        },
    }
}

/// Builds a terminal frame of at most `rows` lines, each truncated to `cols` chars (R55).
pub fn frame(
    shot: &Snapshot,
    events: &[String],
    rows: usize,
    cols: usize,
    now: i64,
    tool: &str,
) -> Vec<String> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let mut raw = Vec::new();
    raw.push(format!("{tool} top — {}", headline(shot, now)));
    for item in &shot.needs_you {
        raw.push(format!("NEEDS YOU: {item}"));
    }
    for item in &shot.attention {
        raw.push(format!("ATTN: {item}"));
    }
    raw.push(format!(
        "cost today: ${:.2} ({} runs) | 24h: ${:.2}",
        shot.cost.today_usd, shot.cost.today_runs, shot.cost.last_24h_usd
    ));
    if !events.is_empty() && raw.len() + 1 < rows {
        raw.push("--- recent events ---".into());
        let budget = rows.saturating_sub(raw.len());
        let start = events.len().saturating_sub(budget);
        for ev in &events[start..] {
            raw.push(ev.clone());
        }
    }
    raw.into_iter()
        .take(rows)
        .map(|l| crate::char_prefix(&l, cols).to_string())
        .collect()
}

/// Inputs to [`next_step`] (R79).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NextStepInput {
    pub ready_prs: Vec<(u64, String)>,
    pub failed_batches: Vec<String>,
    pub running_batches: Vec<(String, String)>,
    pub needs_human_issues: Vec<(u64, String)>,
    pub dirty_worktrees: Vec<String>,
    pub main_broken: Option<String>,
    pub retry_exhausted: Vec<String>,
}

/// Pure single-highest-priority action recommendation (R79).
pub fn next_step(input: &NextStepInput, _cfg: &OpsConfig) -> String {
    if let Some((pr, title)) = input.ready_prs.first() {
        return format!("Review & merge ready PR #{pr}: {title}");
    }
    if let Some(id) = input.failed_batches.first() {
        return format!("Inspect failed batch {id}");
    }
    if let Some((id, phase)) = input.running_batches.first() {
        return format!("Wait for running batch {id} ({phase})");
    }
    if let Some((iss, title)) = input.needs_human_issues.first() {
        return format!("Answer needs-human issue #{iss}: {title}");
    }
    if let Some(path) = input.dirty_worktrees.first() {
        return format!("Clean or commit dirty worktree {path}");
    }
    if let Some(title) = &input.main_broken {
        return format!("Fix broken main CI ({title})");
    }
    if let Some(key) = input.retry_exhausted.first() {
        return format!("Investigate retry-exhausted {key}");
    }
    "All clear — nothing waiting on you.".into()
}
