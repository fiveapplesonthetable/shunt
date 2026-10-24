//! Structured phase notes (`read`, `write`, `test`, `fix`, `push`) for agent progress.
//!
//! Requirements R27–R30:
//! - Syntax: `<phase>: <line>`. Refused (`None`) when the phase is unknown, there is no colon,
//!   the line is empty, the line exceeds `c.note_limit` (200 chars), or the line contains a newline.
//! - `with_note` keeps the newest `c.notes_kept` (50) notes per task base key.
//! - `current_phase`: newest note's phase and line; `since` is the timestamp of the first note
//!   in the trailing run of consecutive notes with that same phase. Empty notes -> `None`.
//! - `phase_is_long` iff `now - since > c.phase_long` (7200 s, strict `>`).

use super::config::WatchConfig;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Canonical agent work phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Read,
    Write,
    Test,
    Fix,
    Push,
}

impl Phase {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Phase::Read => "read",
            Phase::Write => "write",
            Phase::Test => "test",
            Phase::Fix => "fix",
            Phase::Push => "push",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Phase::Read),
            "write" => Some(Phase::Write),
            "test" => Some(Phase::Test),
            "fix" => Some(Phase::Fix),
            "push" => Some(Phase::Push),
            _ => None,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed `<phase>: <line>` note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub phase: Phase,
    pub phase_name: String,
    pub line: String,
}

/// A timestamped note stored in an agent's note list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteAt {
    pub at: i64,
    pub phase: String,
    pub line: String,
}

impl NoteAt {
    pub fn new(at: i64, phase: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            at,
            phase: phase.into(),
            line: line.into(),
        }
    }
}

/// The current phase of an agent and when that consecutive phase streak began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Current {
    pub phase: String,
    pub since: i64,
    pub line: String,
}

/// Parse `<phase>: <one line>`, returning `None` when:
/// - there is no `:` separator,
/// - `<phase>` is not in `c.phases`,
/// - `<line>` (trimmed) is empty, exceeds `c.note_limit` characters, or contains a newline.
pub fn parse_note(text: &str, c: &WatchConfig) -> Option<Note> {
    let (raw_phase, raw_line) = text.split_once(':')?;
    let phase_str = raw_phase.trim();
    let line = raw_line.trim();
    if line.is_empty()
        || line.chars().count() > c.note_limit
        || line.contains('\n')
        || line.contains('\r')
    {
        return None;
    }
    if !c.phases.iter().any(|p| p == phase_str) {
        return None;
    }
    let phase = Phase::from_name(phase_str).unwrap_or(Phase::Read);
    Some(Note {
        phase,
        phase_name: phase_str.to_string(),
        line: line.to_string(),
    })
}

/// Append `note` at timestamp `now` to `notes`, keeping at most the newest `c.notes_kept` (50) entries.
pub fn with_note(notes: &[NoteAt], note: Note, now: i64, c: &WatchConfig) -> Vec<NoteAt> {
    let mut out = notes.to_vec();
    out.push(NoteAt {
        at: now,
        phase: note.phase_name,
        line: note.line,
    });
    let keep = c.notes_kept.max(1);
    if out.len() > keep {
        let drop_count = out.len() - keep;
        out.drain(0..drop_count);
    }
    out
}

/// Return the newest note's phase and line, with `since` set to the timestamp of the first note
/// in the trailing consecutive run of that phase. Returns `None` when `notes` is empty.
pub fn current_phase(notes: &[NoteAt]) -> Option<Current> {
    let last = notes.last()?;
    let mut since = last.at;
    for note in notes.iter().rev() {
        if note.phase != last.phase {
            break;
        }
        since = note.at;
    }
    Some(Current {
        phase: last.phase.clone(),
        since,
        line: last.line.clone(),
    })
}

/// True iff `current` is `Some` and `now - current.since > c.phase_long` (strictly greater than 2 hours).
pub fn phase_is_long(current: Option<&Current>, now: i64, c: &WatchConfig) -> bool {
    current.is_some_and(|cur| now - cur.since > c.phase_long)
}
