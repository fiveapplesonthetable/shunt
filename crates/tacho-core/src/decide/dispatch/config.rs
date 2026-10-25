//! Dispatch configuration: every project-specific value, with generic defaults.

use serde::{Deserialize, Serialize};

/// Provider inference tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderTables {
    /// Model names (besides `claude-*`) that run on claude.
    pub claude_families: Vec<String>,
    /// Model prefixes that run on codex.
    pub codex_prefixes: Vec<String>,
    /// Model prefixes that run on gemini.
    pub gemini_prefixes: Vec<String>,
    pub codex_default: String,
    pub claude_default: String,
    pub gemini_default: String,
    /// Tools (argv[0] basenames / provider names) that support sessions and the guard hook.
    pub session_capable: Vec<String>,
}

impl Default for ProviderTables {
    fn default() -> Self {
        ProviderTables {
            claude_families: ["sonnet", "haiku", "opus", "fable"]
                .map(String::from)
                .to_vec(),
            codex_prefixes: ["gpt-", "o1", "o3", "o4", "codex"]
                .map(String::from)
                .to_vec(),
            gemini_prefixes: ["gemini"].map(String::from).to_vec(),
            codex_default: "gpt-5.4".into(),
            claude_default: "sonnet".into(),
            gemini_default: "gemini-3.1-pro-preview".into(),
            session_capable: vec!["claude".into()],
        }
    }
}

/// Brief assembly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BriefConfig {
    /// A document the agent reads first (`Read <preamble> and follow it exactly.`).
    pub preamble: Option<String>,
    /// How many lines the final summary may have.
    pub report_lines: u32,
    /// `(key prefix, rule text)` appended to briefs whose key starts with the prefix.
    pub key_rules: Vec<(String, String)>,
    pub self_update_rule: Option<String>,
    /// Phase-note instruction; `{key}` is the base key, `{tool}` the CLI name.
    pub note_instruction: String,
}

impl Default for BriefConfig {
    fn default() -> Self {
        BriefConfig {
            preamble: None,
            report_lines: 10,
            key_rules: vec![(
                "fix-".into(),
                "Rules from the owner for this run. A red check is red for a reason: find it and fix it. Never lengthen a timeout, add a retry, force serial mode or raise a size budget to make a check pass.".into(),
            ), (
                "main-".into(),
                "Rules from the owner for this run. A red check is red for a reason: find it and fix it. Never lengthen a timeout, add a retry, force serial mode or raise a size budget to make a check pass.".into(),
            ), (
                "hands-".into(),
                "Rules from the owner for this run. A red check is red for a reason: find it and fix it. Never lengthen a timeout, add a retry, force serial mode or raise a size budget to make a check pass.".into(),
            )],
            self_update_rule: None,
            note_instruction:
                "When you move to a new phase, record it with `{tool} note {key} <what you are doing>`."
                    .into(),
        }
    }
}

/// All dispatch knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DispatchConfig {
    /// CLI name used in sentences.
    pub tool: String,
    /// Env var prefix (`<P>_AGENT_SLOTS`, `<P>_LIVE`, `<P>_AGENT_KEY`).
    pub env_prefix: String,
    /// Fleet home (private TMPDIR is `<home>/tmp`).
    pub home: String,
    /// The repository agents work in.
    pub repo: String,
    /// Directory under which agent worktrees are created.
    pub work_root: String,
    /// Default agent slot limit (`<= 0` pauses all automatic starts).
    pub slots: i32,
    /// Live CPU budget percentage (`1..=100`).
    pub max_cpu_percent: u32,
    /// Generic command template when no provider-specific command is selected.
    pub command: String,
    /// Command template for Claude runs.
    pub claude_command: String,
    /// Command template for Codex runs.
    pub codex_command: String,
    /// Path to the PreToolUse guard hook script.
    pub hook_script: String,
    /// Issue number where stuck-run escalations are posted.
    pub owner_issue: u64,
    pub resume_limit: u32,
    /// Seconds before retry `n` (index `n-1`; the last value repeats).
    pub backoff: Vec<i64>,
    pub stale_hold_secs: i64,
    /// Batch group names a hold may target.
    pub groups: Vec<String>,
    pub hands_prefix: String,
    pub fix_prefix: String,
    pub train_branch_prefix: String,
    /// Log lines starting with these are noise for report detection.
    pub noise_prefixes: Vec<String>,
    /// How many trailing (non-noise) lines must hold the final report.
    pub report_window: usize,
    pub summary_chars: usize,
    pub shown_commits: usize,
    pub shown_files: usize,
    pub tail_lines: usize,
    pub escalation_lines: usize,
    /// Quiet minutes after which the watcher nudges (named in the nudge brief).
    pub nudge_quiet_minutes: u32,
    pub brief: BriefConfig,
    pub providers: ProviderTables,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        DispatchConfig {
            tool: "tacho".into(),
            env_prefix: "TACHO".into(),
            home: "/tmp/tacho-home".into(),
            repo: "/tmp/tacho-repo".into(),
            work_root: "/tmp/tacho-work".into(),
            slots: 4,
            max_cpu_percent: 80,
            command: "claude -p {model} {brief}".into(),
            claude_command: "claude -p --model {model} {brief}".into(),
            codex_command: "codex exec --model {model} {brief}".into(),
            hook_script: "no-background-bash.py".into(),
            owner_issue: 1,
            resume_limit: 1,
            backoff: vec![120],
            stale_hold_secs: 86_400,
            groups: ["android", "ci", "console", "core", "light", "wide"]
                .map(String::from)
                .to_vec(),
            hands_prefix: "hands-".into(),
            fix_prefix: "fix-".into(),
            train_branch_prefix: "train-".into(),
            noise_prefixes: vec!["Permission allow rule".into(), "Warning:".into()],
            report_window: 25,
            summary_chars: 1500,
            shown_commits: 10,
            shown_files: 30,
            tail_lines: 8,
            escalation_lines: 20,
            nudge_quiet_minutes: 45,
            brief: BriefConfig::default(),
            providers: ProviderTables::default(),
        }
    }
}

impl DispatchConfig {
    /// `<P>_<name>`.
    pub fn env(&self, name: &str) -> String {
        format!("{}_{name}", self.env_prefix)
    }

    /// Backoff before retrying attempt `n` (1-based).
    pub fn backoff_for(&self, attempt: u32) -> i64 {
        let i = (attempt.max(1) - 1) as usize;
        self.backoff
            .get(i)
            .or(self.backoff.last())
            .copied()
            .unwrap_or(0)
    }

    /// The PR a `hands-N` / `hands-N~k` key targets.
    pub fn hands_pr(&self, key: &str) -> Option<u64> {
        let base = super::model::RunKey::parse(key).base;
        let n = base.strip_prefix(&self.hands_prefix)?;
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            n.parse().ok()
        } else {
            None
        }
    }

    pub fn hands_key(&self, pr: u64) -> String {
        format!("{}{pr}", self.hands_prefix)
    }

    /// Refresh live controls from an env file text before a tick reads work (R81).
    /// Out-of-range values report a problem without aborting the tick or corrupting config.
    pub fn refresh_live(&self, env_text: &str) -> (Self, Option<String>) {
        let mut next = self.clone();
        let mut problem = None;
        let slots_key = self.env("AGENT_SLOTS");
        let cpu_key = self.env("MAX_CPU_PERCENT");
        for raw in env_text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim().trim_matches('\'').trim_matches('"');
            if k == slots_key || k.ends_with("_AGENT_SLOTS") {
                match v.parse::<i32>() {
                    Ok(n) if (0..=64).contains(&n) => next.slots = n,
                    _ => problem = Some(format!("{k}={v} is out of range (expected 0..=64)")),
                }
            } else if k == cpu_key || k.ends_with("_MAX_CPU_PERCENT") {
                match v.parse::<u32>() {
                    Ok(n) if (1..=100).contains(&n) => next.max_cpu_percent = n,
                    _ => problem = Some(format!("{k}={v} is out of range (expected 1..=100)")),
                }
            }
        }
        (next, problem)
    }
}
