//! In-memory live state for flight recorder queries (design.md section 10.1).
//!
//! Provides `LiveState` tracking per-agent activity, status rendering (`tacho status`),
//! and timeline reports (`tacho report`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A recorded command invocation in the agent's recent history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentCommand {
    pub argv: String,
    pub class: String,
    pub exit_code: Option<i32>,
    pub ts_ns: u64,
}

/// A significant event on the agent's timeline for reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineItem {
    pub ts_ns: u64,
    pub kind: String,
    pub summary: String,
    pub detail: Option<String>,
}

/// Real-time flight recorder status for a single agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLiveState {
    pub agent_id: String,
    pub provider: String,
    pub session_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub task: Option<String>,
    pub state: String,
    pub state_since_ns: u64,
    pub run_n: u32,
    pub turn_n: u32,
    pub last_action: String,
    pub tokens_out: u64,
    pub tokens_in: u64,
    pub tokens_cached: u64,
    pub cost_micros: u64,
    pub flags: Vec<String>,
    pub recent_commands: Vec<RecentCommand>,
    pub timeline: Vec<TimelineItem>,
}

impl AgentLiveState {
    /// Creates a new initial live state for an agent.
    pub fn new(
        agent_id: impl Into<String>,
        provider: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            provider: provider.into(),
            session_id: session_id.into(),
            model: None,
            cwd: None,
            task: None,
            state: String::new(),
            state_since_ns: 0,
            run_n: 1,
            turn_n: 0,
            last_action: "-".to_string(),
            tokens_out: 0,
            tokens_in: 0,
            tokens_cached: 0,
            cost_micros: 0,
            flags: Vec::new(),
            recent_commands: Vec::new(),
            timeline: Vec::new(),
        }
    }
}

/// Complete in-memory live state across all observed agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveState {
    pub agents: BTreeMap<String, AgentLiveState>,
}

impl LiveState {
    /// Creates a new empty live state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accesses an agent's state or inserts default.
    pub fn agent_mut(&mut self, id: &str) -> Option<&mut AgentLiveState> {
        self.agents.get_mut(id)
    }
}

/// Formats a nanosecond duration into a human-readable string (e.g. "12s", "6m", "2h").
pub fn format_duration(dur_ns: u64) -> String {
    let secs = dur_ns / 1_000_000_000;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Formats token counts with SI suffixes (e.g. 41000 -> "41k", 1500000 -> "1.5M").
pub fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if millions.fract() < 0.05 {
            format!("{:.0}M", millions)
        } else {
            format!("{:.1}M", millions)
        }
    } else if count >= 1_000 {
        format!("{}k", count / 1_000)
    } else {
        count.to_string()
    }
}

/// Formats `tacho status` table or JSON representation matching design.md 10.1.
pub fn render_status(live: &LiveState, json: bool, now_ns: u64) -> String {
    if json {
        #[derive(Serialize)]
        struct JsonAgentStatus<'a> {
            agent: &'a str,
            state: &'a str,
            for_seconds: u64,
            run: u32,
            turn: u32,
            last_action: &'a str,
            tokens_out: u64,
            flags: &'a [String],
        }

        let list: Vec<_> = live
            .agents
            .values()
            .map(|a| {
                let dur_secs = now_ns.saturating_sub(a.state_since_ns) / 1_000_000_000;
                JsonAgentStatus {
                    agent: &a.agent_id,
                    state: &a.state,
                    for_seconds: dur_secs,
                    run: a.run_n,
                    turn: a.turn_n,
                    last_action: &a.last_action,
                    tokens_out: a.tokens_out,
                    flags: &a.flags,
                }
            })
            .collect();
        serde_json::to_string_pretty(&list).unwrap_or_else(|_| "[]".to_string())
    } else {
        let mut out = String::new();
        out.push_str(
            "agent      state     for    run  turn  last action                tokens out  flags\n",
        );
        for agent in live.agents.values() {
            let for_str = format_duration(now_ns.saturating_sub(agent.state_since_ns));
            let tokens_str = format_tokens(agent.tokens_out);
            let flags_str = agent.flags.join(" ");

            let mut act = agent.last_action.clone();
            if act.len() > 26 {
                act.truncate(25);
                act.push('…');
            }

            out.push_str(&format!(
                "{:<10} {:<9} {:<6} {:<4} {:<5} {:<26} {:>10}  {}\n",
                agent.agent_id,
                agent.state,
                for_str,
                agent.run_n,
                agent.turn_n,
                act,
                tokens_str,
                flags_str
            ));
        }
        out
    }
}

/// Formats `tacho report <agent>` detailed timeline matching design.md 10.1.
pub fn render_report(
    live: &LiveState,
    agent_id: &str,
    since_ns: Option<u64>,
    now_ns: u64,
) -> String {
    let agent = match live.agents.get(agent_id) {
        Some(a) => a,
        None => return format!("Agent '{}' not found in live state.\n", agent_id),
    };

    let start_ts = since_ns.unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "=== Agent Report: {} (Run {}, State: {}, Total Tokens Out: {}) ===\n",
        agent.agent_id,
        agent.run_n,
        agent.state,
        format_tokens(agent.tokens_out)
    ));
    if let Some(ref m) = agent.model {
        out.push_str(&format!("Model: {}\n", m));
    }
    if let Some(ref cwd) = agent.cwd {
        out.push_str(&format!("Directory: {}\n", cwd));
    }
    if let Some(ref task) = agent.task {
        out.push_str(&format!("Task: {}\n", task));
    }
    out.push_str("\n--- Timeline ---\n");

    let items: Vec<_> = agent
        .timeline
        .iter()
        .filter(|item| item.ts_ns >= start_ts)
        .collect();

    if items.is_empty() {
        out.push_str("(no activity in requested time window)\n");
    } else {
        for item in items {
            let offset_ns = item.ts_ns.saturating_sub(start_ts);
            let offset_str = format_duration(offset_ns);
            out.push_str(&format!(
                "[+{:<4}] {:<12} {}",
                offset_str, item.kind, item.summary
            ));
            if let Some(ref detail) = item.detail {
                out.push_str(&format!(" ({})", detail));
            }
            out.push('\n');
        }
    }

    let _ = now_ns;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_helpers() {
        assert_eq!(format_duration(12_000_000_000), "12s");
        assert_eq!(format_duration(360_000_000_000), "6m");
        assert_eq!(format_duration(7200 * 1_000_000_000), "2h");

        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(41_000), "41k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_render_status_table() {
        let mut live = LiveState::new();
        let mut a1 = AgentLiveState::new("worker-1", "claude", "sess-1");
        a1.state = "Tool".to_string();
        a1.state_since_ns = 1_000_000_000;
        a1.run_n = 1;
        a1.turn_n = 14;
        a1.last_action = "Test cargo test -p core".to_string();
        a1.tokens_out = 41_000;

        let mut a2 = AgentLiveState::new("worker-2", "gemini", "sess-2");
        a2.state = "Stalled".to_string();
        a2.state_since_ns = 1_000_000_000;
        a2.run_n = 2;
        a2.turn_n = 31;
        a2.last_action = "Read src/lib.rs (x4)".to_string();
        a2.tokens_out = 88_000;
        a2.flags.push("LOOP".to_string());

        live.agents.insert("worker-1".to_string(), a1);
        live.agents.insert("worker-2".to_string(), a2);

        let table = render_status(&live, false, 13_000_000_000);
        assert!(table.contains("agent      state     for    run  turn  last action"));
        assert!(table.contains("worker-1"));
        assert!(table.contains("Test cargo test -p core"));
        assert!(table.contains("41k"));
        assert!(table.contains("worker-2"));
        assert!(table.contains("Read src/lib.rs (x4)"));
        assert!(table.contains("88k"));
        assert!(table.contains("LOOP"));

        let json = render_status(&live, true, 13_000_000_000);
        assert!(json.contains("\"agent\": \"worker-1\""));
        assert!(json.contains("\"tokens_out\": 41000"));
    }

    #[test]
    fn test_render_report() {
        let mut live = LiveState::new();

        // Non-existent agent
        let not_found = render_report(&live, "unknown-agent", None, 10_000_000_000);
        assert!(not_found.contains("Agent 'unknown-agent' not found"));

        let mut a = AgentLiveState::new("core-1", "claude-3-5", "session-xyz");
        a.run_n = 1;
        a.state = "Thinking".to_string();
        a.model = Some("claude-3-5-sonnet".to_string());
        a.cwd = Some("/repo/tachograph".to_string());
        a.task = Some("Implement tacho-core".to_string());
        a.tokens_out = 12_500;

        a.timeline.push(TimelineItem {
            ts_ns: 1_000_000_000,
            kind: "Prompt".to_string(),
            summary: "User sent prompt".to_string(),
            detail: Some("digest: abc".to_string()),
        });
        a.timeline.push(TimelineItem {
            ts_ns: 2_500_000_000,
            kind: "ToolCall".to_string(),
            summary: "Execute cargo build".to_string(),
            detail: None,
        });
        a.timeline.push(TimelineItem {
            ts_ns: 5_000_000_000,
            kind: "FileEdit".to_string(),
            summary: "src/lib.rs (+10 -2)".to_string(),
            detail: Some("Edit".to_string()),
        });

        live.agents.insert("core-1".to_string(), a);

        // Full report from beginning
        let rep = render_report(&live, "core-1", None, 10_000_000_000);
        assert!(
            rep.contains("Agent Report: core-1 (Run 1, State: Thinking, Total Tokens Out: 12k)")
        );
        assert!(rep.contains("Model: claude-3-5-sonnet"));
        assert!(rep.contains("Directory: /repo/tachograph"));
        assert!(rep.contains("Task: Implement tacho-core"));
        assert!(rep.contains("Prompt"));
        assert!(rep.contains("User sent prompt (digest: abc)"));
        assert!(rep.contains("ToolCall"));
        assert!(rep.contains("Execute cargo build"));
        assert!(rep.contains("FileEdit"));

        // Report with since_ns filtering
        let rep_since = render_report(&live, "core-1", Some(3_000_000_000), 10_000_000_000);
        assert!(!rep_since.contains("User sent prompt"));
        assert!(!rep_since.contains("Execute cargo build"));
        assert!(rep_since.contains("FileEdit"));
        assert!(rep_since.contains("src/lib.rs"));

        // Report with since_ns past all events
        let rep_empty = render_report(&live, "core-1", Some(10_000_000_000), 10_000_000_000);
        assert!(rep_empty.contains("(no activity in requested time window)"));
    }
}
