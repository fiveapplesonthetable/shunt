//! Log reading: counterexamples, failed tests, classification.

use super::*;

/// Extract a property-based testing counterexample (`"minimal failing input"` / `"proptest"` / `"Falsified"`) from a log.
pub fn counterexample(log: &str) -> Option<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^.*(?:minimal failing input|Falsifying example|Falsified|shrunk to|proptest(?:\s+failed)?:).*$").unwrap()
    });
    RE.find(log).map(|m| m.as_str().trim().to_string())
}

/// Extract named failing tests from Rust (`test <name> ... FAILED`), Gradle/JUnit (`<Class > method> FAILED`),
/// Playwright (`✘ ... [browser] › <spec> (Ns)`), pytest (`FAILED <name>`), or Vitest/Jest (`FAIL <name>`) log output.
pub fn failed_tests(log: &str) -> Vec<String> {
    static RUST_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^test\s+([^\s]+)\s+\.\.\.\s+FAILED\b").unwrap());
    static GRADLE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(\w+\s+>\s+\w+)\s+FAILED\b").unwrap());
    static PLAYWRIGHT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)✘\s+\d+\s+\[\w+\]\s+›\s+(.+?)\s+\(\d").unwrap());
    static PY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^FAILED\s+([^\s]+?)(?:\s+-\s+.*)?$").unwrap());
    static JS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*FAIL\s+([^\s]+\.(?:test|spec)\.[jt]sx?)\b").unwrap());

    let mut seen = BTreeSet::new();
    for caps in RUST_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    for caps in GRADLE_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().trim().to_string());
        }
    }
    for caps in PLAYWRIGHT_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().trim().to_string());
        }
    }
    for caps in PY_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    for caps in JS_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    seen.into_iter().collect()
}

/// Classify a failure log as `"code"` or `"infrastructure"`.
///
/// Key invariant: if the log names a failing test or a property counterexample (`"minimal failing input"`),
/// or contains compiler/type/lint errors, it is ALWAYS classified as `"code"` even if infrastructure
/// strings (`ECONNREFUSED`, `503`, `timed out`) also appear in the log.
pub fn classify(log: &str) -> &'static str {
    if !failed_tests(log).is_empty() || counterexample(log).is_some() {
        return "code";
    }
    static CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)(?:^error(?:\[E\d+\])?:|panicked at|AssertionError|SyntaxError|TypeError|could not compile)",
        )
        .unwrap()
    });
    if CODE_RE.is_match(log) {
        return "code";
    }
    static INFRA_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)(?:ECONNREFUSED|ECONNRESET|ETIMEDOUT|No space left on device|Cannot connect to the Docker daemon|error during connect:.*docker|docker: Error response from daemon|toomanyrequests|rate limit exceeded|502 Bad Gateway|503 Service Unavailable|504 Gateway Time-out|TLS handshake timeout|connection reset by peer|Temporary failure in name resolution|Could not resolve host|failed to fetch|OOMKilled|Cannot allocate memory|out of memory|signal: 9, SIGKILL|resource temporarily unavailable|disk quota exceeded|: command not found|terminating connection due to administrator command|not found: content digest sha256:|failed to export layer|No such image: sha256:|address already in use|containers failed to start|network .* not found|docker could not create the network|Gradle build daemon disappeared unexpectedly|fatal: Unable to create .*\.lock|Failed to initialize NVML)",
        )
        .unwrap()
    });
    if INFRA_RE.is_match(log) {
        return "infrastructure";
    }
    "code"
}

/// Extract the first meaningful error line from a failure log.
pub fn first_error_line(log: &str) -> String {
    if let Some(ce) = counterexample(log) {
        return ce;
    }
    let tests = failed_tests(log);
    if let Some(first) = tests.first() {
        return format!("test {first} ... FAILED");
    }
    for line in log.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.contains("panicked at")
            || lower.contains("failed")
            || lower.contains("assertion")
        {
            return trimmed.chars().take(160).collect();
        }
    }
    log.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("check failed")
        .trim()
        .chars()
        .take(160)
        .collect()
}

/// Build a concise summary line for a batch failure.
pub fn summary_line(log: &str) -> String {
    let kind = classify(log);
    let detail = first_error_line(log);
    format!("[{kind}] {detail}")
}

/// Extract the subset of `areas` named after `failed:` in a summary line, or all of `areas` if none match.
pub fn failed_areas(line: &str, areas: &[String]) -> Vec<String> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"failed:\s*([a-z0-9_\- ]+)").unwrap());
    if let Some(caps) = RE.captures(line) {
        if let Some(m) = caps.get(1) {
            let named: BTreeSet<&str> = m.as_str().split_whitespace().collect();
            let matched: Vec<String> = areas
                .iter()
                .filter(|a| named.contains(a.as_str()))
                .cloned()
                .collect();
            if !matched.is_empty() {
                return matched;
            }
        }
    }
    areas.to_vec()
}
