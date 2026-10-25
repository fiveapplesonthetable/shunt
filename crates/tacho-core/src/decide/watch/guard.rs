//! PreToolUse Bash guard hooks: CPU burner refusal (R60) and background execution refusal (R61).

use super::config::WatchConfig;
use regex::Regex;
use std::sync::LazyLock;

static SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\|\||&&|;|\n|\||&|\bdo\b|\bthen\b|\belse\b").unwrap());
static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\d.]+[smhd]?$").unwrap());

/// Split a shell stage into tokens using POSIX-style quoting (`shlex.split`),
/// falling back to `split_whitespace` when quotes are unbalanced.
pub fn shell_tokens(stage: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = stage.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    if let Some(&next) = chars.peek() {
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            cur.push(chars.next().unwrap());
                        } else {
                            cur.push(ch);
                        }
                    } else {
                        return stage.split_whitespace().map(str::to_string).collect();
                    }
                } else {
                    cur.push(ch);
                }
            }
            Some(_) => unreachable!(),
            None => {
                if ch.is_whitespace() {
                    if in_token {
                        tokens.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else if ch == '\'' || ch == '"' {
                    in_token = true;
                    quote = Some(ch);
                } else if ch == '\\' {
                    in_token = true;
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    } else {
                        return stage.split_whitespace().map(str::to_string).collect();
                    }
                } else {
                    in_token = true;
                    cur.push(ch);
                }
            }
        }
    }

    if quote.is_some() {
        return stage.split_whitespace().map(str::to_string).collect();
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

/// Extract the burner program name a single pipeline stage runs, looking through wrappers,
/// environment assignments (`X=Y`), flags (`-o`), and durations (`60`, `10m`, `1.5s`).
pub fn stage_burner_word(stage: &str, c: &WatchConfig) -> Option<String> {
    for token in shell_tokens(stage) {
        let stripped = token.trim_start_matches(['(', '{']);
        let name = stripped.rsplit('/').next().unwrap_or(stripped);
        if c.is_burner_name(name) {
            return Some(name.to_string());
        }
        if token.contains('=')
            || token.starts_with('-')
            || DURATION_RE.is_match(&token)
            || c.wrappers.iter().any(|w| w == name)
        {
            continue;
        }
        return None;
    }
    None
}

fn contains_tool_starve(command: &str, tool: &str) -> bool {
    let pattern = format!(r"\b{}\s+starve\b", regex::escape(tool));
    Regex::new(&pattern)
        .map(|re| re.is_match(command))
        .unwrap_or(false)
}

/// Inspect a shell `command` and return a refusal message if any stage launches a CPU burner
/// outside `<tool> starve` (R60).
///
/// `yes` directly feeding a pipe (`yes | ...`) answers an interactive prompt and is allowed.
pub fn burner_refusal(command: &str, c: &WatchConfig) -> Option<String> {
    if contains_tool_starve(command, &c.tool) {
        return None;
    }
    let mut stages: Vec<(&str, Option<&str>)> = Vec::new();
    let mut last_end = 0;
    for m in SEPARATOR_RE.find_iter(command) {
        stages.push((&command[last_end..m.start()], Some(m.as_str())));
        last_end = m.end();
    }
    stages.push((&command[last_end..], None));

    for (stage, sep_after) in stages {
        if let Some(word) = stage_burner_word(stage, c) {
            let feeds_pipe = sep_after == Some("|");
            if !(word == "yes" && feeds_pipe) {
                return Some(format!(
                    "Refused: `{word}` is a CPU burner. Load generation runs only as `{tool} starve -- <command>`, \
                     which gives it a CPU slice of its own and stops the burners when the command ends. \
                     {tool} kills any other burner after 10 minutes.",
                    tool = c.tool
                ));
            }
        }
    }
    None
}

/// Refuse `run_in_background: true` on a Bash tool invocation (R61).
pub fn background_refusal(run_in_background: bool, c: &WatchConfig) -> Option<String> {
    if run_in_background {
        Some(format!(
            "Denied: this session exits when you write your final message, and a background command dies with it. \
             Run it in the foreground with a timeout, or start it with `{tool} run` and wait on it with `{tool} wait`.",
            tool = c.tool
        ))
    } else {
        None
    }
}

static LIMIT_KV_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(timeout|timeout_ms|timeout_secs|budget|budget_ms|budget_secs|retries|max_retries|max_attempts|wall_clock|wall_clock_secs)\b(?:\s*:\s*[A-Za-z0-9_<>]+)?\s*[:=]\s*(\d+(?:\.\d+)?)\s*(ms|s|m|h)?",
    )
    .unwrap()
});
static SERIAL_TEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:mode\s*:\s*['"]serial['"]|test\.slow\s*\(\s*\)|fullyParallel\s*:\s*false|--workers(?:=|\s+)1\b|--test-threads(?:=|\s+)1\b)"#,
    )
    .unwrap()
});
static VAGUE_WAIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:thread",
        r"::sleep|std::thread",
        r"::sleep|tokio::time::sleep|time\.sleep|asyncio\.sleep|waitForTimeout|setTimeout)\s*\(",
    ))
    .unwrap()
});
static SPEED_ASSERT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\.elapsed\(\)\s*[<]=?\s*Duration::from_|assert!\([^)]*elapsed\(\)\s*<|expect\([^)]*elapsed[^)]*\)\.toBeLessThan)",
    )
    .unwrap()
});

fn is_test_path(path: &str) -> bool {
    let p = path.trim_start_matches("a/").trim_start_matches("b/");
    let base = p.rsplit('/').next().unwrap_or(p);
    p.starts_with("tests/")
        || p.contains("/tests/")
        || base.starts_with("test_")
        || base.ends_with("_test.rs")
        || base.ends_with("_tests.rs")
        || base.ends_with(".test.ts")
        || base.ends_with(".test.tsx")
        || base.ends_with(".spec.ts")
        || base.ends_with(".spec.tsx")
}

fn to_millis(val: f64, unit: Option<&str>) -> f64 {
    match unit.map(|u| u.to_ascii_lowercase()).as_deref() {
        Some("ms") => val,
        Some("s") => val * 1_000.0,
        Some("m") => val * 60_000.0,
        Some("h") => val * 3_600_000.0,
        _ => val,
    }
}

/// Scans a unified diff for loosened limits (`timeout`, `budget`, `retries`, `max_attempts`,
/// `wall_clock` raised above the removed value in non-test files, or serial/slow test flags added).
pub fn loosened_limits_in_diff(diff: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut cur_file = String::new();
    let mut removed_limits: std::collections::BTreeMap<String, f64> =
        std::collections::BTreeMap::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            cur_file = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches("b/")
                .to_string();
            removed_limits.clear();
            continue;
        }
        if line.starts_with("@@") {
            removed_limits.clear();
            continue;
        }
        if line.starts_with("---") {
            continue;
        }
        if let Some(removed) = line.strip_prefix('-') {
            if !is_test_path(&cur_file) {
                for cap in LIMIT_KV_RE.captures_iter(removed) {
                    let key = cap[1].to_ascii_lowercase();
                    if let Ok(num) = cap[2].parse::<f64>() {
                        let norm = to_millis(num, cap.get(3).map(|m| m.as_str()));
                        removed_limits
                            .entry(key)
                            .and_modify(|v| *v = v.min(norm))
                            .or_insert(norm);
                    }
                }
            }
        } else if let Some(added) = line.strip_prefix('+') {
            if SERIAL_TEST_RE.is_match(added) {
                violations.push(format!(
                    "{}: serial or slow test workaround added: {}",
                    if cur_file.is_empty() {
                        "<diff>"
                    } else {
                        &cur_file
                    },
                    added.trim()
                ));
            }
            if !is_test_path(&cur_file) {
                for cap in LIMIT_KV_RE.captures_iter(added) {
                    let key = cap[1].to_ascii_lowercase();
                    if let Ok(num) = cap[2].parse::<f64>() {
                        let norm = to_millis(num, cap.get(3).map(|m| m.as_str()));
                        if let Some(&old) = removed_limits.get(&key) {
                            if norm > old {
                                violations.push(format!(
                                    "{}: loosened {} from {} to {}",
                                    if cur_file.is_empty() {
                                        "<diff>"
                                    } else {
                                        &cur_file
                                    },
                                    key,
                                    old,
                                    norm
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    violations
}

/// Scans a unified diff for vague sleeps (`time.sleep`, `waitForTimeout`, `setTimeout`, etc.)
/// added in test files instead of deterministic condition/event waiting.
pub fn vague_waits_in_diff(diff: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut cur_file = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            cur_file = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches("b/")
                .to_string();
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if (cur_file.is_empty() || is_test_path(&cur_file)) && VAGUE_WAIT_RE.is_match(added) {
                violations.push(format!(
                    "{}: vague sleep added in test: {}",
                    if cur_file.is_empty() {
                        "<diff>"
                    } else {
                        &cur_file
                    },
                    added.trim()
                ));
            }
        }
    }
    violations
}

/// Scans a unified diff for wall-clock speed assertions (`elapsed() < Duration::from_...`) added in tests.
pub fn time_based_tests_in_diff(diff: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut cur_file = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            cur_file = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches("b/")
                .to_string();
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if (cur_file.is_empty() || is_test_path(&cur_file)) && SPEED_ASSERT_RE.is_match(added) {
                violations.push(format!(
                    "{}: wall-clock speed assertion added in test: {}",
                    if cur_file.is_empty() {
                        "<diff>"
                    } else {
                        &cur_file
                    },
                    added.trim()
                ));
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loosened_limits_refuses_raised_timeout_retries_and_serial_mode_outside_test_exemptions() {
        let diff = "\
--- a/src/runner.rs
+++ b/src/runner.rs
@@ -10,2 +10,2 @@
-const TIMEOUT_SECS: u64 = 30;
-let max_attempts = 2;
+const TIMEOUT_SECS: u64 = 120;
+let max_attempts = 5;
--- a/e2e/login.spec.ts
+++ b/e2e/login.spec.ts
@@ -1,3 +1,5 @@
+test.describe.configure({ mode: 'serial' });
+test.slow();
--- a/tests/unit_test.rs
+++ b/tests/unit_test.rs
@@ -1,1 +1,1 @@
-let timeout_secs = 5;
+let timeout_secs = 10;
";
        let violations = loosened_limits_in_diff(diff);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("src/runner.rs") && v.contains("timeout_secs")),
            "expected raised timeout_secs violation in src/runner.rs, got: {violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("src/runner.rs") && v.contains("max_attempts")),
            "expected raised max_attempts violation in src/runner.rs, got: {violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.contains("mode: 'serial'")),
            "expected serial mode violation, got: {violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.contains("test.slow()")),
            "expected test.slow() violation, got: {violations:?}"
        );
        assert!(
            !violations.iter().any(|v| v.contains("tests/unit_test.rs")),
            "numeric timeout fixture inside tests/ must be exempt, got: {violations:?}"
        );
    }

    #[test]
    fn vague_waits_and_time_based_tests_are_detected_in_test_diffs() {
        let diff = concat!(
            "--- a/tests/integration_test.rs\n",
            "+++ b/tests/integration_test.rs\n",
            "@@ -5,2 +5,4 @@\n",
            "+    std::thread",
            "::sleep(Duration::from_millis(200));\n",
            "+    assert!(start.elapsed() < Duration::from_secs(1));\n",
        );
        let waits = vague_waits_in_diff(diff);
        assert_eq!(waits.len(), 1);
        assert!(waits[0].contains(concat!("thread", "::sleep")));

        let speed = time_based_tests_in_diff(diff);
        assert_eq!(speed.len(), 1);
        assert!(speed[0].contains("elapsed()"));
    }
}
