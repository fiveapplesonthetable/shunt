//! Watchdog liveness decisions and systemd unit drift detection.
//!
//! Keeps the OPEN / UPDATE / CLOSE issue lifecycle so a recovered daemon closes its down alert,
//! and checks systemd unit drift in shipped order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::OpsConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogPolicy {
    /// Seconds without a heartbeat before the daemon counts as down (default 15 min).
    pub silent_after: i64,
    /// Issue title for the down alert; if empty, defaults to `<tool> is down`.
    pub down_title: String,
    /// Number of trailing tick.log lines included in the issue body.
    pub tail_lines: usize,
}

impl Default for WatchdogPolicy {
    fn default() -> Self {
        Self {
            silent_after: 15 * 60,
            down_title: String::new(),
            tail_lines: 20,
        }
    }
}

impl WatchdogPolicy {
    pub fn title(&self, tool: &str) -> String {
        if self.down_title.is_empty() {
            format!("{tool} is down")
        } else {
            self.down_title.clone()
        }
    }
}

/// Why the daemon counts as down, checked in order: unit active, heartbeat exists, heartbeat age.
pub fn down_reason(
    active: bool,
    heartbeat_age: Option<i64>,
    policy: &WatchdogPolicy,
) -> Option<String> {
    if !active {
        return Some("the daemon is not active".into());
    }
    let Some(age) = heartbeat_age else {
        return Some("the daemon heartbeat does not exist".into());
    };
    if age > policy.silent_after {
        return Some(format!(
            "the daemon heartbeat has not advanced for {} minutes",
            age / 60
        ));
    }
    None
}

/// Format the issue body for a down alert with at most `tail_limit` trailing log lines.
pub fn down_body(tool: &str, reason: &str, tail: &[String], tail_limit: usize) -> String {
    let start = tail.len().saturating_sub(tail_limit);
    let shown = tail[start..].join("\n");
    format!("{tool} is down: {reason}.\n\nLast log lines:\n\n```\n{shown}\n```\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmKind {
    Open,
    Update,
    Close,
    None,
}

impl AlarmKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlarmKind::Open => "open",
            AlarmKind::Update => "update",
            AlarmKind::Close => "close",
            AlarmKind::None => "none",
        }
    }
}

/// An existing open `<tool> is down` issue on the code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenIssue {
    pub number: u64,
    pub body: String,
}

/// What the watchdog should do on the code host, with a human reason and CLI summary line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlarmAction {
    pub kind: AlarmKind,
    pub number: Option<u64>,
    pub title: String,
    pub body: String,
    pub reason: String,
    /// `<kind>: <reason|healthy>`, printed by `<tool> watchdog`.
    pub summary: String,
}

/// Decide the OPEN / UPDATE / CLOSE / NONE issue transition from `down_reason`.
pub fn down_alert(
    reason: Option<&str>,
    tail: &[String],
    open_issue: Option<&OpenIssue>,
    cfg: &OpsConfig,
) -> AlarmAction {
    let title = cfg.watchdog.title(&cfg.tool);
    let Some(why) = reason else {
        return match open_issue {
            Some(issue) => AlarmAction {
                kind: AlarmKind::Close,
                number: Some(issue.number),
                title,
                body: String::new(),
                reason: "daemon recovered; closing the open down issue".into(),
                summary: "close: healthy".into(),
            },
            None => AlarmAction {
                kind: AlarmKind::None,
                number: None,
                title,
                body: String::new(),
                reason: "daemon is healthy and no down issue is open".into(),
                summary: "none: healthy".into(),
            },
        };
    };
    let body = down_body(&cfg.tool, why, tail, cfg.watchdog.tail_lines);
    match open_issue {
        None => AlarmAction {
            kind: AlarmKind::Open,
            number: None,
            title,
            body,
            reason: format!("daemon is down ({why}) and no issue is open"),
            summary: format!("open: {why}"),
        },
        Some(issue) if issue.body == body => AlarmAction {
            kind: AlarmKind::None,
            number: Some(issue.number),
            title,
            body: String::new(),
            reason: "down issue already shows the current reason and log tail".into(),
            summary: format!("none: {why}"),
        },
        Some(issue) => AlarmAction {
            kind: AlarmKind::Update,
            number: Some(issue.number),
            title,
            body,
            reason: format!("down issue #{} body changed ({why})", issue.number),
            summary: format!("update: {why}"),
        },
    }
}

/// Names of shipped systemd units whose installed text is missing or differs, in shipped order.
pub fn drifted_units(
    shipped: &[(String, String)],
    installed: &BTreeMap<String, String>,
) -> Vec<String> {
    shipped
        .iter()
        .filter(|(name, text)| installed.get(name) != Some(text))
        .map(|(name, _)| name.clone())
        .collect()
}
