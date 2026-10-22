//! Closed enumeration of event kinds and their structural mappings.
//!
//! Every event in tachograph belongs to one of 33 closed kinds. This module
//! defines `Kind`, the static list `Kind::ALL`, and pure mappings from each
//! kind to its Perfetto TrackEvent type, destination ring buffer class,
//! display name, extension field number, and begin/end pairing.

/// Destination ring buffer class for an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RingClass {
    /// Holds lifecycle, tasks, agent state, milestones, providers, custom events.
    Core,
    /// Holds runs, turns, tool calls, commands, files, waits, messages.
    Agents,
    /// Holds sampled machine counters.
    Machine,
}

impl RingClass {
    /// Trusted packet sequence ID assigned to this ring's writer sequence.
    /// Values 0x7AC0_0001..=0x7AC0_0003 are distinct from traced IDs.
    #[inline]
    pub const fn sequence_id(&self) -> u32 {
        match self {
            RingClass::Core => 0x7AC0_0001,
            RingClass::Agents => 0x7AC0_0002,
            RingClass::Machine => 0x7AC0_0003,
        }
    }
}

/// TrackEvent event type for Perfetto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum TrackEventType {
    Unspecified = 0,
    SliceBegin = 1,
    SliceEnd = 2,
    Instant = 3,
    Counter = 4,
    State = 5,
}

/// The closed set of all 33 event kinds supported by tachograph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Kind {
    Run = 0,
    RunEnded = 1,
    Turn = 2,
    TurnEnded = 3,
    ToolCall = 4,
    ToolCallEnded = 5,
    Command = 6,
    CommandEnded = 7,
    FileAccess = 8,
    FileAccessEnded = 9,
    FileEdited = 10,
    Wait = 11,
    WaitEnded = 12,
    Prompt = 13,
    Message = 14,
    Compacted = 15,
    Stall = 16,
    ProviderError = 17,
    TaskQueued = 18,
    TaskState = 19,
    AgentState = 20,
    Milestone = 21,
    Review = 22,
    ReviewEnded = 23,
    ReviewFinding = 24,
    Snapshot = 25,
    SnapshotEnded = 26,
    Daemon = 27,
    ClockStep = 28,
    TrackCreated = 29,
    TraceError = 30,
    Custom = 31,
    CustomEnded = 32,
}

impl Kind {
    /// Complete array of all 33 event kinds in canonical discriminant order.
    pub const ALL: [Kind; 33] = [
        Kind::Run,
        Kind::RunEnded,
        Kind::Turn,
        Kind::TurnEnded,
        Kind::ToolCall,
        Kind::ToolCallEnded,
        Kind::Command,
        Kind::CommandEnded,
        Kind::FileAccess,
        Kind::FileAccessEnded,
        Kind::FileEdited,
        Kind::Wait,
        Kind::WaitEnded,
        Kind::Prompt,
        Kind::Message,
        Kind::Compacted,
        Kind::Stall,
        Kind::ProviderError,
        Kind::TaskQueued,
        Kind::TaskState,
        Kind::AgentState,
        Kind::Milestone,
        Kind::Review,
        Kind::ReviewEnded,
        Kind::ReviewFinding,
        Kind::Snapshot,
        Kind::SnapshotEnded,
        Kind::Daemon,
        Kind::ClockStep,
        Kind::TrackCreated,
        Kind::TraceError,
        Kind::Custom,
        Kind::CustomEnded,
    ];

    /// Returns the Perfetto TrackEvent Type enum value.
    #[inline]
    pub const fn track_event_type(&self) -> TrackEventType {
        match self {
            Kind::Run
            | Kind::Turn
            | Kind::ToolCall
            | Kind::Command
            | Kind::FileAccess
            | Kind::Wait
            | Kind::Review
            | Kind::Snapshot
            | Kind::Custom => TrackEventType::SliceBegin,

            Kind::RunEnded
            | Kind::TurnEnded
            | Kind::ToolCallEnded
            | Kind::CommandEnded
            | Kind::FileAccessEnded
            | Kind::WaitEnded
            | Kind::ReviewEnded
            | Kind::SnapshotEnded
            | Kind::CustomEnded => TrackEventType::SliceEnd,

            Kind::TaskState | Kind::AgentState => TrackEventType::State,

            Kind::FileEdited
            | Kind::Prompt
            | Kind::Message
            | Kind::Compacted
            | Kind::Stall
            | Kind::ProviderError
            | Kind::TaskQueued
            | Kind::Milestone
            | Kind::ReviewFinding
            | Kind::Daemon
            | Kind::ClockStep
            | Kind::TrackCreated
            | Kind::TraceError => TrackEventType::Instant,
        }
    }

    /// Returns the ring buffer where this event kind should be placed.
    #[inline]
    pub const fn ring_class(&self) -> RingClass {
        match self {
            Kind::TaskQueued
            | Kind::TaskState
            | Kind::Milestone
            | Kind::Review
            | Kind::ReviewEnded
            | Kind::ReviewFinding
            | Kind::AgentState
            | Kind::ProviderError
            | Kind::Custom
            | Kind::CustomEnded
            | Kind::Daemon
            | Kind::ClockStep
            | Kind::TrackCreated
            | Kind::TraceError
            | Kind::Snapshot
            | Kind::SnapshotEnded => RingClass::Core,

            Kind::Run
            | Kind::RunEnded
            | Kind::Turn
            | Kind::TurnEnded
            | Kind::ToolCall
            | Kind::ToolCallEnded
            | Kind::Command
            | Kind::CommandEnded
            | Kind::FileAccess
            | Kind::FileAccessEnded
            | Kind::FileEdited
            | Kind::Wait
            | Kind::WaitEnded
            | Kind::Prompt
            | Kind::Message
            | Kind::Compacted
            | Kind::Stall => RingClass::Agents,
        }
    }

    /// Canonical display name used for interning and UI representation.
    #[inline]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Kind::Run => "Run",
            Kind::RunEnded => "RunEnded",
            Kind::Turn => "Turn",
            Kind::TurnEnded => "TurnEnded",
            Kind::ToolCall => "ToolCall",
            Kind::ToolCallEnded => "ToolCallEnded",
            Kind::Command => "Command",
            Kind::CommandEnded => "CommandEnded",
            Kind::FileAccess => "FileAccess",
            Kind::FileAccessEnded => "FileAccessEnded",
            Kind::FileEdited => "FileEdited",
            Kind::Wait => "Wait",
            Kind::WaitEnded => "WaitEnded",
            Kind::Prompt => "Prompt",
            Kind::Message => "Message",
            Kind::Compacted => "Compacted",
            Kind::Stall => "Stall",
            Kind::ProviderError => "ProviderError",
            Kind::TaskQueued => "TaskQueued",
            Kind::TaskState => "TaskState",
            Kind::AgentState => "AgentState",
            Kind::Milestone => "Milestone",
            Kind::Review => "Review",
            Kind::ReviewEnded => "ReviewEnded",
            Kind::ReviewFinding => "ReviewFinding",
            Kind::Snapshot => "Snapshot",
            Kind::SnapshotEnded => "SnapshotEnded",
            Kind::Daemon => "Daemon",
            Kind::ClockStep => "ClockStep",
            Kind::TrackCreated => "TrackCreated",
            Kind::TraceError => "TraceError",
            Kind::Custom => "Custom",
            Kind::CustomEnded => "CustomEnded",
        }
    }

    /// Extension field number in `perfetto.protos.TrackEvent` (4003..=4035).
    #[inline]
    pub const fn extension_field_number(&self) -> u32 {
        4003 + (*self as u32)
    }

    /// Interned string ID for the event name (discriminant + 1, constant per build).
    #[inline]
    pub const fn name_iid(&self) -> u64 {
        (*self as u64) + 1
    }

    /// For slice kinds, returns the opposite half (Begin <-> End).
    /// For instants and state events, returns `None`.
    #[inline]
    pub const fn pairing(&self) -> Option<Kind> {
        match self {
            Kind::Run => Some(Kind::RunEnded),
            Kind::RunEnded => Some(Kind::Run),
            Kind::Turn => Some(Kind::TurnEnded),
            Kind::TurnEnded => Some(Kind::Turn),
            Kind::ToolCall => Some(Kind::ToolCallEnded),
            Kind::ToolCallEnded => Some(Kind::ToolCall),
            Kind::Command => Some(Kind::CommandEnded),
            Kind::CommandEnded => Some(Kind::Command),
            Kind::FileAccess => Some(Kind::FileAccessEnded),
            Kind::FileAccessEnded => Some(Kind::FileAccess),
            Kind::Wait => Some(Kind::WaitEnded),
            Kind::WaitEnded => Some(Kind::Wait),
            Kind::Review => Some(Kind::ReviewEnded),
            Kind::ReviewEnded => Some(Kind::Review),
            Kind::Snapshot => Some(Kind::SnapshotEnded),
            Kind::SnapshotEnded => Some(Kind::Snapshot),
            Kind::Custom => Some(Kind::CustomEnded),
            Kind::CustomEnded => Some(Kind::Custom),
            _ => None,
        }
    }

    /// Returns `true` if this kind marks the beginning of a duration slice.
    #[inline]
    pub const fn is_begin(&self) -> bool {
        matches!(self.track_event_type(), TrackEventType::SliceBegin)
    }

    /// Returns `true` if this kind marks the end of a duration slice.
    #[inline]
    pub const fn is_end(&self) -> bool {
        matches!(self.track_event_type(), TrackEventType::SliceEnd)
    }

    /// Returns `true` if this kind is an instantaneous point-in-time event.
    #[inline]
    pub const fn is_instant(&self) -> bool {
        matches!(self.track_event_type(), TrackEventType::Instant)
    }

    /// Returns `true` if this kind is a state track event.
    #[inline]
    pub const fn is_state(&self) -> bool {
        matches!(self.track_event_type(), TrackEventType::State)
    }
}
