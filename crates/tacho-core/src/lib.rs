//! Core domain types and encoding logic for tachograph.
//!
//! Pure crate: no IO, no threads, no clocks.
//!
//! Provides the event model (`Kind`, `Event`), track registry, zero-allocation
//! wire encoder for Perfetto traces, privacy scrubber, command classifier,
//! and lane allocator.

pub mod classify;
pub mod encoder;
pub mod event;
pub mod kind;
pub mod lane;
pub mod scrub;
pub mod track;
pub mod wire;

pub use classify::{classify, CommandClass};
pub use encoder::{encode_snapshot_header, Encoder};
pub use event::Event;
pub use kind::{Kind, RingClass, TrackEventType};
pub use lane::{AssignedTrack, LaneAllocator, LaneError};
pub use scrub::{relativize_path, truncate_utf8, Scrubber};
pub use track::{compute_uuid, TrackEntry, TrackKind, TrackRegistry};
