//! Provider-neutral, parsed flight recorder inputs.
//!
//! Represents all inputs accepted by tachograph from hooks, `tacho run`, OTLP,
//! `tacho emit`, and `api.sock` NDJSON streams. This enum is pure data,
//! independent of storage, threads, and time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tacho_proto::tacho;

/// Unique identifier for an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Provider name (e.g. "claude", "gemini", "codex", "jetski").
    pub provider: String,
    /// Session identifier within the provider.
    pub session_id: String,
}

impl SessionKey {
    /// Creates a new session key.
    pub fn new(provider: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            session_id: session_id.into(),
        }
    }
}

/// Operation type for custom spans, points, counters, or state tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomOp {
    Begin,
    End,
    Instant,
    Counter,
    State,
}

/// File edit operation category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEditKind {
    Edit,
    Create,
    Delete,
    Rename,
}

impl From<FileEditKind> for tacho::FileEditKind {
    fn from(k: FileEditKind) -> Self {
        match k {
            FileEditKind::Edit => tacho::FileEditKind::Edit,
            FileEditKind::Create => tacho::FileEditKind::Create,
            FileEditKind::Delete => tacho::FileEditKind::Delete,
            FileEditKind::Rename => tacho::FileEditKind::Rename,
        }
    }
}

/// Wait reason category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOn {
    Input,
    Message,
    Subagent,
    RateLimit,
    Other,
}

impl From<WaitOn> for tacho::WaitOn {
    fn from(w: WaitOn) -> Self {
        match w {
            WaitOn::Input => tacho::WaitOn::WaitInput,
            WaitOn::Message => tacho::WaitOn::WaitMessage,
            WaitOn::Subagent => tacho::WaitOn::WaitSubagent,
            WaitOn::RateLimit => tacho::WaitOn::WaitRateLimit,
            WaitOn::Other => tacho::WaitOn::WaitOther,
        }
    }
}

/// Direction of a message exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    Sent,
    Delivered,
}

impl From<MessageDirection> for tacho::MessageDirection {
    fn from(d: MessageDirection) -> Self {
        match d {
            MessageDirection::Sent => tacho::MessageDirection::Sent,
            MessageDirection::Delivered => tacho::MessageDirection::Delivered,
        }
    }
}

/// Lifecycle status for task tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    InProgress,
    Review,
    Done,
    Failed,
    Cancelled,
}

impl From<TaskStatus> for tacho::TaskStatus {
    fn from(s: TaskStatus) -> Self {
        match s {
            TaskStatus::Queued => tacho::TaskStatus::TaskQueued,
            TaskStatus::InProgress => tacho::TaskStatus::TaskInProgress,
            TaskStatus::Review => tacho::TaskStatus::TaskReview,
            TaskStatus::Done => tacho::TaskStatus::TaskDone,
            TaskStatus::Failed => tacho::TaskStatus::TaskFailed,
            TaskStatus::Cancelled => tacho::TaskStatus::TaskCancelled,
        }
    }
}

/// Severity level for review findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Info,
    Minor,
    Major,
    Critical,
}

impl From<FindingSeverity> for tacho::FindingSeverity {
    fn from(s: FindingSeverity) -> Self {
        match s {
            FindingSeverity::Info => tacho::FindingSeverity::FindingInfo,
            FindingSeverity::Minor => tacho::FindingSeverity::FindingMinor,
            FindingSeverity::Major => tacho::FindingSeverity::FindingMajor,
            FindingSeverity::Critical => tacho::FindingSeverity::FindingCritical,
        }
    }
}

/// Parsed, provider-neutral representation of a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolInput {
    Shell {
        argv: String,
        cwd: Option<String>,
        #[serde(default)]
        git_tree: Option<String>,
    },
    FileRead {
        path: String,
    },
    FileSearch {
        pattern: String,
        path: Option<String>,
    },
    FileEdit {
        path: String,
        #[serde(alias = "edit_kind", default)]
        kind: Option<FileEditKind>,
        lines_added: Option<u32>,
        lines_removed: Option<u32>,
    },
    Subagent {
        description_len: Option<u64>,
    },
    Other {
        input_digest: Option<String>,
        input_bytes: Option<u64>,
    },
}

/// High-level input event parsed from any ingestion source.
///
/// Serializes to and deserializes from NDJSON with a stable `type` discriminator tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    RunStart {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        provider: String,
        session_id: String,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        parent_session: Option<String>,
        #[serde(default)]
        git_tree: Option<String>,
    },
    RunEnd {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        reason: Option<String>,
    },
    Prompt {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        text_len: u64,
        #[serde(default)]
        digest: Option<String>,
    },
    TurnStart {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        model: Option<String>,
    },
    TurnEnd {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        cached_tokens: Option<u64>,
        #[serde(default)]
        text_bytes: Option<u64>,
        #[serde(default)]
        cost_micros: Option<u64>,
    },
    ToolBegin {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        tool_use_id: String,
        tool_name: String,
        input: ToolInput,
    },
    ToolEnd {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        tool_use_id: String,
        error: bool,
        #[serde(default)]
        result_bytes: Option<u64>,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    Wait {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        on: Option<WaitOn>,
    },
    WaitEnd {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        timed_out: bool,
    },
    Message {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        from: String,
        to: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        bytes: Option<u64>,
        #[serde(default)]
        dir: Option<MessageDirection>,
    },
    Compacted {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        session: SessionKey,
        #[serde(default)]
        tokens_before: Option<u64>,
        #[serde(default)]
        tokens_after: Option<u64>,
        #[serde(default)]
        bytes_compacted: Option<u64>,
    },
    ProviderError {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        provider: String,
        #[serde(default)]
        error_code: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        rate_limit: bool,
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
    Task {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        slug: String,
        #[serde(default)]
        brief_digest: Option<String>,
        #[serde(default)]
        brief_bytes: Option<u64>,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        base_commit: Option<String>,
        #[serde(default)]
        token_budget: Option<u64>,
        #[serde(default)]
        cost_budget_micros: Option<u64>,
    },
    TaskState {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        slug: String,
        state: TaskStatus,
        #[serde(default)]
        because: Option<String>,
    },
    Finding {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        task: String,
        severity: FindingSeverity,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        title: String,
    },
    Custom {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        track: String,
        op: CustomOp,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        args: BTreeMap<String, String>,
        #[serde(default)]
        value: Option<i64>,
        #[serde(default)]
        flow_out: Option<String>,
        #[serde(default)]
        flow_in: Option<String>,
    },
    CounterSample {
        #[serde(default)]
        source_ts_ns: Option<u64>,
        track_key: String,
        value: i64,
        #[serde(default)]
        unit: Option<String>,
    },
}

impl Input {
    /// Returns the optional client-supplied timestamp in nanoseconds.
    pub fn source_ts_ns(&self) -> Option<u64> {
        match self {
            Input::RunStart { source_ts_ns, .. }
            | Input::RunEnd { source_ts_ns, .. }
            | Input::Prompt { source_ts_ns, .. }
            | Input::TurnStart { source_ts_ns, .. }
            | Input::TurnEnd { source_ts_ns, .. }
            | Input::ToolBegin { source_ts_ns, .. }
            | Input::ToolEnd { source_ts_ns, .. }
            | Input::Wait { source_ts_ns, .. }
            | Input::WaitEnd { source_ts_ns, .. }
            | Input::Message { source_ts_ns, .. }
            | Input::Compacted { source_ts_ns, .. }
            | Input::ProviderError { source_ts_ns, .. }
            | Input::Task { source_ts_ns, .. }
            | Input::TaskState { source_ts_ns, .. }
            | Input::Finding { source_ts_ns, .. }
            | Input::Custom { source_ts_ns, .. }
            | Input::CounterSample { source_ts_ns, .. } => *source_ts_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_serde_roundtrip() {
        let inputs = vec![
            Input::RunStart {
                source_ts_ns: Some(1_000_000),
                provider: "claude".to_string(),
                session_id: "sess-1".to_string(),
                agent: Some("worker-1".to_string()),
                task: Some("task-a".to_string()),
                pid: Some(101),
                cwd: Some("/home/dev".to_string()),
                model: Some("claude-3-5".to_string()),
                parent_session: None,
                git_tree: None,
            },
            Input::ToolBegin {
                source_ts_ns: None,
                session: SessionKey::new("claude", "sess-1"),
                tool_use_id: "tool-42".to_string(),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: "cargo test".to_string(),
                    cwd: Some("/home/dev".to_string()),
                    git_tree: None,
                },
            },
            Input::Finding {
                source_ts_ns: Some(2_000_000),
                task: "task-a".to_string(),
                severity: FindingSeverity::Major,
                path: Some("src/lib.rs".to_string()),
                line: Some(10),
                title: "Memory leak".to_string(),
            },
            Input::Custom {
                source_ts_ns: None,
                track: "custom:build".to_string(),
                op: CustomOp::Begin,
                name: Some("compile".to_string()),
                id: Some("span-1".to_string()),
                args: BTreeMap::from([("target".to_string(), "debug".to_string())]),
                value: None,
                flow_out: Some("f1".to_string()),
                flow_in: None,
            },
            Input::CounterSample {
                source_ts_ns: Some(3_000_000),
                track_key: "machine/cpu_busy".to_string(),
                value: 450,
                unit: Some("per mille".to_string()),
            },
        ];

        for input in inputs {
            let json = serde_json::to_string(&input).expect("serialize failed");
            let deserialized: Input = serde_json::from_str(&json).expect("deserialize failed");
            assert_eq!(input, deserialized);
        }
    }
}
