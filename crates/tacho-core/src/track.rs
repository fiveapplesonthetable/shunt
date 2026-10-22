//! Track hierarchy and registry.
//!
//! Every track in tachograph has a unique textual key (e.g. "agent:id/run:1/turns")
//! and a 64-bit integer UUID derived via `xxh3_64` with the highest bit cleared.
//! Snapshots serialize `TrackDescriptor` protos with `child_ordering = EXPLICIT`
//! and ranks assigned in creation order.

use std::collections::HashMap;

use crate::wire::{encode_bytes_field, encode_string_field, encode_varint_field};

/// Computes the 63-bit positive UUID for a track key using XXH3-64.
/// The top bit is cleared so the result fits comfortably into signed 64-bit SQL types.
#[inline]
pub fn compute_uuid(key: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(key.as_bytes()) & 0x7FFF_FFFF_FFFF_FFFF
}

/// The visual and structural category of a track in Perfetto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    /// Organizational group / parent track with no direct events.
    Group,
    /// Standard slice track supporting nested duration events.
    Slices,
    /// Counter track recording numerical samples over time.
    Counter,
    /// State track displaying discrete symbolic states over time.
    State,
}

/// Computes the deterministic sibling order rank based on track role and name.
/// - state = 1
/// - turns = 1
/// - run:N = 10 + N
/// - lane:K = 10 + K
/// - overflow = 50
/// - waits = 60
/// - tokens_in = 100
/// - tokens_out = 101
/// - tokens_cached = 102
/// - cost = 103
/// - cpu = 110
/// - rss = 111
pub fn compute_track_rank(key: &str, name: &str) -> i32 {
    if name == "state" || key.ends_with("/state") {
        return 1;
    }
    if name == "turns" || key.ends_with("/turns") {
        return 1;
    }
    let target_name = if name.is_empty() || name == key {
        key.rsplit('/').next().unwrap_or(name)
    } else {
        name
    };

    if let Some(lane_str) = target_name.strip_prefix("lane:") {
        if let Ok(k) = lane_str.parse::<i32>() {
            return 10 + k;
        }
    }
    if let Some(run_str) = target_name.strip_prefix("run:") {
        if let Ok(n) = run_str.parse::<i32>() {
            return 10 + n;
        }
    }
    match target_name {
        "procs" => 40,
        "overflow" => 50,
        "waits" => 60,
        "tokens_in" => 100,
        "tokens_out" => 101,
        "tokens_cached" => 102,
        "cost" => 103,
        "cpu" => 110,
        "rss" => 111,
        "proc_count" => 112,
        "daemon" => 1,
        "agents" => 2,
        "tasks" => 3,
        "machine" => 4,
        "providers" => 5,
        "custom" => 200,
        _ => 500,
    }
}

/// Metadata describing a single track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEntry {
    /// Canonical textual key for this track.
    pub key: String,
    /// Unique 63-bit integer identifier.
    pub uuid: u64,
    /// Optional UUID of the parent track.
    pub parent_uuid: Option<u64>,
    /// Optional key of the parent track.
    pub parent_key: Option<String>,
    /// User-visible name displayed in ui.perfetto.dev.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Track visual category.
    pub kind: TrackKind,
    /// Sibling display rank (assigned in creation order).
    pub rank: i32,
    /// Optional unit name for counter tracks (e.g. "bytes", "tokens").
    pub counter_unit: Option<String>,
    /// Whether a hash collision occurred when registering this track key.
    pub collision: bool,
}

/// In-memory registry of all tracks known to the flight recorder.
#[derive(Debug, Default, Clone)]
pub struct TrackRegistry {
    tracks: Vec<TrackEntry>,
    key_to_idx: HashMap<String, usize>,
    uuid_to_idx: HashMap<u64, usize>,
}

impl TrackRegistry {
    /// Creates a new empty track registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new track or returns the existing entry if already registered.
    /// Computes deterministic sibling order rank based on track role.
    pub fn register(
        &mut self,
        key: &str,
        parent_key: Option<&str>,
        name: &str,
        description: Option<&str>,
        kind: TrackKind,
        counter_unit: Option<&str>,
    ) -> &TrackEntry {
        let rank = compute_track_rank(key, name);
        self.register_with_rank(key, parent_key, name, description, kind, counter_unit, rank)
    }

    /// Registers a new track with an explicit sibling order rank.
    #[allow(clippy::too_many_arguments)]
    pub fn register_with_rank(
        &mut self,
        key: &str,
        parent_key: Option<&str>,
        name: &str,
        description: Option<&str>,
        kind: TrackKind,
        counter_unit: Option<&str>,
        rank: i32,
    ) -> &TrackEntry {
        if let Some(&idx) = self.key_to_idx.get(key) {
            return &self.tracks[idx];
        }

        let parent_uuid =
            parent_key.and_then(|pk| self.key_to_idx.get(pk).map(|&i| self.tracks[i].uuid));

        // Compute UUID with collision resolution rule: `key#2`, `key#3`, ...
        let mut candidate_key = key.to_string();
        let mut suffix = 2;
        let mut uuid = compute_uuid(&candidate_key);
        let mut collision = false;

        while let Some(&existing_idx) = self.uuid_to_idx.get(&uuid) {
            if self.tracks[existing_idx].key == key {
                break;
            }
            collision = true;
            candidate_key = format!("{}#{}", key, suffix);
            suffix += 1;
            uuid = compute_uuid(&candidate_key);
        }

        let entry = TrackEntry {
            key: key.to_string(),
            uuid,
            parent_uuid,
            parent_key: parent_key.map(|s| s.to_string()),
            name: name.to_string(),
            description: description.map(|s| s.to_string()),
            kind,
            rank,
            counter_unit: counter_unit.map(|s| s.to_string()),
            collision,
        };

        let idx = self.tracks.len();
        self.tracks.push(entry);
        self.key_to_idx.insert(key.to_string(), idx);
        self.uuid_to_idx.insert(uuid, idx);

        &self.tracks[idx]
    }

    /// Finds a track by textual key.
    pub fn get_by_key(&self, key: &str) -> Option<&TrackEntry> {
        self.key_to_idx.get(key).map(|&idx| &self.tracks[idx])
    }

    /// Finds a track by textual key mutably.
    pub fn get_by_key_mut(&mut self, key: &str) -> Option<&mut TrackEntry> {
        let idx = *self.key_to_idx.get(key)?;
        Some(&mut self.tracks[idx])
    }

    /// Finds a track by UUID.
    pub fn get_by_uuid(&self, uuid: u64) -> Option<&TrackEntry> {
        self.uuid_to_idx.get(&uuid).map(|&idx| &self.tracks[idx])
    }

    /// Iterates over all registered tracks in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &TrackEntry> {
        self.tracks.iter()
    }

    /// Returns the number of registered tracks.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Returns true if no tracks are registered.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
}

/// Encodes a `TrackDescriptor` proto message into the given buffer.
pub fn encode_track_descriptor(entry: &TrackEntry, buf: &mut Vec<u8>) {
    // 1: optional uint64 uuid = 1;
    encode_varint_field(1, entry.uuid, buf);

    // 2: optional string name = 2;
    encode_string_field(2, &entry.name, buf);

    // 5: optional uint64 parent_uuid = 5;
    if let Some(parent_uuid) = entry.parent_uuid {
        encode_varint_field(5, parent_uuid, buf);
    }

    // 8: optional CounterDescriptor counter = 8;
    if entry.kind == TrackKind::Counter {
        let mut counter_buf = Vec::new();
        if let Some(ref unit) = entry.counter_unit {
            // CounterDescriptor.unit_name = 6
            encode_string_field(6, unit, &mut counter_buf);
        }
        encode_bytes_field(8, &counter_buf, buf);
    }

    // 9: optional bool disallow_merging_with_system_tracks = 9;
    encode_varint_field(9, 1, buf);

    // 11: optional ChildTracksOrdering child_ordering = 11 (EXPLICIT = 3);
    encode_varint_field(11, 3, buf);

    // 12: optional int32 sibling_order_rank = 12;
    encode_varint_field(12, entry.rank as u64, buf);

    // 14: optional string description = 14;
    if let Some(ref desc) = entry.description {
        encode_string_field(14, desc, buf);
    }

    // 18: optional StateDescriptor state = 18;
    if entry.kind == TrackKind::State {
        // StateDescriptor is an empty message
        encode_bytes_field(18, &[], buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_ranks() {
        assert_eq!(compute_track_rank("agent:worker-1/state", "state"), 1);
        assert_eq!(compute_track_rank("agent:worker-1/run:1", "run:1"), 11);
        assert_eq!(compute_track_rank("agent:worker-1/run:2", "run:2"), 12);
        assert_eq!(compute_track_rank("agent:worker-1/run:1/turns", "turns"), 1);
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/lane:0", "lane:0"),
            10
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/lane:1", "lane:1"),
            11
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/overflow", "overflow"),
            50
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/waits", "waits"),
            60
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/tokens_in", "tokens_in"),
            100
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/tokens_out", "tokens_out"),
            101
        );
        assert_eq!(
            compute_track_rank("agent:worker-1/run:1/tokens_cached", "tokens_cached"),
            102
        );
        assert_eq!(compute_track_rank("agent:worker-1/run:1/cost", "cost"), 103);
        assert_eq!(compute_track_rank("agent:worker-1/run:1/cpu", "cpu"), 110);
        assert_eq!(compute_track_rank("agent:worker-1/run:1/rss", "rss"), 111);
    }
}
