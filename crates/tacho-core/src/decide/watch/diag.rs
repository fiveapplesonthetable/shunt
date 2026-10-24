//! Pure diagnostics for `watch` (R69–R71):
//! - Kernel thread waits (`wchan`) and `/proc/locks` lock-holder diagnosis (`waits`, `proc_lock_holders`)
//! - Log scrubbing (`redact_text`, `scrub`, `tail`, `lock_waits`, `lock_holders`)
//! - Incremental transcript pricing & classification (`project_slug`, `classify_transcript`, `parse_transcript_line`, `consume_transcript_bytes`, `price_transcript`, `once_records`, `summarize_transcripts`, `codex_runs`)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::procs::{below, Proc};

/// Default lock-file suffixes watched in agent `/proc/<pid>/fd/*` links (R69).
pub const LOCK_FILES: &[&str] = &[".cargo-lock", ".package-cache", "gradle.lock", ".lock"];

/// Default build tool names surfaced as `/proc/locks` write-lock holders (R70).
pub const DEFAULT_LOCK_HOLDERS: &[&str] = &[
    "cargo", "rustc", "rustup", "docker", "npm", "node", "gradle", "java", "sccache",
];

/// Default number of scrubbed lines kept by [`tail`] (R70).
pub const TAIL_LINES: usize = 60;

/// Maximum character length per scrubbed line in [`scrub`] (R70).
pub const LINE_CHARS: usize = 240;

/// Redaction placeholder string (R70).
pub const REDACT_MARK: &str = "[redacted]";

/// Result of inspecting an agent tree's thread `wchan`s and open lock files (R69).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Waits {
    pub waiting_on: Vec<String>,
    pub lock_holder: String,
}

/// Find who holds each lock on `inodes` from `/proc/locks` text: `inode -> pid`,
/// skipping holders inside `inside` and waiter rows (`->`) (R69).
pub fn proc_lock_holders(
    locks_text: &str,
    inodes: &BTreeSet<u64>,
    inside: &BTreeSet<u32>,
) -> BTreeMap<u64, u32> {
    let mut out = BTreeMap::new();
    for line in locks_text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.contains(&"->") || fields.len() < 6 {
            continue;
        }
        let Ok(pid) = fields[4].parse::<u32>() else {
            continue;
        };
        let Some((_, inode_str)) = fields[5].rsplit_once(':') else {
            continue;
        };
        let Ok(inode) = inode_str.parse::<u64>() else {
            continue;
        };
        if inodes.contains(&inode) && !inside.contains(&pid) {
            out.insert(inode, pid);
        }
    }
    out
}

/// Compute what an agent's process tree is blocked on: sorted kernel wait names (`wchan`)
/// of its threads, and the command holding a Cargo/Gradle lock file it has open (R69).
///
/// `thread_wchans` maps `pid -> list of wchan strings`.
/// `open_fds` maps `pid -> list of (target_path, inode)`.
pub fn waits(
    table: &[Proc],
    root: u32,
    thread_wchans: &BTreeMap<u32, Vec<String>>,
    open_fds: &BTreeMap<u32, Vec<(String, u64)>>,
    locks_text: &str,
) -> Waits {
    let tree_pids: BTreeSet<u32> = below(table, root).into_iter().map(|p| p.id.pid).collect();
    let mut names = BTreeSet::new();
    let mut inodes = BTreeSet::new();

    for &pid in &tree_pids {
        if let Some(wchans) = thread_wchans.get(&pid) {
            for w in wchans {
                let w = w.trim();
                if !w.is_empty() && w != "0" {
                    names.insert(w.to_string());
                }
            }
        }
        if let Some(fds) = open_fds.get(&pid) {
            for (path, ino) in fds {
                if LOCK_FILES.iter().any(|suffix| path.ends_with(suffix)) {
                    inodes.insert(*ino);
                }
            }
        }
    }

    let holders = proc_lock_holders(locks_text, &inodes, &tree_pids);
    let by_pid: BTreeMap<u32, &str> = table.iter().map(|p| (p.id.pid, p.name.as_str())).collect();
    let lock_holder = holders
        .values()
        .next()
        .map(|pid| by_pid.get(pid).copied().unwrap_or("unknown").to_string())
        .unwrap_or_default();

    Waits {
        waiting_on: names.into_iter().collect(),
        lock_holder,
    }
}

static PRIV_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)")
        .unwrap()
});
static TOKEN_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:gh[pousr]_|github_pat_|sk-|xox[bap]-)[A-Za-z0-9_-]{10,}").unwrap()
});
static AKIA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap());
static JWT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\beyJ[\w-]{8,}\.[\w-]{8,}\.[\w-]*").unwrap());
static AUTH_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)(\s+)[A-Za-z0-9._~+/=-]{12,}").unwrap());
static KV_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(\b(?:token|secret|password|passwd|api[_-]?key|authorization)[=:]\s?)[^\s"',;]+"#,
    )
    .unwrap()
});
static CRED_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[^\w/])((?:~|/)[\w.@/-]*(?:credential|secret|token|passw|id_rsa|id_ed25519|\.pem\b|\.key\b|gcs-[\w-]+\.json|/\.config/[\w-]+/env)[\w.@/-]*)")
        .unwrap()
});

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-z][a-z0-9+.-]*://\S+").unwrap());
static HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:[a-z0-9-]+\.)+(?:com|net|org|io|ai|dev|app|internal|local|lan|ts\.net)\b")
        .unwrap()
});
static ADDRESS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b").unwrap());
static PHONE_CANDIDATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+?\d[\d ()-]{8,}\d").unwrap());
static PATH_CANDIDATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~?/(?:[\w.@+-]+/)+([\w.@+-]*)").unwrap());
static LOCK_WAIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Blocking waiting for file lock on ([\w ]+)").unwrap());

/// Redact credential shapes and configured literal secrets from `value` (R70).
pub fn redact_text(value: &str, literals: &[&str]) -> String {
    let mut out = value.to_string();
    for lit in literals {
        if !lit.is_empty() {
            out = out.replace(lit, REDACT_MARK);
        }
    }
    out = PRIV_KEY_RE.replace_all(&out, REDACT_MARK).into_owned();
    out = TOKEN_PREFIX_RE.replace_all(&out, REDACT_MARK).into_owned();
    out = AKIA_RE.replace_all(&out, REDACT_MARK).into_owned();
    out = JWT_RE.replace_all(&out, REDACT_MARK).into_owned();
    out = AUTH_SCHEME_RE
        .replace_all(&out, |c: &regex::Captures<'_>| {
            format!("{}{REDACT_MARK}", &c[1])
        })
        .into_owned();
    out = KV_SECRET_RE
        .replace_all(&out, |c: &regex::Captures<'_>| {
            format!("{}{REDACT_MARK}", &c[1])
        })
        .into_owned();
    out = CRED_PATH_RE
        .replace_all(&out, |c: &regex::Captures<'_>| {
            format!("{}{REDACT_MARK}", &c[1])
        })
        .into_owned();
    out
}

fn is_word_or_dot(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'
}

fn is_word_dot_or_colon(ch: char) -> bool {
    is_word_or_dot(ch) || ch == ':'
}

fn replace_phones(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for m in PHONE_CANDIDATE_RE.find_iter(input) {
        let prev_ok = input[..m.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_or_dot(c));
        let next_ok = input[m.end()..]
            .chars()
            .next()
            .is_none_or(|c| !is_word_or_dot(c));
        if prev_ok && next_ok {
            out.push_str(&input[last..m.start()]);
            out.push_str("<number>");
            last = m.end();
        }
    }
    out.push_str(&input[last..]);
    out
}

fn replace_paths(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for caps in PATH_CANDIDATE_RE.captures_iter(input) {
        let m = caps.get(0).unwrap();
        let prev_ok = input[..m.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_dot_or_colon(c));
        if prev_ok {
            out.push_str(&input[last..m.start()]);
            out.push_str("<path>/");
            out.push_str(&caps[1]);
            last = m.end();
        }
    }
    out.push_str(&input[last..]);
    out
}

/// Scrub one log line: strip ANSI codes, redact credentials, and replace URLs (`<url>`),
/// hosts (`<host>`), IPv4 addresses (`<address>`), phone numbers (`<number>`), and directory
/// prefixes (`<path>/<filename>`), truncated to 240 chars (R70).
pub fn scrub(line: &str) -> String {
    let redacted = redact_text(line, &[]);
    let no_ansi = ANSI_RE.replace_all(&redacted, "");
    let no_url = URL_RE.replace_all(&no_ansi, "<url>");
    let no_host = HOST_RE.replace_all(&no_url, "<host>");
    let no_addr = ADDRESS_RE.replace_all(&no_host, "<address>");
    let no_phone = replace_phones(&no_addr);
    let no_path = replace_paths(&no_phone);
    no_path.chars().take(LINE_CHARS).collect()
}

/// Return the last `lines` non-empty lines of `log`, each scrubbed via [`scrub`] (R70).
pub fn tail(log: &str, lines: usize) -> Vec<String> {
    let non_empty: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = non_empty.len().saturating_sub(lines);
    non_empty[start..].iter().map(|l| scrub(l)).collect()
}

/// Extract sorted unique Cargo lock kinds from `Blocking waiting for file lock on <X>` lines (R70).
pub fn lock_waits(log: &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    for caps in LOCK_WAIT_RE.captures_iter(log) {
        set.insert(caps[1].trim().to_string());
    }
    set.into_iter().collect()
}

/// Return sorted unique build-tool names holding `FLOCK` or `POSIX` `WRITE` locks in `/proc/locks`;
/// PIDs never leave the machine (R70).
pub fn lock_holders<F>(proc_locks: &str, mut comm_of: F) -> Vec<String>
where
    F: FnMut(u32) -> String,
{
    let allowed: BTreeSet<&str> = DEFAULT_LOCK_HOLDERS.iter().copied().collect();
    let mut names = BTreeSet::new();
    for row in proc_locks.lines() {
        let fields: Vec<&str> = row.split_whitespace().collect();
        if fields.len() > 5 && matches!(fields[1], "FLOCK" | "POSIX") && fields[3] == "WRITE" {
            if let Ok(pid) = fields[4].parse::<u32>() {
                let name = comm_of(pid);
                if allowed.contains(name.as_str()) {
                    names.insert(name);
                }
            }
        }
    }
    names.into_iter().collect()
}

/// Seconds in an hour, day, and week for transcript retention and cost summaries (R71).
pub const HOUR_SECS: i64 = 3_600;
pub const DAY_SECS: i64 = 24 * 3_600;
pub const WEEK_SECS: i64 = 7 * 24 * 3_600;
pub const TRANSCRIPT_KEEP_SECS: i64 = WEEK_SECS + DAY_SECS;

/// Where a transcript record originated (R71).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptPlace {
    Orchestrator,
    Workers,
    Subagents,
}

impl TranscriptPlace {
    pub const fn as_str(&self) -> &'static str {
        match self {
            TranscriptPlace::Orchestrator => "orchestrator",
            TranscriptPlace::Workers => "workers",
            TranscriptPlace::Subagents => "subagents",
        }
    }
}

/// Token usage from one assistant message in a transcript (R71).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl TokenUsage {
    pub const fn context_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

/// Parsed and priced assistant transcript record (R71).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    pub id: String,
    pub at: f64,
    pub model: String,
    pub cost: f64,
    pub place: String,
    pub key: String,
    pub ctx: u64,
}

/// Incremental per-file transcript cache entry (R71).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptCacheEntry {
    pub offset: u64,
    pub records: Vec<TranscriptRecord>,
}

/// Top task spend entry for today (R71).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopKeySpend {
    pub key: String,
    pub usd: f64,
}

/// Spend summary across the last hour, today, and the last 7 days (R71).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptCostSummary {
    pub hour: f64,
    pub today: f64,
    pub week: f64,
    pub by_model: BTreeMap<String, f64>,
    pub by_place: BTreeMap<String, f64>,
    pub top_today: Vec<TopKeySpend>,
    pub per_pr: BTreeMap<String, f64>,
}

/// Convert a working directory path into the project directory slug (`/` and `.` replaced with `-`) (R71).
pub fn project_slug(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

/// Classify a transcript path relative to the projects directory into `(place, agent_key)` (R71).
pub fn classify_transcript(
    rel: &str,
    repo_slug: &str,
    worktrees_by_slug: &BTreeMap<String, String>,
) -> (TranscriptPlace, String) {
    let project = rel.split('/').next().unwrap_or("");
    if rel.contains("/subagents/") {
        return (
            TranscriptPlace::Subagents,
            worktrees_by_slug.get(project).cloned().unwrap_or_default(),
        );
    }
    if project == repo_slug {
        return (TranscriptPlace::Orchestrator, String::new());
    }
    (
        TranscriptPlace::Workers,
        worktrees_by_slug.get(project).cloned().unwrap_or_default(),
    )
}

/// Default model rates `(input, output, cache_read)` in USD per million tokens;
/// cache writes are billed at `2 * input` (R71).
pub fn model_rates(model: &str) -> (f64, f64, f64) {
    match model {
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4" => (5.0, 25.0, 0.50),
        "claude-sonnet-5-5" | "claude-sonnet-5" | "claude-sonnet-4" => (2.0, 10.0, 0.20),
        "claude-haiku-4-5-20251001" => (1.0, 5.0, 0.10),
        "claude-haiku-5-5" => (0.1, 0.5, 0.01),
        "claude-fable-5-1" => (10.0, 50.0, 0.25),
        _ => (0.0, 0.0, 0.0),
    }
}

/// Price one message's token usage in USD; an unknown model costs `0.0` rather than a guess (R71).
pub fn price_transcript(model: &str, usage: &TokenUsage) -> f64 {
    let (rate_in, rate_out, rate_read) = model_rates(model);
    ((usage.input_tokens as f64) * rate_in
        + (usage.output_tokens as f64) * rate_out
        + (usage.cache_read_input_tokens as f64) * rate_read
        + (usage.cache_creation_input_tokens as f64) * rate_in * 2.0)
        / 1e6
}

/// Deduplicate records by message `id` so resumed sessions do not double-count messages (R71).
pub fn once_records(records: &[TranscriptRecord]) -> Vec<TranscriptRecord> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for r in records {
        if seen.insert(r.id.clone()) {
            out.push(r.clone());
        }
    }
    out
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Parse one JSONL transcript line into a [`TranscriptRecord`] if it is an `"assistant"` record (R71).
pub fn parse_transcript_line(line: &str, place: &str, key: &str) -> Option<TranscriptRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let m = v.get("message")?;
    let id = m.get("id")?.as_str()?.to_string();
    let model = m
        .get("model")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let at = v.get("at").and_then(|x| x.as_f64()).or_else(|| {
        let ts = v.get("timestamp")?.as_str()?;
        parse_iso8601_epoch(ts)
    })?;
    let u = m.get("usage");
    let tok = |k| {
        u.and_then(|x| x.get(k))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    let usage = TokenUsage {
        input_tokens: tok("input_tokens"),
        output_tokens: tok("output_tokens"),
        cache_read_input_tokens: tok("cache_read_input_tokens"),
        cache_creation_input_tokens: tok("cache_creation_input_tokens"),
    };
    Some(TranscriptRecord {
        id,
        at,
        cost: price_transcript(&model, &usage),
        model,
        place: place.to_string(),
        key: key.to_string(),
        ctx: usage.context_tokens(),
    })
}

fn parse_iso8601_epoch(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .unwrap_or(s);
    let (date_part, time_part) = s.split_once('T')?;
    let mut dp = date_part.split('-');
    let (year, month, day): (i64, i64, i64) = (
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
    );
    let mut tp = time_part.split(':');
    let (hour, min, sec): (i64, i64, f64) = (
        tp.next()?.parse().ok()?,
        tp.next()?.parse().ok()?,
        tp.next()?.parse().ok()?,
    );
    let days = days_from_civil(year, month, day)?;
    Some((days * 86_400 + hour * 3_600 + min * 60) as f64 + sec)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Incremental transcript reader: consumes complete newline-terminated lines from `file_bytes`
/// starting at `entry.offset` (resetting to 0 if the file was truncated) (R71).
pub fn consume_transcript_bytes(
    file_bytes: &[u8],
    entry: &TranscriptCacheEntry,
    place: &str,
    key: &str,
) -> TranscriptCacheEntry {
    let base = if (file_bytes.len() as u64) < entry.offset {
        TranscriptCacheEntry::default()
    } else {
        entry.clone()
    };
    let slice = &file_bytes[base.offset as usize..];
    let Some(last_nl) = slice.iter().rposition(|&b| b == b'\n') else {
        return base;
    };
    let end = last_nl + 1;
    let text = String::from_utf8_lossy(&slice[..end]);
    let mut records = base.records;
    for line in text.lines() {
        if let Some(rec) = parse_transcript_line(line, place, key) {
            records.push(rec);
        }
    }
    TranscriptCacheEntry {
        offset: base.offset + (end as u64),
        records,
    }
}

/// Summarize transcript spend across the last hour, today, and the last 7 days (R71).
pub fn summarize_transcripts(
    records: &[TranscriptRecord],
    now: f64,
    day_start: f64,
    pr_of_key: &BTreeMap<String, u64>,
) -> TranscriptCostSummary {
    let unique = once_records(records);
    let week: Vec<&TranscriptRecord> = unique
        .iter()
        .filter(|r| now - r.at < WEEK_SECS as f64)
        .collect();
    let today: Vec<&TranscriptRecord> =
        week.iter().copied().filter(|r| r.at >= day_start).collect();
    let hour: Vec<&TranscriptRecord> = week
        .iter()
        .copied()
        .filter(|r| now - r.at < HOUR_SECS as f64)
        .collect();

    let mut by_model_raw: BTreeMap<String, f64> = BTreeMap::new();
    let mut by_place_raw: BTreeMap<String, f64> = BTreeMap::new();
    let mut per_pr_raw: BTreeMap<u64, f64> = BTreeMap::new();

    for r in &week {
        *by_model_raw.entry(r.model.clone()).or_insert(0.0) += r.cost;
        *by_place_raw.entry(r.place.clone()).or_insert(0.0) += r.cost;
        if let Some(&pr) = pr_of_key.get(&r.key) {
            *per_pr_raw.entry(pr).or_insert(0.0) += r.cost;
        }
    }

    let mut by_key_today: BTreeMap<String, f64> = BTreeMap::new();
    for r in &today {
        if !r.key.is_empty() {
            *by_key_today.entry(r.key.clone()).or_insert(0.0) += r.cost;
        }
    }
    let mut top_vec: Vec<(String, f64)> = by_key_today.into_iter().collect();
    top_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top_today = top_vec
        .into_iter()
        .take(5)
        .map(|(key, v)| TopKeySpend {
            key,
            usd: round2(v),
        })
        .collect();

    TranscriptCostSummary {
        hour: round2(hour.iter().map(|r| r.cost).sum()),
        today: round2(today.iter().map(|r| r.cost).sum()),
        week: round2(week.iter().map(|r| r.cost).sum()),
        by_model: by_model_raw
            .into_iter()
            .map(|(k, v)| (k, round2(v)))
            .collect(),
        by_place: by_place_raw
            .into_iter()
            .map(|(k, v)| (k, round2(v)))
            .collect(),
        top_today,
        per_pr: per_pr_raw
            .into_iter()
            .map(|(n, v)| (n.to_string(), round2(v)))
            .collect(),
    }
}

/// Count Codex runs started in the last hour and today (`(codex_hour, codex_today)`) (R71).
pub fn codex_runs(agents: &[(String, f64)], now: f64, day_start: f64) -> (usize, usize) {
    let mut hour = 0;
    let mut today = 0;
    for (provider, started) in agents {
        if provider == "codex" {
            hour += usize::from(now - *started < HOUR_SECS as f64);
            today += usize::from(*started >= day_start);
        }
    }
    (hour, today)
}
