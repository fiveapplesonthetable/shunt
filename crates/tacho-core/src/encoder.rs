//! Perfetto wire encoder for flight recorder events and trace snapshot headers.
//!
//! Encodes typed in-memory `Event`s into framed `TracePacket` bytes into a caller-supplied
//! `Vec<u8>`. After warm-up, encoding an event performs zero heap allocations.

use prost::Message;
use tacho_proto::tacho;
use tacho_proto::DESCRIPTOR_SET;

use crate::event::Event;
use crate::kind::{Kind, RingClass, TrackEventType};
use crate::track::{compute_uuid, encode_track_descriptor, TrackRegistry};
use crate::wire::{
    encode_bytes_field, encode_fixed64_field, encode_string_field, encode_varint_field,
    frame_packet,
};

/// Reusable encoder holding scratch buffers for zero-allocation event encoding.
#[derive(Debug, Default)]
pub struct Encoder {
    packet_buf: Vec<u8>,
    track_event_buf: Vec<u8>,
    body_buf: Vec<u8>,
}

impl Encoder {
    /// Creates a new encoder with default initial buffer capacities.
    pub fn new() -> Self {
        Self::with_capacity(2048)
    }

    /// Creates a new encoder pre-allocated to the given capacity to eliminate warm-up reallocations.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            packet_buf: Vec::with_capacity(capacity),
            track_event_buf: Vec::with_capacity(capacity),
            body_buf: Vec::with_capacity(capacity),
        }
    }

    /// Encodes an `Event` into a framed `TracePacket` appended to `out`.
    ///
    /// Reuses internal buffers; performs zero allocations once warmed up.
    pub fn encode_event(&mut self, event: &Event, registry: &TrackRegistry, out: &mut Vec<u8>) {
        self.packet_buf.clear();
        self.track_event_buf.clear();
        self.body_buf.clear();

        // 1. Serialize extension body message if present
        encode_event_body(&event.body, &mut self.body_buf);

        // 2. Build TrackEvent (TracePacket field 11)
        // 9: optional Type type = 9;
        let te_type = if event.counter_value.is_some() {
            4 // TYPE_COUNTER
        } else {
            match event.kind.track_event_type() {
                TrackEventType::SliceBegin => 1,
                TrackEventType::SliceEnd => 2,
                TrackEventType::Instant => 3,
                TrackEventType::Counter => 4,
                TrackEventType::State => 5,
                TrackEventType::Unspecified => 0,
            }
        };
        encode_varint_field(9, te_type, &mut self.track_event_buf);

        // 11: optional uint64 track_uuid = 11;
        let track_uuid = registry
            .get_by_key(&event.track_key)
            .map(|t| t.uuid)
            .unwrap_or_else(|| compute_uuid(&event.track_key));
        encode_varint_field(11, track_uuid, &mut self.track_event_buf);

        // Event name / name_iid (omitted on SliceEnd to merge into the open slice)
        if !event.kind.is_end() {
            if let Some(custom) = &event.custom_name {
                encode_string_field(23, custom, &mut self.track_event_buf);
            } else if event.kind == Kind::AgentState {
                let state_name = match &event.body.body {
                    Some(tacho::event::Body::AgentState(s)) => {
                        match s
                            .state
                            .and_then(|v| tacho::AgentStateType::try_from(v).ok())
                        {
                            Some(tacho::AgentStateType::Idle) => "Idle",
                            Some(tacho::AgentStateType::Thinking) => "Thinking",
                            Some(tacho::AgentStateType::Tool) => "Tool",
                            Some(tacho::AgentStateType::Waiting) => "Waiting",
                            Some(tacho::AgentStateType::Stalled) => "Stalled",
                            Some(tacho::AgentStateType::Exited) => "Exited",
                            _ => "AgentState",
                        }
                    }
                    _ => "AgentState",
                };
                encode_string_field(23, state_name, &mut self.track_event_buf);
            } else if event.kind == Kind::TaskState {
                let state_name = match &event.body.body {
                    Some(tacho::event::Body::TaskState(s)) => {
                        match s.state.and_then(|v| tacho::TaskStatus::try_from(v).ok()) {
                            Some(tacho::TaskStatus::TaskQueued) => "Queued",
                            Some(tacho::TaskStatus::TaskInProgress) => "InProgress",
                            Some(tacho::TaskStatus::TaskReview) => "Review",
                            Some(tacho::TaskStatus::TaskDone) => "Done",
                            Some(tacho::TaskStatus::TaskFailed) => "Failed",
                            Some(tacho::TaskStatus::TaskCancelled) => "Cancelled",
                            _ => "TaskState",
                        }
                    }
                    _ => "TaskState",
                };
                encode_string_field(23, state_name, &mut self.track_event_buf);
            } else if event.kind == Kind::Custom {
                let body_name = match &event.body.body {
                    Some(tacho::event::Body::Custom(c)) => c.name.as_deref(),
                    _ => None,
                };
                let custom_name = body_name.unwrap_or(event.kind.display_name());
                // 23: string name = 23; (inline for custom open set)
                encode_string_field(23, custom_name, &mut self.track_event_buf);
            } else {
                // 10: uint64 name_iid = 10; (interned for closed set)
                encode_varint_field(10, event.kind.name_iid(), &mut self.track_event_buf);
            }
        }

        // 22: repeated string categories = 22;
        if !event.kind.is_end() {
            if let Some(category) = event_category(event) {
                encode_string_field(22, category, &mut self.track_event_buf);
            }
        }

        // 30: int64 counter_value = 30;
        if let Some(val) = event.counter_value {
            encode_varint_field(30, val as u64, &mut self.track_event_buf);
        }

        // 47: repeated fixed64 flow_ids = 47;
        for &flow_id in &event.flow_ids {
            encode_fixed64_field(47, flow_id, &mut self.track_event_buf);
        }

        // 48: repeated fixed64 terminating_flow_ids = 48;
        for &term_id in &event.terminating_flow_ids {
            encode_fixed64_field(48, term_id, &mut self.track_event_buf);
        }

        // Common extension fields (4000..4002)
        // 4000: optional uint64 seq = 4000;
        encode_varint_field(4000, event.seq, &mut self.track_event_buf);

        // 4001: optional uint64 cause_seq = 4001;
        if let Some(cause) = event.cause_seq {
            encode_varint_field(4001, cause, &mut self.track_event_buf);
        }

        // 4002: optional string agent = 4002;
        if let Some(ref agent) = event.agent {
            encode_string_field(4002, agent, &mut self.track_event_buf);
        }

        // Specific extension body field (4003..4035)
        if !self.body_buf.is_empty() {
            let ext_field = event.kind.extension_field_number();
            encode_bytes_field(ext_field, &self.body_buf, &mut self.track_event_buf);
        }

        // 3. Build TracePacket
        // 8: optional uint64 timestamp = 8;
        encode_varint_field(8, event.ts_ns, &mut self.packet_buf);

        // 10: optional uint32 trusted_packet_sequence_id = 10;
        let seq_id = event.kind.ring_class().sequence_id();
        encode_varint_field(10, seq_id as u64, &mut self.packet_buf);

        // 13: optional uint32 sequence_flags = 13 (SEQ_NEEDS_INCREMENTAL_STATE = 2);
        encode_varint_field(13, 2, &mut self.packet_buf);

        // 11: optional TrackEvent track_event = 11;
        encode_bytes_field(11, &self.track_event_buf, &mut self.packet_buf);

        // 4. Frame packet into output buffer
        frame_packet(&self.packet_buf, out);
    }
}

/// Serializes the inner oneof message of `tacho::Event` into bytes.
fn encode_event_body(event: &tacho::Event, buf: &mut Vec<u8>) {
    if let Some(ref body) = event.body {
        let res = match body {
            tacho::event::Body::Run(m) => m.encode(buf),
            tacho::event::Body::RunEnded(m) => m.encode(buf),
            tacho::event::Body::Turn(m) => m.encode(buf),
            tacho::event::Body::TurnEnded(m) => m.encode(buf),
            tacho::event::Body::ToolCall(m) => m.encode(buf),
            tacho::event::Body::ToolCallEnded(m) => m.encode(buf),
            tacho::event::Body::Command(m) => m.encode(buf),
            tacho::event::Body::CommandEnded(m) => m.encode(buf),
            tacho::event::Body::FileAccess(m) => m.encode(buf),
            tacho::event::Body::FileAccessEnded(m) => m.encode(buf),
            tacho::event::Body::FileEdited(m) => m.encode(buf),
            tacho::event::Body::Wait(m) => m.encode(buf),
            tacho::event::Body::WaitEnded(m) => m.encode(buf),
            tacho::event::Body::Prompt(m) => m.encode(buf),
            tacho::event::Body::Message(m) => m.encode(buf),
            tacho::event::Body::Compacted(m) => m.encode(buf),
            tacho::event::Body::Stall(m) => m.encode(buf),
            tacho::event::Body::ProviderError(m) => m.encode(buf),
            tacho::event::Body::TaskQueued(m) => m.encode(buf),
            tacho::event::Body::TaskState(m) => m.encode(buf),
            tacho::event::Body::AgentState(m) => m.encode(buf),
            tacho::event::Body::Milestone(m) => m.encode(buf),
            tacho::event::Body::Review(m) => m.encode(buf),
            tacho::event::Body::ReviewEnded(m) => m.encode(buf),
            tacho::event::Body::ReviewFinding(m) => m.encode(buf),
            tacho::event::Body::Snapshot(m) => m.encode(buf),
            tacho::event::Body::SnapshotEnded(m) => m.encode(buf),
            tacho::event::Body::Daemon(m) => m.encode(buf),
            tacho::event::Body::ClockStep(m) => m.encode(buf),
            tacho::event::Body::TrackCreated(m) => m.encode(buf),
            tacho::event::Body::TraceError(m) => m.encode(buf),
            tacho::event::Body::Custom(m) => m.encode(buf),
            tacho::event::Body::CustomEnded(m) => m.encode(buf),
        };
        debug_assert!(res.is_ok(), "encoding proto message should not fail");
    }
}

/// Encodes the snapshot header:
/// 1. `ExtensionDescriptor` with `tacho.proto` descriptor set.
/// 2. `ClockSnapshot` with primary trace clock set to REALTIME.
/// 3. Per ring sequence (0x7AC00001..3): `TracePacketDefaults` + full `InternedData` + `SEQ_INCREMENTAL_STATE_CLEARED`.
/// 4. Every `TrackDescriptor` in `registry` in creation order.
pub fn encode_snapshot_header(
    registry: &TrackRegistry,
    realtime_now_ns: u64,
    boottime_now_ns: Option<u64>,
    out: &mut Vec<u8>,
) {
    let mut packet_buf = Vec::with_capacity(1024);

    // 1. ExtensionDescriptor (TracePacket field 72)
    {
        packet_buf.clear();
        let mut ext_desc_buf = Vec::with_capacity(DESCRIPTOR_SET.len() + 16);
        // ExtensionDescriptor.extension_set = 1;
        encode_bytes_field(1, DESCRIPTOR_SET, &mut ext_desc_buf);
        // TracePacket.extension_descriptor = 72;
        encode_bytes_field(72, &ext_desc_buf, &mut packet_buf);
        frame_packet(&packet_buf, out);
    }

    // 2. ClockSnapshot (TracePacket field 6)
    {
        packet_buf.clear();
        let mut clock_snap_buf = Vec::with_capacity(64);

        // Clock 1: REALTIME (clock_id = 1)
        {
            let mut clk_buf = Vec::with_capacity(16);
            encode_varint_field(1, 1, &mut clk_buf); // clock_id = 1 (REALTIME)
            encode_varint_field(2, realtime_now_ns, &mut clk_buf); // timestamp
            encode_bytes_field(1, &clk_buf, &mut clock_snap_buf);
        }

        // Clock 2: Optional BOOTTIME (clock_id = 6)
        if let Some(boot_ns) = boottime_now_ns {
            let mut clk_buf = Vec::with_capacity(16);
            encode_varint_field(1, 6, &mut clk_buf); // clock_id = 6 (BOOTTIME)
            encode_varint_field(2, boot_ns, &mut clk_buf); // timestamp
            encode_bytes_field(1, &clk_buf, &mut clock_snap_buf);
        }

        // ClockSnapshot.primary_trace_clock = 2 (BuiltinClock::BUILTIN_CLOCK_REALTIME = 1)
        encode_varint_field(2, 1, &mut clock_snap_buf);

        // TracePacket.clock_snapshot = 6
        encode_bytes_field(6, &clock_snap_buf, &mut packet_buf);
        frame_packet(&packet_buf, out);
    }

    // 3. Per ring sequence: TracePacketDefaults + InternedData + SEQ_INCREMENTAL_STATE_CLEARED
    let ring_sequences = [
        RingClass::Core.sequence_id(),
        RingClass::Agents.sequence_id(),
        RingClass::Machine.sequence_id(),
    ];

    // Build the full InternedData event_names block once (identical across rings)
    let mut interned_data_buf = Vec::with_capacity(1024);
    for kind in Kind::ALL {
        let mut name_buf = Vec::with_capacity(32);
        // EventName.iid = 1
        encode_varint_field(1, kind.name_iid(), &mut name_buf);
        // EventName.name = 2
        encode_string_field(2, kind.display_name(), &mut name_buf);
        // InternedData.event_names = 2
        encode_bytes_field(2, &name_buf, &mut interned_data_buf);
    }

    // Build TracePacketDefaults with timestamp_clock_id = 1 (REALTIME)
    let mut defaults_buf = Vec::with_capacity(16);
    // TracePacketDefaults.timestamp_clock_id = 58
    encode_varint_field(58, 1, &mut defaults_buf);

    for &seq_id in &ring_sequences {
        packet_buf.clear();

        // 10: optional uint32 trusted_packet_sequence_id = 10;
        encode_varint_field(10, seq_id as u64, &mut packet_buf);

        // 13: optional uint32 sequence_flags = 13 (SEQ_INCREMENTAL_STATE_CLEARED = 1);
        encode_varint_field(13, 1, &mut packet_buf);

        // 59: optional TracePacketDefaults trace_packet_defaults = 59;
        encode_bytes_field(59, &defaults_buf, &mut packet_buf);

        // 12: optional InternedData interned_data = 12;
        encode_bytes_field(12, &interned_data_buf, &mut packet_buf);

        frame_packet(&packet_buf, out);
    }

    // 4. Every TrackDescriptor in the registry
    for entry in registry.iter() {
        packet_buf.clear();
        let mut desc_buf = Vec::with_capacity(256);
        encode_track_descriptor(entry, &mut desc_buf);
        // TracePacket.track_descriptor = 60
        encode_bytes_field(60, &desc_buf, &mut packet_buf);
        frame_packet(&packet_buf, out);
    }
}

fn event_category(event: &Event) -> Option<&'static str> {
    if event.counter_value.is_some() {
        return Some("counter");
    }
    match event.kind {
        Kind::Run | Kind::RunEnded => Some("run"),
        Kind::Turn | Kind::TurnEnded => Some("turn"),
        Kind::Command | Kind::CommandEnded => {
            if let Some(tacho::event::Body::Command(c)) = &event.body.body {
                match c.class {
                    Some(2) => Some("build"),
                    Some(3) => Some("test"),
                    Some(7) => Some("commit"),
                    Some(8) => Some("push"),
                    Some(9) => Some("fetch"),
                    Some(10) => Some("rebase"),
                    _ => Some("build"),
                }
            } else if let Some(custom) = &event.custom_name {
                let s = custom.as_str();
                if s.starts_with("Build") {
                    Some("build")
                } else if s.starts_with("Test") {
                    Some("test")
                } else if s.starts_with("Commit") {
                    Some("commit")
                } else if s.starts_with("Push") {
                    Some("push")
                } else if s.starts_with("Fetch") {
                    Some("fetch")
                } else if s.starts_with("Rebase") {
                    Some("rebase")
                } else {
                    Some("build")
                }
            } else {
                Some("build")
            }
        }
        Kind::ToolCall | Kind::ToolCallEnded => {
            if let Some(custom) = &event.custom_name {
                let s = custom.as_str();
                if s.starts_with("Edit") {
                    Some("edit")
                } else if s.starts_with("Read") {
                    Some("read")
                } else if s.starts_with("Search") {
                    Some("search")
                } else {
                    Some("edit")
                }
            } else {
                Some("edit")
            }
        }
        Kind::FileAccess | Kind::FileAccessEnded => {
            if let Some(tacho::event::Body::FileAccess(f)) = &event.body.body {
                if f.kind == Some(tacho::FileAccessKind::Search as i32) {
                    Some("search")
                } else {
                    Some("read")
                }
            } else {
                Some("read")
            }
        }
        Kind::FileEdited => Some("edit"),
        Kind::Wait | Kind::WaitEnded => Some("wait"),
        Kind::Stall => Some("stall"),
        Kind::TaskQueued | Kind::TaskState => Some("task"),
        Kind::AgentState => Some("state"),
        Kind::Milestone => Some("milestone"),
        Kind::Prompt => Some("prompt"),
        Kind::Message => Some("message"),
        Kind::Review | Kind::ReviewEnded | Kind::ReviewFinding => Some("review"),
        Kind::Custom | Kind::CustomEnded => {
            if event.track_key.ends_with("/procs") {
                Some("proc")
            } else {
                Some("custom")
            }
        }
        _ => None,
    }
}
