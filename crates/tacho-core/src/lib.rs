//! Core domain types and encoding logic for tachograph.
//!
//! Pure crate: no IO, no threads, no clocks.
//!
//! Provides the event model (`Kind`, `Event`), track registry, zero-allocation
//! wire encoder for Perfetto traces, privacy scrubber, command classifier,
//! and lane allocator.

pub mod classify;
pub mod detect;
pub mod encoder;
pub mod event;
pub mod input;
pub mod kind;
pub mod lane;
pub mod live;
pub mod recorder;
pub mod scrub;
pub mod track;
pub mod wire;

pub use classify::{classify, CommandClass};
pub use detect::{blake3_digest_8, AgentDetectorState, DetectorConfig, DetectorFiring};
pub use encoder::{encode_snapshot_header, Encoder};
pub use event::Event;
pub use input::{
    CustomOp, FileEditKind, FindingSeverity, Input, MessageDirection, SessionKey, TaskStatus,
    ToolInput, WaitOn,
};
pub use kind::{Kind, RingClass, TrackEventType};
pub use lane::{AssignedTrack, LaneAllocator, LaneError};
pub use live::{
    render_report, render_status, AgentLiveState, LiveState, RecentCommand, TimelineItem,
};
pub use recorder::Recorder;
pub use scrub::{relativize_path, truncate_utf8, Scrubber};
pub use track::{compute_uuid, TrackEntry, TrackKind, TrackRegistry};
