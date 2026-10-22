pub mod classify;
pub mod claude;
pub mod gemini;
pub mod jetski;

pub use classify::{
    blake3_digest_8, blake3_digest_8_bytes, canonical_json, classify_tool, count_lines,
    parse_exit_status,
};

/// Contextual environment passed when a hook is invoked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEnv {
    /// Agent identity (from TACHO_AGENT).
    pub agent: Option<String>,
    /// Task identifier (from TACHO_TASK).
    pub task: Option<String>,
    /// Invocation timestamp in nanoseconds since UNIX epoch.
    pub invoked_at_ns: u64,
    /// Parent process ID if known.
    pub ppid: Option<u32>,
}

impl HookEnv {
    /// Creates a new HookEnv with the given invocation timestamp.
    pub fn new(invoked_at_ns: u64) -> Self {
        Self {
            agent: None,
            task: None,
            invoked_at_ns,
            ppid: None,
        }
    }
}

/// Diagnostic counters for unknown payloads or missing fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookDiagnostics {
    pub unknown_payloads: u64,
    pub missing_fields: u64,
}
