//! Configuration decisions: env-file parsing (systemd rules), merge, hot reload, CPU limit,
//! the busy gate, private TMPDIR validation and agent command templates.

use std::collections::{BTreeMap, BTreeSet};

/// Parse an env file with systemd `EnvironmentFile=` rules.
///
/// - `#`/`;` lines are comments; a line without `=` is ignored; keys must match
///   `[A-Za-z_][A-Za-z0-9_]*`; whitespace after `=` is skipped.
/// - Single quotes are verbatim and may span lines.
/// - Double quotes unescape `\" \\ \` \$`, join backslash-newline, keep any other `\x`.
/// - Junk after a closing quote drops the entry; an unterminated quote ends parsing.
/// - Unquoted: `\x` -> `x`, backslash-newline continues, trailing ` \t\r` trimmed.
/// - No variable expansion.
///
/// Later duplicates win when converted with [`to_map`]; order is preserved here.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let skip_line = |i: &mut usize| {
        while *i < c.len() && c[*i] != '\n' {
            *i += 1;
        }
    };
    while i < c.len() {
        while i < c.len() && matches!(c[i], ' ' | '\t' | '\r' | '\n') {
            i += 1;
        }
        if i >= c.len() {
            break;
        }
        if c[i] == '#' || c[i] == ';' {
            skip_line(&mut i);
            continue;
        }
        let start = i;
        while i < c.len() && c[i] != '=' && c[i] != '\n' {
            i += 1;
        }
        if i >= c.len() || c[i] == '\n' {
            continue; // no '=': ignored
        }
        let key: String = c[start..i].iter().collect::<String>().trim().to_string();
        i += 1;
        while i < c.len() && (c[i] == ' ' || c[i] == '\t') {
            i += 1;
        }
        let mut value = String::new();
        let mut keep = valid_key(&key);
        match c.get(i) {
            Some('\'') => {
                i += 1;
                let Some(end) = (i..c.len()).find(|&j| c[j] == '\'') else {
                    return out; // unterminated quote ends parsing
                };
                value = c[i..end].iter().collect();
                i = end + 1;
                keep &= trailing_ok(&c, &mut i);
            }
            Some('"') => {
                i += 1;
                loop {
                    match c.get(i) {
                        None => return out,
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some('\\') => match c.get(i + 1) {
                            Some(&e @ ('"' | '\\' | '`' | '$')) => {
                                value.push(e);
                                i += 2;
                            }
                            Some('\n') => i += 2,
                            Some(&e) => {
                                value.push('\\');
                                value.push(e);
                                i += 2;
                            }
                            None => return out,
                        },
                        Some(&ch) => {
                            value.push(ch);
                            i += 1;
                        }
                    }
                }
                keep &= trailing_ok(&c, &mut i);
            }
            _ => {
                while i < c.len() && c[i] != '\n' {
                    if c[i] == '\\' {
                        match c.get(i + 1) {
                            Some('\n') => {}
                            Some(&e) => value.push(e),
                            None => {}
                        }
                        i += 2;
                        continue;
                    }
                    value.push(c[i]);
                    i += 1;
                }
                let trimmed = value.trim_end_matches([' ', '\t', '\r']).len();
                value.truncate(trimmed);
            }
        }
        if keep {
            out.push((key, value));
        }
    }
    out
}

/// After a closing quote only blanks may follow on the line; otherwise the entry is junk.
fn trailing_ok(c: &[char], i: &mut usize) -> bool {
    while *i < c.len() && matches!(c[*i], ' ' | '\t' | '\r') {
        *i += 1;
    }
    if *i >= c.len() || c[*i] == '\n' {
        return true;
    }
    while *i < c.len() && c[*i] != '\n' {
        *i += 1;
    }
    false
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
pub fn valid_key(key: &str) -> bool {
    let mut it = key.chars();
    matches!(it.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && it.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Later entries win.
pub fn to_map(entries: &[(String, String)]) -> BTreeMap<String, String> {
    entries.iter().cloned().collect()
}

/// The process environment wins over the file.
pub fn merge(
    file: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = file.clone();
    out.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

/// Keys the process environment pinned at startup: present in env and absent from the file
/// or different from it. Those are never hot-reloaded.
pub fn env_pinned(
    hot: &[&str],
    startup_file: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    hot.iter()
        .filter(|k| {
            env.get(**k)
                .is_some_and(|v| startup_file.get(**k) != Some(v))
        })
        .map(|k| k.to_string())
        .collect()
}

/// Re-read hot keys from a freshly parsed file. `Some(v)` sets, `None` unsets (removed from the
/// file). Pinned keys are left out entirely (the caller keeps them).
pub fn refresh(
    hot: &[&str],
    pinned: &BTreeSet<String>,
    file: &BTreeMap<String, String>,
) -> BTreeMap<String, Option<String>> {
    hot.iter()
        .filter(|k| !pinned.contains(**k))
        .map(|k| (k.to_string(), file.get(*k).cloned()))
        .collect()
}

/// Where a CPU limit came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit {
    pub percent: f64,
}

/// Resolve the CPU limit (percent of all CPUs).
///
/// `raw` is `<P>MAX_CPU_PERCENT` (0..=100). The legacy `<P>MAX_LOAD` (0..=2*cpus) is used only
/// while the percent is unset, converted to `min(100, load/cpus*100)`. An invalid value never
/// fails: it keeps `previous` (on refresh) or `default`, and returns a problem sentence that the
/// snapshot shows as a `config` attention line.
pub fn cpu_limit(
    prefix: &str,
    raw: Option<&str>,
    legacy: Option<&str>,
    cpus: usize,
    previous: Option<f64>,
    default: f64,
) -> (CpuLimit, Option<String>) {
    let cpus = cpus.max(1) as f64;
    let fallback = |name: &str, v: &str, what: String| {
        let used = if previous.is_some() {
            "the previous limit"
        } else {
            "the default"
        };
        (
            CpuLimit {
                percent: previous.unwrap_or(default),
            },
            Some(format!("{prefix}{name}={v} must be {what}; using {used}")),
        )
    };
    let parse = |v: &str| v.trim().parse::<f64>().ok().filter(|x| x.is_finite());
    if let Some(v) = raw.filter(|v| !v.trim().is_empty()) {
        return match parse(v) {
            Some(p) if (0.0..=100.0).contains(&p) => (CpuLimit { percent: p }, None),
            _ => fallback(
                "MAX_CPU_PERCENT",
                v,
                "a percentage from 0 to 100".to_string(),
            ),
        };
    }
    if let Some(v) = legacy.filter(|v| !v.trim().is_empty()) {
        let max = 2.0 * cpus;
        return match parse(v) {
            Some(l) if (0.0..=max).contains(&l) => (
                CpuLimit {
                    percent: (l / cpus * 100.0).min(100.0),
                },
                None,
            ),
            _ => fallback("MAX_LOAD", v, format!("a load from 0 to {max}")),
        };
    }
    (CpuLimit { percent: default }, None)
}

/// Machine facts the busy gate needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MachineFacts {
    /// Paths whose free space is below the low-disk threshold.
    pub low_disk: Vec<String>,
    /// CPU used by the fleet's quota'd slices, in percent of the machine.
    pub quota_cpu_percent: Option<f64>,
    pub load1: f64,
    pub load5: f64,
    pub cpus: usize,
}

/// Why the machine is too busy to start more work.
#[derive(Debug, Clone, PartialEq)]
pub enum Busy {
    LowDisk { path: String },
    QuotaCpu { percent: f64, limit: f64 },
    Load { load: f64, max: f64 },
}

impl Busy {
    pub fn reason(&self) -> String {
        match self {
            Busy::LowDisk { path } => format!("{path} is low on disk space"),
            Busy::QuotaCpu { percent, limit } => {
                format!("CPU use {percent:.0}% is over the {limit:.0}% limit")
            }
            Busy::Load { load, max } => format!("load {load:.1} is over {max:.1}"),
        }
    }
}

/// `None` when the machine may take more work.
pub fn busy(m: &MachineFacts, limit: CpuLimit) -> Option<Busy> {
    if let Some(path) = m.low_disk.first() {
        return Some(Busy::LowDisk { path: path.clone() });
    }
    if let Some(p) = m.quota_cpu_percent {
        if p > limit.percent {
            return Some(Busy::QuotaCpu {
                percent: p,
                limit: limit.percent,
            });
        }
    }
    let load = m.load1.max(m.load5);
    let max = m.cpus.max(1) as f64 * limit.percent / 100.0;
    (load > max).then_some(Busy::Load { load, max })
}

/// lstat facts about the private TMPDIR.
#[derive(Debug, Clone, PartialEq)]
pub struct TmpDirFacts {
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub owner_uid: u32,
    pub mode: u32,
}

/// Fail closed: the private TMPDIR must be an absolute, real directory we own with mode 0700.
pub fn check_tmpdir(f: &TmpDirFacts, my_uid: u32) -> Result<(), String> {
    let p = &f.path;
    if !p.starts_with('/') {
        return Err(format!("{p} is not an absolute path"));
    }
    if f.is_symlink {
        return Err(format!("{p} is a symlink; refusing it as TMPDIR"));
    }
    if !f.is_dir {
        return Err(format!("{p} is not a directory"));
    }
    if f.owner_uid != my_uid {
        return Err(format!("{p} belongs to uid {}, not {my_uid}", f.owner_uid));
    }
    if f.mode & 0o077 != 0 {
        return Err(format!(
            "{p} has mode {:o}; group and others must have no access",
            f.mode & 0o777
        ));
    }
    Ok(())
}

/// Split an agent command template argv-style and fill `{model}`; a word that is exactly
/// `{brief}` becomes the brief as one literal argument (never re-split or shell-expanded).
pub fn command_argv(template: &str, model: &str, brief: &str) -> Result<Vec<String>, String> {
    Ok(split_words(template)?
        .into_iter()
        .map(|w| {
            if w == "{brief}" {
                brief.to_string()
            } else {
                w.replace("{model}", model)
            }
        })
        .collect())
}

/// POSIX-shell-like word splitting (quotes and backslashes; no expansion).
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        match ch {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match it.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated ' in {s:?}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next() {
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some('\n') => {}
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err(format!("unterminated \" in {s:?}")),
                        },
                        Some(x) => cur.push(x),
                        None => return Err(format!("unterminated \" in {s:?}")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(x) = it.next() {
                    if x != '\n' {
                        cur.push(x);
                    }
                }
            }
            x => {
                in_word = true;
                cur.push(x);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Render entries back into an env file that [`parse_env_file`] reads identically.
pub fn render_env_file(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in entries {
        out.push_str(k);
        out.push_str("=\"");
        for ch in v.chars() {
            if matches!(ch, '"' | '\\' | '`' | '$') {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push_str("\"\n");
    }
    out
}
