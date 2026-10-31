//! Core domain types and encoding logic for tachograph.
//!
//! Pure crate: no IO, no threads, no clocks.
//!
//! Provides the event model (`Kind`, `Event`), track registry, zero-allocation
//! wire encoder for Perfetto traces, privacy scrubber, command classifier,
//! and lane allocator.

pub mod classify;
pub mod decide;
pub mod detect;
pub mod encoder;
pub mod event;
pub mod input;
pub mod kind;
pub mod lane;
pub mod live;
pub mod project;
pub mod recorder;
pub mod scrub;
pub mod track;
pub mod wire;

pub use classify::{classify, CommandClass};
pub use detect::{blake3_digest_8, AgentDetectorState, DetectorConfig, DetectorFiring};
pub use encoder::{encode_snapshot_header, Encoder};
pub use event::Event;
pub use input::{
    AgentRole, CustomOp, FileEditKind, FindingSeverity, Input, MessageDirection, SessionKey,
    TaskStatus, ToolInput, WaitOn,
};
pub use kind::{Kind, RingClass, TrackEventType};
pub use lane::{AssignedTrack, LaneAllocator, LaneError};
pub use live::{
    render_report, render_status, AgentLiveState, LiveState, RecentCommand, TimelineItem,
};
pub use recorder::Recorder;
pub use scrub::{relativize_path, truncate_utf8, Scrubber};
pub use track::{compute_uuid, TrackEntry, TrackKind, TrackRegistry};

/// Resolves the `trace_processor_shell` oracle binary path in a single canonical place:
/// 1. `$TACHO_TP`
/// 2. `$TACHO_TRACE_PROCESSOR`
/// 3. `${XDG_CACHE_HOME:-$HOME/.cache}/tacho/trace_processor_shell` (if >= v57, supporting state tracks)
/// 4. Runs `scripts/oracle.sh` if present in the workspace to populate `$XDG_CACHE_HOME/tacho/trace_processor_shell`.
/// 5. `PATH` lookup for `trace_processor_shell` (preferring >= v57).
pub fn find_trace_processor() -> Option<std::path::PathBuf> {
    use std::env;
    use std::path::PathBuf;

    let supports_state_tracks = |bin: &PathBuf| -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .map(|out| {
                let s = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                s.contains("v57.") || s.contains("v58.") || s.contains("v59.") || s.contains("v6")
            })
            .unwrap_or(false)
    };

    for var in ["TACHO_TP", "TACHO_TRACE_PROCESSOR"] {
        if let Ok(val) = env::var(var) {
            let p = PathBuf::from(val.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let cache_root = env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".cache"))
        });

    if let Some(ref root) = cache_root {
        let cached = root.join("tacho/trace_processor_shell");
        if cached.is_file() && supports_state_tracks(&cached) {
            return Some(cached);
        }
    }

    // Attempt to invoke workspace scripts/oracle.sh before falling back to PATH so an older
    // system trace_processor_shell without TYPE_STATE = 5 does not shadow the pinned v58.3 binary.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(ws_root) = manifest_dir.parent().and_then(|p| p.parent()) {
        let script = ws_root.join("scripts/oracle.sh");
        if script.is_file() {
            if let Ok(out) = std::process::Command::new(&script).output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    let p = PathBuf::from(s.trim());
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }

    if let Ok(path_var) = env::var("PATH") {
        let mut fallback = None;
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("trace_processor_shell");
            if candidate.is_file() {
                if supports_state_tracks(&candidate) {
                    return Some(candidate);
                }
                if fallback.is_none() {
                    fallback = Some(candidate);
                }
            }
        }
        if fallback.is_some() {
            return fallback;
        }
    }

    None
}

/// Return at most the first `max_chars` characters of `s`.
///
/// SHAs and ids come from untrusted sources (git output, logs, API payloads); byte slicing
/// like `&s[..8]` panics when a multi-byte character straddles the cut, this never does.
pub(crate) fn char_prefix(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod char_prefix_tests {
    use super::char_prefix;

    #[test]
    fn test_char_prefix_never_splits_multibyte_chars() {
        assert_eq!(char_prefix("abcdef0123", 8), "abcdef01");
        assert_eq!(char_prefix("abc", 8), "abc");
        assert_eq!(char_prefix("", 8), "");
        assert_eq!(char_prefix("ééééééééé", 8), "éééééééé");
        assert_eq!(char_prefix("abcdefg🦀xyz", 8), "abcdefg🦀");
    }
}
