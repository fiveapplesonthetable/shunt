//! In-memory event representation before wire encoding.

use crate::kind::Kind;
use tacho_proto::tacho;

/// A fully typed in-memory flight recorder event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Log sequence number, unique per daemon install.
    pub seq: u64,
    /// Event timestamp in nanoseconds (CLOCK_REALTIME).
    pub ts_ns: u64,
    /// Event kind from the closed set of 33 kinds.
    pub kind: Kind,
    /// Textual track key where the event is emitted (e.g. "agent:worker-1/run:1/turns").
    pub track_key: String,
    /// Causal origin event sequence number, if known.
    pub cause_seq: Option<u64>,
    /// Agent identifier string, denormalized for direct SQL queries without joins.
    pub agent: Option<String>,
    /// Typed protobuf body oneof.
    pub body: tacho::Event,
    /// Explicit outgoing flow IDs (originating or passing through this event).
    pub flow_ids: Vec<u64>,
    /// Explicit incoming terminating flow IDs (ending at this event).
    pub terminating_flow_ids: Vec<u64>,
    /// Counter value for counter tracks.
    pub counter_value: Option<i64>,
    /// Dynamic name for custom spans or instants (open set).
    pub custom_name: Option<String>,
}

impl Event {
    /// Constructs a new Event with default empty flow IDs and no counter value.
    pub fn new(
        seq: u64,
        ts_ns: u64,
        kind: Kind,
        track_key: impl Into<String>,
        cause_seq: Option<u64>,
        agent: Option<String>,
        body: tacho::Event,
    ) -> Self {
        Self {
            seq,
            ts_ns,
            kind,
            track_key: track_key.into(),
            cause_seq,
            agent,
            body,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            custom_name: None,
        }
    }

    /// Attaches an outgoing flow ID to this event.
    pub fn with_flow(mut self, flow_id: u64) -> Self {
        self.flow_ids.push(flow_id);
        self
    }

    /// Attaches multiple outgoing flow IDs to this event.
    pub fn with_flows(mut self, ids: impl IntoIterator<Item = u64>) -> Self {
        self.flow_ids.extend(ids);
        self
    }

    /// Attaches a terminating flow ID to this event.
    pub fn with_terminating_flow(mut self, flow_id: u64) -> Self {
        self.terminating_flow_ids.push(flow_id);
        self
    }

    /// Attaches multiple terminating flow IDs to this event.
    pub fn with_terminating_flows(mut self, ids: impl IntoIterator<Item = u64>) -> Self {
        self.terminating_flow_ids.extend(ids);
        self
    }

    /// Attaches a counter value to this event.
    pub fn with_counter(mut self, val: i64) -> Self {
        self.counter_value = Some(val);
        self
    }

    /// Sets an inline custom name for custom spans or instants.
    pub fn with_custom_name(mut self, name: impl Into<String>) -> Self {
        self.custom_name = Some(name.into());
        self
    }
}
