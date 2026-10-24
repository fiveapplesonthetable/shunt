//! Flake ledger, task queuing (R24), failure staleness pruning (R47), and fix decisions.

use super::config::VerifyConfig;
use super::verdict::UntiedRecord;
use super::*;

/// Pure SHA-1 digest (20 bytes) used for deterministic 6-hex-char flake task keys (R24).
fn sha1_hex6(data: &[u8]) -> String {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }
    let _ = (h1, h2, h3, h4);
    format!("{:06x}", h0 >> 8)
}

/// R24: Build a deterministic task key for a flaky test (`flake-<slug>`).
/// Names whose slug exceeds 40 characters append a deterministic 6-hex-char SHA-1
/// suffix so long test names sharing a 40-char prefix never collide.
pub fn flake_key(test_name: &str) -> String {
    let slug: String = test_name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.len() <= 40 {
        format!("flake-{}", collapsed.trim_matches('-'))
    } else {
        let prefix: String = collapsed.chars().take(40).collect();
        let prefix = prefix.trim_end_matches('-');
        format!("flake-{prefix}-{}", sha1_hex6(test_name.as_bytes()))
    }
}

/// R24: Extract lines mentioning the last segment of `test_name` (split on `::`, ` > `, or ` › `)
/// or the full `test_name`, capped at `max_lines`.
pub fn lines_about(log: &str, test_name: &str, max_lines: usize) -> String {
    let leaf = test_name
        .rsplit("::")
        .next()
        .unwrap_or(test_name)
        .rsplit(" > ")
        .next()
        .unwrap_or(test_name)
        .rsplit(" › ")
        .next()
        .unwrap_or(test_name)
        .trim();
    let needle = if leaf.is_empty() { test_name } else { leaf };
    let mut out = Vec::new();
    for line in log.lines() {
        if (line.contains(needle) || line.contains(test_name)) && out.len() < max_lines {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Record newly observed flaky tests into the persistent flake ledger.
/// Idempotent per `batch_id`: if a test was already recorded for `batch_id`, its count is not incremented again.
pub fn record_flakes(
    ledger: &BTreeMap<String, FlakeEntry>,
    tests: &[String],
    area: &str,
    batch_id: &str,
    log: &str,
    now: i64,
) -> BTreeMap<String, FlakeEntry> {
    let mut out = ledger.clone();
    for t in tests {
        let entry = out.entry(t.clone()).or_insert_with(|| FlakeEntry {
            test: t.clone(),
            area: area.to_string(),
            count: 0,
            first_seen: now,
            last_seen: now,
            batches: Vec::new(),
            excerpt: String::new(),
            task_key: None,
        });
        if !batch_id.is_empty() && entry.batches.iter().any(|b| b == batch_id) {
            continue;
        }
        entry.count += 1;
        entry.last_seen = now;
        if !area.is_empty() {
            entry.area = area.to_string();
        }
        if !batch_id.is_empty() {
            entry.batches.push(batch_id.to_string());
        }
        let excerpt = lines_about(log, t, 12);
        if !excerpt.is_empty() {
            entry.excerpt = excerpt;
        }
    }
    out
}

/// Generate `TodoItem`s for any flaky test that has flaked `>= FLAKE_TASK_THRESHOLD` times
/// and does not already have a queued, active, or retired fix task.
pub fn flake_tasks(
    ledger: &BTreeMap<String, FlakeEntry>,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    now: i64,
) -> (BTreeMap<String, FlakeEntry>, Vec<TodoItem>) {
    plan_flake_tasks(ledger, todo, agents, retired, now, &VerifyConfig::default())
}

/// R24: configurable version of [`flake_tasks`] using `cfg.flake_threshold`, `cfg.tool_name`,
/// and `cfg.flake_model`.
pub fn plan_flake_tasks(
    ledger: &BTreeMap<String, FlakeEntry>,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    now: i64,
    cfg: &VerifyConfig,
) -> (BTreeMap<String, FlakeEntry>, Vec<TodoItem>) {
    let mut updated = ledger.clone();
    let mut new_tasks = Vec::new();
    let threshold = cfg.flake_threshold.max(1);
    for (test, entry) in updated.iter_mut() {
        if entry.count < threshold {
            continue;
        }
        let key = entry.task_key.clone().unwrap_or_else(|| flake_key(test));
        if retired.contains(&key) || todo.iter().any(|t| t.key == key) || agents.contains_key(&key)
        {
            entry.task_key = Some(key);
            continue;
        }
        entry.task_key = Some(key.clone());
        let tool = &cfg.tool_name;
        let mut brief = format!(
            "Fix flaky test `{test}` in area `{}` (flaked {} times in batches {:?}). \
Reproduce under CPU starvation (`{tool} starve` — 10 times under 4 busy loops), \
fix the underlying race or ordering assumption (never by a longer timeout), \
and if you cannot reproduce or fix it cleanly, open a draft PR explaining findings.",
            entry.area, entry.count, entry.batches
        );
        if !entry.excerpt.is_empty() {
            brief.push_str(&format!("\n\nFailure excerpt:\n{}", entry.excerpt));
        }
        new_tasks.push(TodoItem {
            key,
            brief,
            area: entry.area.clone(),
            base: None,
            after: Vec::new(),
            priority: 1,
            created_at: now,
            provider: None,
            model: Some(cfg.flake_model.clone()),
            effort: None,
            compare: None,
        });
    }
    (updated, new_tasks)
}

/// Render sorted rows for `tacho flakes`.
pub fn flake_rows(ledger: &BTreeMap<String, FlakeEntry>) -> Vec<FlakeRow> {
    let mut rows: Vec<FlakeRow> = ledger
        .values()
        .map(|e| FlakeRow {
            test: e.test.clone(),
            area: e.area.clone(),
            count: e.count,
            last_seen: e.last_seen,
            batches: e.batches.clone(),
            task_key: e.task_key.clone(),
            excerpt: e.excerpt.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
            .then_with(|| a.test.cmp(&b.test))
    });
    rows
}

/// Single-retry fix agent state machine for PRs that failed batch verification:
/// - Dispatches at most 1 automatic `fix-pr<n>` agent per red `(pr, sha)`.
/// - If a fix agent already ran for that `(pr, sha)` and the PR is still red at that head,
///   emits a human notification instead of looping.
pub fn decide_fixes(
    failed: &BTreeMap<u64, FailedRecord>,
    open_prs: &[PullFacts],
    agents: &BTreeMap<String, AgentRun>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    notified: &HashMap<u64, String>,
    free_slots: usize,
) -> FixDecisions {
    let by_num: HashMap<u64, &PullFacts> = open_prs.iter().map(|p| (p.number, p)).collect();
    let mut out = FixDecisions::default();

    for (&pr_num, rec) in failed {
        if holds.contains_key(&format!("pr:{pr_num}")) {
            continue;
        }
        let Some(pr) = by_num.get(&pr_num) else {
            continue;
        };
        if !pr.state.eq_ignore_ascii_case("OPEN") || pr.head != rec.sha {
            continue;
        }
        if already_ran("fix", pr_num, &rec.sha, agents, live_keys) {
            let key_prefix = format!("fix-pr{pr_num}");
            let still_running = agents.values().any(|a| {
                (a.key == key_prefix || a.key.starts_with(&format!("{key_prefix}-")))
                    && live_keys.contains(&a.key)
            });
            if !still_running && notified.get(&pr_num) != Some(&rec.sha) {
                out.notify.push((
                    pr_num,
                    format!(
                        "PR #{pr_num} remains red at {} after automatic fix attempt: {}",
                        &rec.sha[..rec.sha.len().min(8)],
                        rec.summary
                    ),
                ));
            }
            continue;
        }
        if out.dispatch.len() < free_slots {
            out.dispatch.push((pr_num, rec.clone()));
        }
    }
    out
}

/// R47: prune stale `failed` and `untied` records against current open PR heads (`open_heads: pr -> sha`).
///
/// - A `failed` entry for `pr` is kept only if `open_heads.get(&pr) == Some(&rec.sha)`.
/// - An `untied` entry is kept only if every PR in `rec.prs` is still open and at least one PR in
///   `rec.prs` is still at the SHA recorded in `rec.shas` (or if `rec.shas` is empty, all PRs are open).
pub fn prune_stale_records(
    failed: &BTreeMap<u64, FailedRecord>,
    untied: &[UntiedRecord],
    open_heads: &BTreeMap<u64, String>,
) -> (BTreeMap<u64, FailedRecord>, Vec<UntiedRecord>, bool) {
    let mut next_failed = BTreeMap::new();
    for (&pr, rec) in failed {
        if open_heads.get(&pr) == Some(&rec.sha) {
            next_failed.insert(pr, rec.clone());
        }
    }
    let mut next_untied = Vec::new();
    for u in untied {
        let all_open = !u.prs.is_empty() && u.prs.iter().all(|p| open_heads.contains_key(p));
        let sha_match = if u.shas.is_empty() {
            true
        } else {
            u.prs.iter().all(|p| u.shas.get(p) == open_heads.get(p))
        };
        if all_open && sha_match {
            next_untied.push(u.clone());
        }
    }
    let changed = next_failed.len() != failed.len() || next_untied.len() != untied.len();
    (next_failed, next_untied, changed)
}
