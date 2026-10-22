//! Lane allocator for overlapping concurrent slices on an agent run.
//!
//! Perfetto requires duration slices on a track to be properly nested (LIFO callstack).
//! When agents issue parallel or overlapping tool calls, the lane allocator assigns each
//! tool-level slice to:
//! 1. `turns` if inside the currently open Turn and no sibling tool slice is open;
//! 2. lowest available `lane:k` with nothing open;
//! 3. a newly allocated `lane:k+1`.
//!
//! Slices ended without a matching begin produce an `UnmatchedEnd` error (which the daemon
//! translates into an instant event, never an unbalanced end). Slices remaining open at
//! run termination are cleanly closed with `abandoned = true`.

use crate::kind::Kind;
use std::collections::HashMap;

/// Opaque identifier for a slice (e.g. sequence number, tool_use_id, or caller handle).
pub type SliceId = u64;

/// Target track assigned to an open slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignedTrack {
    /// Placed on the base run track (e.g. `agent:id/run:1`).
    Run,
    /// Placed on the `turns` track as the Turn slice itself.
    TurnsTurn,
    /// Placed on the `turns` track as a tool slice nested inside Turn.
    TurnsTool,
    /// Placed on an allocated `lane:k` track (0-indexed internally).
    Lane(usize),
    /// Placed on a dedicated track (e.g. `waits`, `custom`).
    Dedicated(String),
}

/// Errors produced during lane allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneError {
    /// An end event arrived for a slice whose begin is unknown.
    UnmatchedEnd { slice_id: SliceId, kind: Kind },
}

/// A slice that remained open when the run ended and was forced closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedSlice {
    pub slice_id: SliceId,
    pub kind: Kind,
    pub track_key: String,
}

/// Pure state machine managing lane assignments for one run.
#[derive(Debug, Clone)]
pub struct LaneAllocator {
    run_track: String,
    turns_track: String,
    waits_track: String,
    run_open: Option<SliceId>,
    turn_open: Option<SliceId>,
    turns_tool_open: Option<SliceId>,
    lanes: Vec<Option<SliceId>>,
    open_slices: HashMap<SliceId, (Kind, AssignedTrack)>,
}

impl LaneAllocator {
    /// Creates a new lane allocator for a run track (e.g. "agent:worker-1/run:1").
    pub fn new(run_track: impl Into<String>) -> Self {
        let run_track = run_track.into();
        let turns_track = format!("{}/turns", run_track);
        let waits_track = format!("{}/waits", run_track);
        Self {
            run_track,
            turns_track,
            waits_track,
            run_open: None,
            turn_open: None,
            turns_tool_open: None,
            lanes: Vec::new(),
            open_slices: HashMap::new(),
        }
    }

    /// Returns the track key string corresponding to an assigned track.
    fn track_key_for(&self, assigned: &AssignedTrack) -> String {
        match assigned {
            AssignedTrack::Run => self.run_track.clone(),
            AssignedTrack::TurnsTurn | AssignedTrack::TurnsTool => self.turns_track.clone(),
            AssignedTrack::Lane(i) => format!("{}/lane:{}", self.run_track, i + 1),
            AssignedTrack::Dedicated(t) => t.clone(),
        }
    }

    /// Assigns a new begin slice to an appropriate track ensuring proper nesting.
    pub fn allocate_begin(&mut self, slice_id: SliceId, kind: Kind) -> String {
        if let Some((_, assigned)) = self.open_slices.get(&slice_id) {
            return self.track_key_for(assigned);
        }

        let (assigned, track_key) = match kind {
            Kind::Run => {
                self.run_open = Some(slice_id);
                (AssignedTrack::Run, self.run_track.clone())
            }
            Kind::Turn => {
                self.turn_open = Some(slice_id);
                (AssignedTrack::TurnsTurn, self.turns_track.clone())
            }
            Kind::Wait => (
                AssignedTrack::Dedicated(self.waits_track.clone()),
                self.waits_track.clone(),
            ),
            _ => {
                // Tool-level slices: ToolCall, Command, FileAccess, Custom, Review, Snapshot
                if self.turn_open.is_some() && self.turns_tool_open.is_none() {
                    // Rule 1: inside open Turn and no sibling tool open on `turns`
                    self.turns_tool_open = Some(slice_id);
                    (AssignedTrack::TurnsTool, self.turns_track.clone())
                } else {
                    // Rule 2 & 3: Find lowest available lane or allocate new
                    let mut lane_idx = None;
                    for (i, slot) in self.lanes.iter().enumerate() {
                        if slot.is_none() {
                            lane_idx = Some(i);
                            break;
                        }
                    }

                    let idx = match lane_idx {
                        Some(i) => {
                            self.lanes[i] = Some(slice_id);
                            i
                        }
                        None => {
                            let i = self.lanes.len();
                            self.lanes.push(Some(slice_id));
                            i
                        }
                    };

                    let lane_key = format!("{}/lane:{}", self.run_track, idx + 1);
                    (AssignedTrack::Lane(idx), lane_key)
                }
            }
        };

        self.open_slices.insert(slice_id, (kind, assigned));
        track_key
    }

    /// Releases an open slice and returns its assigned track key.
    /// Returns `Err(LaneError::UnmatchedEnd)` if the slice begin was never seen.
    pub fn allocate_end(&mut self, slice_id: SliceId, kind: Kind) -> Result<String, LaneError> {
        let (_begin_kind, assigned) = match self.open_slices.remove(&slice_id) {
            Some(v) => v,
            None => return Err(LaneError::UnmatchedEnd { slice_id, kind }),
        };

        let track_key = match assigned {
            AssignedTrack::Run => {
                self.run_open = None;
                self.run_track.clone()
            }
            AssignedTrack::TurnsTurn => {
                self.turn_open = None;
                self.turns_track.clone()
            }
            AssignedTrack::TurnsTool => {
                self.turns_tool_open = None;
                self.turns_track.clone()
            }
            AssignedTrack::Lane(idx) => {
                if idx < self.lanes.len() {
                    self.lanes[idx] = None;
                }
                format!("{}/lane:{}", self.run_track, idx + 1)
            }
            AssignedTrack::Dedicated(track) => track,
        };

        Ok(track_key)
    }

    /// Drains any open tool slice on the turns track before Turn ends.
    pub fn drain_turns_tool(&mut self) -> Option<AbandonedSlice> {
        if let Some(tool_id) = self.turns_tool_open.take() {
            if let Some((kind, _)) = self.open_slices.remove(&tool_id) {
                return Some(AbandonedSlice {
                    slice_id: tool_id,
                    kind,
                    track_key: self.turns_track.clone(),
                });
            }
        }
        None
    }

    /// Closes all remaining open slices on run termination with `abandoned = true`.
    pub fn drain_abandoned(&mut self) -> Vec<AbandonedSlice> {
        let mut abandoned = Vec::new();

        // 1. Tool slices on `turns`
        if let Some(tool_id) = self.turns_tool_open.take() {
            if let Some((kind, _)) = self.open_slices.remove(&tool_id) {
                abandoned.push(AbandonedSlice {
                    slice_id: tool_id,
                    kind,
                    track_key: self.turns_track.clone(),
                });
            }
        }

        // 2. Open Turn on `turns`
        if let Some(turn_id) = self.turn_open.take() {
            if let Some((kind, _)) = self.open_slices.remove(&turn_id) {
                abandoned.push(AbandonedSlice {
                    slice_id: turn_id,
                    kind,
                    track_key: self.turns_track.clone(),
                });
            }
        }

        // 3. Open slices in lanes
        for (i, slot) in self.lanes.iter_mut().enumerate() {
            if let Some(slice_id) = slot.take() {
                if let Some((kind, _)) = self.open_slices.remove(&slice_id) {
                    abandoned.push(AbandonedSlice {
                        slice_id,
                        kind,
                        track_key: format!("{}/lane:{}", self.run_track, i + 1),
                    });
                }
            }
        }

        // 4. Remaining open slices (waits, dedicated), sorted LIFO (newest first)
        let mut remaining: Vec<_> = self.open_slices.drain().collect();
        remaining.sort_by_key(|&(id, _)| std::cmp::Reverse(id));
        for (slice_id, (kind, assigned)) in remaining {
            let track_key = match assigned {
                AssignedTrack::Run => self.run_track.clone(),
                AssignedTrack::TurnsTurn | AssignedTrack::TurnsTool => self.turns_track.clone(),
                AssignedTrack::Lane(i) => format!("{}/lane:{}", self.run_track, i + 1),
                AssignedTrack::Dedicated(t) => t,
            };
            abandoned.push(AbandonedSlice {
                slice_id,
                kind,
                track_key,
            });
        }

        abandoned
    }

    /// Returns the number of currently open slices.
    pub fn open_count(&self) -> usize {
        self.open_slices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_lane_allocation_rules() {
        let mut allocator = LaneAllocator::new("agent:1/run:1");

        // 1. Begin turn
        let t1 = allocator.allocate_begin(1, Kind::Turn);
        assert_eq!(t1, "agent:1/run:1/turns");

        // 2. First tool inside turn -> placed on `turns`
        let c1 = allocator.allocate_begin(2, Kind::Command);
        assert_eq!(c1, "agent:1/run:1/turns");

        // 3. Overlapping second tool -> placed on `lane:1`
        let c2 = allocator.allocate_begin(3, Kind::ToolCall);
        assert_eq!(c2, "agent:1/run:1/lane:1");

        // 4. Overlapping third tool -> placed on `lane:2`
        let c3 = allocator.allocate_begin(4, Kind::ToolCall);
        assert_eq!(c3, "agent:1/run:1/lane:2");

        // 5. End second tool (frees lane:1)
        let end_c2 = allocator.allocate_end(3, Kind::ToolCallEnded).unwrap();
        assert_eq!(end_c2, "agent:1/run:1/lane:1");

        // 6. Fourth tool reuses lowest free lane (lane:1)
        let c4 = allocator.allocate_begin(5, Kind::ToolCall);
        assert_eq!(c4, "agent:1/run:1/lane:1");

        // 7. Unmatched end returns error
        let err = allocator.allocate_end(999, Kind::ToolCallEnded);
        assert!(matches!(err, Err(LaneError::UnmatchedEnd { .. })));

        // 8. Drain abandoned closes remaining open slices
        let abandoned = allocator.drain_abandoned();
        assert_eq!(abandoned.len(), 4); // turn(1), command(2), tool(4), tool(5)
        assert_eq!(allocator.open_count(), 0);
    }

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Action {
        Begin(SliceId, Kind),
        End(SliceId, Kind),
    }

    fn tool_kind_strategy() -> impl Strategy<Value = Kind> {
        prop_oneof![
            Just(Kind::ToolCall),
            Just(Kind::Command),
            Just(Kind::FileAccess),
            Just(Kind::Custom),
        ]
    }

    proptest! {
        #[test]
        fn proptest_always_properly_nested(
            actions in prop::collection::vec(
                prop_oneof![
                    (1u64..30, tool_kind_strategy()).prop_map(|(id, k)| Action::Begin(id, k)),
                    (1u64..30, tool_kind_strategy()).prop_map(|(id, k)| Action::End(id, k)),
                ],
                1..60
            )
        ) {
            let mut allocator = LaneAllocator::new("agent:test/run:1");
            // Track key -> stack of currently open SliceIds on that track
            let mut track_stacks: HashMap<String, Vec<SliceId>> = HashMap::new();

            // Simulate turn slice being open
            let turn_track = allocator.allocate_begin(1000, Kind::Turn);
            track_stacks.entry(turn_track).or_default().push(1000);

            for action in actions {
                match action {
                    Action::Begin(id, kind) => {
                        let is_new = !allocator.open_slices.contains_key(&id);
                        let track = allocator.allocate_begin(id, kind);
                        if is_new {
                            track_stacks.entry(track).or_default().push(id);
                        }
                    }
                    Action::End(id, kind) => {
                        if let Ok(track) = allocator.allocate_end(id, kind) {
                            let stack = track_stacks.get_mut(&track).unwrap();
                            let popped = stack.pop();
                            prop_assert_eq!(popped, Some(id), "End must match top of stack for track {}", track);
                        }
                    }
                }
            }

            // Drain remaining open slices
            let abandoned = allocator.drain_abandoned();
            for s in abandoned {
                let stack = track_stacks.get_mut(&s.track_key).unwrap();
                let popped = stack.pop();
                prop_assert_eq!(popped, Some(s.slice_id), "Drained abandoned slice must match top of stack");
            }
        }
    }
}
