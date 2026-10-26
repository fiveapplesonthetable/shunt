//! Agent kit pure decisions: slug validation, port block allocation, migration ordering,
//! lockfile hashing, node cache state, export env files, PATH merging, first-error extraction,
//! proof verification, attribution scanning, PR refusals, and PR body formatting (R80, KE/KP/KR/KS).

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::short;

pub const PORT_LOW: u16 = 20000;
pub const PORT_SIZE: u16 = 20;
pub const PORT_BLOCKS: u16 = 600;

/// Pure SHA-256 hex digest (FIPS 180-4) used for `lock_key` and rescue directory naming.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (acc, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *acc = acc.wrapping_add(v);
        }
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// Validates a worktree/branch slug: 3..=80 chars of `[a-z0-9]+(-[a-z0-9]+)*` (KS1).
/// Returns an empty string when valid, or a human explanation when invalid.
pub fn slug_problem(slug: &str) -> String {
    let valid_len = (3..=80).contains(&slug.len());
    let valid_chars = !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid_len && valid_chars {
        String::new()
    } else {
        "a slug is 3 to 80 characters of lowercase letters, digits and single dashes, e.g. a-held-rate-can-be-discarded".into()
    }
}

/// Returns the first free 20-port block in `20000..32000` where neither `taken` contains `base`
/// nor `busy(base + k)` is true for any `k in 0..20` (KS2–KS6).
pub fn allocate_ports<F>(taken: &BTreeSet<u16>, busy: F) -> Option<u16>
where
    F: Fn(u16) -> bool,
{
    for i in 0..PORT_BLOCKS {
        let base = PORT_LOW + i * PORT_SIZE;
        if !taken.contains(&base) && !(0..PORT_SIZE).any(|k| busy(base + k)) {
            return Some(base);
        }
    }
    None
}

/// Orders migration paths across crates by filename first, then full path (KS7).
pub fn migration_order(paths: &[String]) -> Vec<String> {
    let mut out = paths.to_vec();
    out.sort_by(|a, b| {
        let fa = a.rsplit('/').next().unwrap_or(a);
        let fb = b.rsplit('/').next().unwrap_or(b);
        (fa, a.as_str()).cmp(&(fb, b.as_str()))
    });
    out
}

/// 16-char SHA-256 hex prefix of `lockfile` (KS8).
pub fn lock_key(lockfile: &[u8]) -> String {
    sha256_hex(lockfile)[..16].to_string()
}

/// State of a cached `node_modules` tree (KS9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeCacheState {
    Ready,
    Building,
    Absent,
}

impl NodeCacheState {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeCacheState::Ready => "ready",
            NodeCacheState::Building => "building",
            NodeCacheState::Absent => "absent",
        }
    }
}

/// Pure node cache state decision (KS9).
pub fn node_cache_state(ready: bool, installing: bool) -> NodeCacheState {
    if ready {
        NodeCacheState::Ready
    } else if installing {
        NodeCacheState::Building
    } else {
        NodeCacheState::Absent
    }
}

fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Renders `export KEY=VALUE\n` lines suitable for shell sourcing (KS10).
pub fn export_env_file(values: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in values {
        out.push_str(&format!("export {k}={}\n", sh_quote(v)));
    }
    out
}

/// Inverse of [`export_env_file`]; ignores non-`export ` lines (KS10).
pub fn parse_export_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((k, raw_v)) = rest.split_once('=') {
                let words = super::config::split_words(raw_v).unwrap_or_default();
                out.insert(k.to_string(), words.into_iter().next().unwrap_or_default());
            }
        }
    }
    out
}

/// Prepends directories from `extra` in front of `current`, deduplicating while preserving first-seen order (KS11).
pub fn merge_path(extra: &str, current: &str) -> String {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for part in extra.split(':').chain(current.split(':')) {
        if !part.is_empty() && seen.insert(part) {
            out.push(part);
        }
    }
    out.join(":")
}

/// True when `cwd` is equal to `root` or inside `root/` (KS12).
pub fn owns_path(cwd: &str, root: &str) -> bool {
    super::tidy::inside(cwd, root)
}

/// Extracts the first recognizable error line + up to 3 following lines, or the last 5 non-blank
/// lines when no error marker matches, capped at 600 chars (KE1–KE4).
pub fn first_error(log: &str) -> String {
    let re = Regex::new(
        r"^(error(\[E\d+\])?[: ]|FAILURE:|e: |.*\bFAILED\b|.*panicked at|.*error TS\d+|.*AssertionError|\s*✘|Error: )",
    )
    .expect("valid error regex");
    let lines: Vec<&str> = log.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let end = (i + 4).min(lines.len());
            let joined = lines[i..end].join("\n");
            return crate::char_prefix(&joined, 600).to_string();
        }
    }
    let non_blank: Vec<&str> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
    let start = non_blank.len().saturating_sub(5);
    let joined = non_blank[start..].join("\n");
    crate::char_prefix(&joined, 600).to_string()
}

/// A planned proof check command (KP6, KP7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProofCheck {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub needs: String,
    pub slot: bool,
}

/// Outcome of a single proof check in `.tachoproof/summary.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProofCheckResult {
    pub name: String,
    pub cmd: Vec<String>,
    pub ok: bool,
    pub seconds: u64,
    pub output: String,
    pub error: String,
}

/// Summary of a `prove` run (KP1–KP5, KR11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProofRecord {
    pub head: String,
    pub tree: String,
    pub ok: bool,
    pub checks: Vec<ProofCheckResult>,
}

/// Why `proof` does not cover the current worktree (KP1–KP5).
pub fn proof_problems(
    proof: Option<&ProofRecord>,
    tree: &str,
    dirty: bool,
    tool: &str,
) -> Vec<String> {
    let Some(proof) = proof else {
        return vec![format!("there is no proof. Run: {tool} prove")];
    };
    if !proof.ok {
        let (name, err) = proof
            .checks
            .iter()
            .find(|c| !c.ok)
            .map(|c| (c.name.as_str(), crate::char_prefix(&c.error, 200)))
            .unwrap_or(("a check", ""));
        return vec![format!(
            "the last `{tool} prove` failed at {name}: {err}. Fix it, commit, and run: {tool} prove"
        )];
    }
    let mut out = Vec::new();
    if dirty {
        out.push(format!(
            "there are uncommitted changes. Commit them, then run: {tool} prove"
        ));
    }
    if proof.tree != tree {
        out.push(format!(
            "the proof is older than the last commit: the files differ from the ones `{tool} prove` ran on. Run: {tool} prove"
        ));
    }
    out
}

/// Adds `rust_cache` for `cargo` / `"conservative checks"` and `gradle_slot` only for
/// `"conservative checks"` (KP6, KP7).
pub fn check_environment(
    check: &ProofCheck,
    inherited: &BTreeMap<String, String>,
    rust_cache: &BTreeMap<String, String>,
    gradle_slot: &str,
    gradle_slot_var: &str,
) -> BTreeMap<String, String> {
    let mut env = inherited.clone();
    let is_cargo = check.cmd.first().is_some_and(|c| c == "cargo");
    if is_cargo || check.name == "conservative checks" {
        for (k, v) in rust_cache {
            env.insert(k.clone(), v.clone());
        }
    }
    if check.name == "conservative checks" && !gradle_slot.is_empty() {
        env.insert(gradle_slot_var.to_string(), gradle_slot.to_string());
    }
    env
}

/// Detects tool-attribution markers in commit messages or PR descriptions (KR3–KR5, KR10).
pub fn attribution(text: &str) -> Vec<String> {
    let patterns: [(&str, &str); 4] = [
        (r"(?i)co-authored-by", "a Co-Authored-By line"),
        (r"(?i)generated (?:with|by)\b", "a \"Generated with\" line"),
        (
            r"(?i)https?://\S*(?:claude\.ai|anthropic\.com)\S*|https?://\S*session\S*",
            "a session link",
        ),
        ("\u{1f916}", "a robot emoji signature"),
    ];
    let mut out = Vec::new();
    for (pat, label) in patterns {
        if Regex::new(pat).expect("valid regex").is_match(text) {
            out.push(label.to_string());
        }
    }
    out
}

/// Every reason a PR cannot be opened yet, returning all reasons at once (KR1–KR9).
#[allow(clippy::too_many_arguments)]
pub fn pr_refusals(
    files: &[String],
    commits: &[(String, String)],
    title: &str,
    description: &str,
    merges: &[String],
    proof: Option<&ProofRecord>,
    tree: &str,
    dirty: bool,
    tool: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    if title.trim().is_empty() || description.trim().is_empty() {
        out.push(format!(
            "the title or the description is empty. Run: {tool} pr \"<title>\" -m \"<what changed and why, a few sentences>\""
        ));
    }
    if files.is_empty() {
        out.push("the branch changes nothing against its base. A draft PR must change files; if you could not do the work, push nothing and say so".into());
    }
    if !merges.is_empty() {
        let short_merges: Vec<&str> = merges.iter().map(|m| short(m)).collect();
        out.push(format!(
            "the branch has merge commits ({}). Rebuild it as plain commits on its base: git rebase --onto <base> <first-own-commit>^ , or cherry-pick your commits onto a fresh `{tool} start`",
            short_merges.join(", ")
        ));
    }
    for (sha, message) in commits {
        for what in attribution(message) {
            out.push(format!(
                "commit {} carries {what}. Remove it: git commit --amend (the last commit) or git rebase -i",
                short(sha)
            ));
        }
    }
    for what in attribution(&format!("{title}\n{description}")) {
        out.push(format!(
            "the title or description carries {what}. Remove it"
        ));
    }
    out.extend(proof_problems(proof, tree, dirty, tool));
    out
}

/// Formats the PR body with optional `Stacks on #N.` and the `## Proof` section (KR11).
pub fn build_body(
    description: &str,
    proof: &ProofRecord,
    stacked_on: Option<u64>,
    tool: &str,
) -> String {
    let mut out = vec![description.trim().to_string(), String::new()];
    if let Some(n) = stacked_on {
        if n > 0 {
            out.push(format!("Stacks on #{n}."));
            out.push(String::new());
        }
    }
    out.push("## Proof".into());
    out.push(String::new());
    out.push(format!(
        "`{tool} prove` ran these at {}:",
        short(&proof.head)
    ));
    out.push(String::new());
    if proof.checks.is_empty() {
        out.push("No check applies to these files.".into());
    } else {
        for c in &proof.checks {
            let status = if c.ok { "passed" } else { "failed" };
            out.push(format!("**{}**: {status} in {}s", c.name, c.seconds));
            out.push(String::new());
            out.push("```".into());
            out.push(format!("$ {}", c.cmd.join(" ")));
            out.push(c.output.clone());
            out.push("```".into());
            out.push(String::new());
        }
    }
    format!("{}\n", out.join("\n").trim_end())
}

/// Pure FIPS 180-4 SHA-384 hex digest (96 hex chars) matching `sqlx` `_sqlx_migrations.checksum`.
pub fn sha384_hex(data: &[u8]) -> String {
    const K: [u64; 80] = [
        0x428a2f98d728ae22,
        0x7137449123ef65cd,
        0xb5c0fbcfec4d3b2f,
        0xe9b5dba58189dbbc,
        0x3956c25bf348b538,
        0x59f111f1b605d019,
        0x923f82a4af194f9b,
        0xab1c5ed5da6d8118,
        0xd807aa98a3030242,
        0x12835b0145706fbe,
        0x243185be4ee4b28c,
        0x550c7dc3d5ffb4e2,
        0x72be5d74f27b896f,
        0x80deb1fe3b1696b1,
        0x9bdc06a725c71235,
        0xc19bf174cf692694,
        0xe49b69c19ef14ad2,
        0xefbe4786384f25e3,
        0x0fc19dc68b8cd5b5,
        0x240ca1cc77ac9c65,
        0x2de92c6f592b0275,
        0x4a7484aa6ea6e483,
        0x5cb0a9dcbd41fbd4,
        0x76f988da831153b5,
        0x983e5152ee66dfab,
        0xa831c66d2db43210,
        0xb00327c898fb213f,
        0xbf597fc7beef0ee4,
        0xc6e00bf33da88fc2,
        0xd5a79147930aa725,
        0x06ca6351e003826f,
        0x142929670a0e6e70,
        0x27b70a8546d22ffc,
        0x2e1b21385c26c926,
        0x4d2c6dfc5ac42aed,
        0x53380d139d95b3df,
        0x650a73548baf63de,
        0x766a0abb3c77b2a8,
        0x81c2c92e47edaee6,
        0x92722c851482353b,
        0xa2bfe8a14cf10364,
        0xa81a664bbc423001,
        0xc24b8b70d0f89791,
        0xc76c51a30654be30,
        0xd192e819d6ef5218,
        0xd69906245565a910,
        0xf40e35855771202a,
        0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8,
        0x1e376c085141ab53,
        0x2748774cdf8eeb99,
        0x34b0bcb5e19b48a8,
        0x391c0cb3c5c95a63,
        0x4ed8aa4ae3418acb,
        0x5b9cca4f7763e373,
        0x682e6ff3d6b2b8a3,
        0x748f82ee5defb2fc,
        0x78a5636f43172f60,
        0x84c87814a1f0ab72,
        0x8cc702081a6439ec,
        0x90befffa23631e28,
        0xa4506cebde82bde9,
        0xbef9a3f7b2c67915,
        0xc67178f2e372532b,
        0xca273eceea26619c,
        0xd186b8c721c0c207,
        0xeada7dd6cde0eb1e,
        0xf57d4f7fee6ed178,
        0x06f067aa72176fba,
        0x0a637dc5a2c898a6,
        0x113f9804bef90dae,
        0x1b710b35131c471b,
        0x28db77f523047d84,
        0x32caab7b40c72493,
        0x3c9ebe0a15c9bebc,
        0x431d67c49c100d4c,
        0x4cc5d4becb3e42b6,
        0x597f299cfc657e2a,
        0x5fcb6fab3ad6faec,
        0x6c44198c4a475817,
    ];
    let mut h: [u64; 8] = [
        0xcbbb9d5dc1059ed8,
        0x629a292a367cd507,
        0x9159015a3070dd17,
        0x152fecd8f70e5939,
        0x67332667ffc00b31,
        0x8eb44a8768581511,
        0xdb0c2e0d64f98fa7,
        0x47b5481dbefa4fa4,
    ];
    let bit_len = (data.len() as u128) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 128) != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            w[i] = u64::from_be_bytes([
                chunk[i * 8],
                chunk[i * 8 + 1],
                chunk[i * 8 + 2],
                chunk[i * 8 + 3],
                chunk[i * 8 + 4],
                chunk[i * 8 + 5],
                chunk[i * 8 + 6],
                chunk[i * 8 + 7],
            ]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (acc, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *acc = acc.wrapping_add(v);
        }
    }
    let mut out = String::with_capacity(96);
    for word in &h[..6] {
        out.push_str(&format!("{word:016x}"));
    }
    out
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Formats a UTC second-stamped migration filename `<YYYYMMDDHHMMSS>_<name>.sql`.
///
/// Validates that `name` is lowercase `snake_case` and refuses when any existing migration file
/// already uses the same 14-digit UTC version prefix.
pub fn format_utc_migration_filename(
    utc_epoch_secs: i64,
    name: &str,
    existing_files: &[String],
) -> Result<String, String> {
    let valid_name = !name.is_empty()
        && name.as_bytes()[0].is_ascii_lowercase()
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        return Err(format!(
            "migration name '{name}' must be lowercase snake_case (e.g. add_users_index)"
        ));
    }
    let days = utc_epoch_secs.div_euclid(86_400);
    let rem = utc_epoch_secs.rem_euclid(86_400);
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    let prefix = format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}");
    for existing in existing_files {
        let base = existing.rsplit('/').next().unwrap_or(existing);
        if base.starts_with(&format!("{prefix}_")) {
            return Err(format!(
                "migration version prefix {prefix} collides with existing migration {base}"
            ));
        }
    }
    Ok(format!("{prefix}_{name}.sql"))
}

/// Planned migration step for template database migration and `_sqlx_migrations` SHA-384 recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlxMigrationStep {
    pub version: i64,
    pub description: String,
    pub filename: String,
    pub sql: String,
    pub checksum_hex: String,
}

/// Plans unapplied migrations in `migration_order` and verifies SHA-384 checksums of already-applied
/// migrations in `_sqlx_migrations`.
pub fn plan_template_migrations(
    migrations: &[(String, String)],
    applied_checksums: &BTreeMap<i64, String>,
) -> Result<Vec<SqlxMigrationStep>, String> {
    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    let mut paths = Vec::new();
    for (p, sql) in migrations {
        paths.push(p.clone());
        by_path.insert(p.clone(), sql.clone());
    }
    let max_applied = applied_checksums.keys().max().copied();
    let ordered = migration_order(&paths);
    let mut steps = Vec::new();
    let mut prev_version: Option<i64> = None;
    for path in ordered {
        let base = path.rsplit('/').next().unwrap_or(&path);
        let Some(stem) = base.strip_suffix(".sql") else {
            continue;
        };
        let Some((ver_str, desc_raw)) = stem.split_once('_') else {
            return Err(format!(
                "invalid migration filename '{base}': missing '<version>_<name>.sql'"
            ));
        };
        let version: i64 = ver_str
            .parse()
            .map_err(|_| format!("invalid migration version '{ver_str}' in '{base}'"))?;
        if let Some(prev) = prev_version {
            if version <= prev {
                return Err(format!(
                    "duplicate or out-of-order migration version {version} in '{base}' (after {prev})"
                ));
            }
        }
        prev_version = Some(version);
        let description = desc_raw.replace('_', " ");
        let sql = by_path.get(&path).cloned().unwrap_or_default();
        let checksum_hex = sha384_hex(sql.as_bytes());
        if let Some(existing_sum) = applied_checksums.get(&version) {
            if !existing_sum.eq_ignore_ascii_case(&checksum_hex) {
                return Err(format!(
                    "migration {version} ({base}) checksum mismatch: applied {existing_sum}, file has {checksum_hex}"
                ));
            }
            continue;
        }
        if let Some(max_ver) = max_applied {
            if version < max_ver {
                return Err(format!(
                    "out-of-order unapplied migration {version} ({base}) precedes already-applied migration {max_ver}"
                ));
            }
        }
        steps.push(SqlxMigrationStep {
            version,
            description,
            filename: base.to_string(),
            sql,
            checksum_hex,
        });
    }
    Ok(steps)
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn utc_migration_filename_uses_utc_seconds_and_refuses_collisions_or_bad_names() {
        // 2026-10-25T18:05:09Z == 1792951509
        let fname = format_utc_migration_filename(1_792_951_509, "add_agent_worktree_guard", &[])
            .expect("valid migration name");
        assert_eq!(fname, "20261025180509_add_agent_worktree_guard.sql");

        let dup = format_utc_migration_filename(
            1_792_951_509,
            "another_change",
            &[format!("migrations/{fname}")],
        );
        assert!(
            dup.is_err(),
            "duplicate 14-digit UTC version prefix must be refused"
        );

        assert!(
            format_utc_migration_filename(1_792_951_509, "Bad-Name", &[]).is_err(),
            "non-snake_case name must be refused"
        );
    }

    #[test]
    fn template_migrations_plan_in_order_and_verify_sqlx_sha384_checksums() {
        // FIPS 180-4 test vector: SHA-384("abc")
        assert_eq!(
            sha384_hex(b"abc"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );

        let files = vec![
            (
                "crates/b/migrations/20261025120002_create_runs.sql".to_string(),
                "CREATE TABLE runs (id INT);".to_string(),
            ),
            (
                "crates/a/migrations/20261025120001_create_users.sql".to_string(),
                "CREATE TABLE users (id INT);".to_string(),
            ),
        ];
        let steps = plan_template_migrations(&files, &BTreeMap::new()).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].version, 20261025120001);
        assert_eq!(steps[0].description, "create users");
        assert_eq!(
            steps[0].checksum_hex,
            sha384_hex(b"CREATE TABLE users (id INT);")
        );
        assert_eq!(steps[1].version, 20261025120002);

        // Already applied with matching checksum is skipped; modified SQL fails closed.
        let mut applied = BTreeMap::new();
        applied.insert(20261025120001, steps[0].checksum_hex.clone());
        let rem = plan_template_migrations(&files, &applied).unwrap();
        assert_eq!(rem.len(), 1);
        assert_eq!(rem[0].version, 20261025120002);

        applied.insert(20261025120001, "0".repeat(96));
        assert!(plan_template_migrations(&files, &applied).is_err());
    }
}
