//! Snapshot-time projection of logged events into human-friendly Perfetto slices.
//!
//! The log stores some facts as instants or states that read better as spans on a
//! timeline: a stall lasts until the agent recovers, a message sits in an inbox until it
//! is delivered, a worktree stays at a commit until HEAD moves. The projector turns those
//! into begin/end pairs on dedicated tracks, adds counters derived from events (files
//! edited, commits), and links a message to the activity it caused with a flow arrow.
//! It is deterministic: the same rows always produce the same bytes.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use tacho_core::{Encoder, Event, Kind, TrackEventType, TrackRegistry};
use tacho_proto::tacho;

/// Flow ordinal for message → recipient's next activity. Core ordinals are all < 16, and
/// flow ids are `seq * 256 + ordinal`, so this cannot collide with recorder flows.
const FLOW_MESSAGE_DELIVERY: u64 = 200;

/// Minimum duration of a synthesized slice, so zero-length spans stay visible and clickable.
const MIN_SYNTH_SLICE_NS: u64 = 1_000;

/// Encoder plus output stream, counting bytes as packets are written.
pub(super) struct Sink<'a, W: Write> {
    encoder: Encoder,
    registry: &'a TrackRegistry,
    buf: Vec<u8>,
    writer: W,
    pub(super) bytes: u64,
}

impl<'a, W: Write> Sink<'a, W> {
    pub(super) fn new(registry: &'a TrackRegistry, writer: W, header_bytes: u64) -> Self {
        Self {
            encoder: Encoder::new(),
            registry,
            buf: Vec::with_capacity(2048),
            writer,
            bytes: header_bytes,
        }
    }

    fn emit(&mut self, ev: &Event, ty: Option<TrackEventType>) -> io::Result<()> {
        self.buf.clear();
        self.encoder
            .encode_event_with_type(ev, ty, self.registry, &mut self.buf);
        self.writer.write_all(&self.buf)?;
        self.bytes += self.buf.len() as u64;
        Ok(())
    }

    pub(super) fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// `agent:<id>/run:<n>` prefix of a run-scoped track key.
pub(super) fn run_prefix_of(track_key: &str) -> Option<String> {
    let mut parts = track_key.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if first.starts_with("agent:") && second.starts_with("run:") {
        Some(format!("{}/{}", first, second))
    } else {
        None
    }
}

/// Agent id of an event: its `agent` field, else the `agent:<id>` head of its track key.
fn agent_of(ev: &Event) -> String {
    ev.agent.clone().unwrap_or_else(|| {
        ev.track_key
            .split('/')
            .next()
            .and_then(|head| head.strip_prefix("agent:"))
            .unwrap_or("")
            .to_string()
    })
}

/// Track that per-task state lives on (`task:<slug>/state`).
pub(super) fn task_state_track(track_key: &str) -> String {
    if track_key.ends_with("/state") {
        track_key.to_string()
    } else {
        format!("{}/state", track_key)
    }
}

/// Track that an agent's stalls are drawn on (`agent:<id>/stalls`).
fn stall_track(track_key: &str, agent_id: &str) -> String {
    if track_key.ends_with("/stalls") {
        track_key.to_string()
    } else {
        format!("agent:{}/stalls", agent_id)
    }
}

/// Destination track for messages logged on the root `tacho` track.
pub(super) const MESSAGES_TRACK: &str = "tacho/messages";

#[derive(Debug, Clone)]
struct OpenSlice {
    track_key: String,
    kind: Kind,
    open_ts: u64,
    open_seq: u64,
    agent: Option<String>,
    target_agent: Option<String>,
    waiting_for_delivery: bool,
}

/// Streaming state machine; feed events in `seq` order, then call [`SnapshotProjector::finish`].
#[derive(Default)]
pub(super) struct SnapshotProjector {
    /// Last timestamp emitted per synthesized track; slices on one track must not go back.
    track_last_ts: HashMap<String, u64>,
    open_daemon: Option<OpenSlice>,
    open_stalls: BTreeMap<String, Vec<OpenSlice>>,
    open_worktree: BTreeMap<String, OpenSlice>,
    open_reviews: BTreeMap<String, Vec<OpenSlice>>,
    open_message: Option<OpenSlice>,
    open_raw_slices: BTreeMap<String, Vec<OpenSlice>>,
    /// Message flows waiting for the recipient's next slice, keyed by recipient agent.
    pending_deliveries: BTreeMap<String, Vec<u64>>,
    run_files_edited: HashMap<String, i64>,
    run_commits: HashMap<String, i64>,
    max_ts: u64,
    pub(super) events: u64,
}

impl SnapshotProjector {
    fn next_track_ts(&mut self, track: &str, candidate_ts: u64) -> u64 {
        let last = self.track_last_ts.entry(track.to_string()).or_insert(0);
        let ts = (*last).max(candidate_ts);
        *last = ts;
        ts
    }

    /// Ends a synthesized slice at `candidate_ts` (at least [`MIN_SYNTH_SLICE_NS`] long).
    fn end_slice<W: Write>(
        &mut self,
        slice: &OpenSlice,
        candidate_ts: u64,
        out: &mut Sink<'_, W>,
    ) -> io::Result<u64> {
        let min_end = slice.open_ts.saturating_add(MIN_SYNTH_SLICE_NS);
        let end_ts = self.next_track_ts(&slice.track_key, candidate_ts.max(min_end));
        let end_ev = Event::new(
            slice.open_seq,
            end_ts,
            slice.kind,
            &slice.track_key,
            Some(slice.open_seq),
            slice.agent.clone(),
            tacho::Event { body: None },
        );
        out.emit(&end_ev, Some(TrackEventType::SliceEnd))?;
        self.max_ts = self.max_ts.max(end_ts);
        Ok(end_ts)
    }

    fn end_all<W: Write>(
        &mut self,
        mut slices: Vec<OpenSlice>,
        ts: u64,
        out: &mut Sink<'_, W>,
    ) -> io::Result<u64> {
        let mut end_ts = ts;
        while let Some(s) = slices.pop() {
            end_ts = self.end_slice(&s, ts, out)?;
        }
        Ok(end_ts)
    }

    /// Begins a synthesized slice: re-homes `ev` onto `track`, emits it, returns its state.
    fn begin_slice<W: Write>(
        &mut self,
        mut ev: Event,
        track: String,
        out: &mut Sink<'_, W>,
    ) -> io::Result<OpenSlice> {
        let begin_ts = self.next_track_ts(&track, ev.ts_ns);
        ev.ts_ns = begin_ts;
        ev.track_key = track.clone();
        out.emit(&ev, Some(TrackEventType::SliceBegin))?;
        Ok(OpenSlice {
            track_key: track,
            kind: ev.kind,
            open_ts: begin_ts,
            open_seq: ev.seq,
            agent: ev.agent,
            target_agent: None,
            waiting_for_delivery: false,
        })
    }

    /// Emits a derived counter sample (e.g. `files_edited`) at `ts` on `track`.
    fn emit_counter<W: Write>(
        &mut self,
        src: &Event,
        track: String,
        value: i64,
        unit: &str,
        out: &mut Sink<'_, W>,
    ) -> io::Result<()> {
        let ts = self.next_track_ts(&track, src.ts_ns);
        let ev = Event::new(
            src.seq,
            ts,
            Kind::Custom,
            track,
            None,
            src.agent.clone(),
            tacho::Event { body: None },
        )
        .with_counter(value)
        .with_custom_name(unit);
        out.emit(&ev, None)
    }

    /// Projects one event. Events outside the requested window only advance no state.
    pub(super) fn process_event<W: Write>(
        &mut self,
        ev: Event,
        in_window: bool,
        out: &mut Sink<'_, W>,
    ) -> io::Result<()> {
        if !in_window {
            return Ok(());
        }
        self.events += 1;
        self.max_ts = self.max_ts.max(ev.ts_ns);

        if ev.kind != Kind::Message {
            self.close_message_on_recipient_activity(&ev, out)?;
        }

        match ev.kind {
            Kind::TrackCreated | Kind::Prompt => Ok(()),
            Kind::FileEdited => self.on_file_edited(ev, out),
            Kind::Milestone if ev.track_key.ends_with("/turns") => self.on_turn_milestone(ev, out),
            Kind::TaskQueued => self.on_task_queued(ev, out),
            Kind::TaskState => self.on_task_state(ev, out),
            Kind::Stall => self.on_stall(ev, out),
            Kind::AgentState => self.on_agent_state(ev, out),
            Kind::RunEnded => self.on_run_ended(ev, out),
            Kind::Daemon => self.on_daemon(ev, out),
            Kind::Message => self.on_message(ev, out),
            Kind::ReviewFinding => {
                let track = ev.track_key.clone();
                let open = self.begin_slice(ev, track.clone(), out)?;
                self.open_reviews.entry(track).or_default().push(open);
                Ok(())
            }
            Kind::ReviewEnded => {
                let mut ev = ev;
                let findings = self.open_reviews.remove(&ev.track_key).unwrap_or_default();
                let end_ts = self.end_all(findings, ev.ts_ns, out)?;
                if !self.record_raw_slice(&ev) {
                    return Ok(());
                }
                ev.ts_ns = self.next_track_ts(&ev.track_key, ev.ts_ns.max(end_ts));
                out.emit(&ev, None)
            }
            _ => {
                let mut ev = ev;
                if !self.record_raw_slice(&ev) {
                    return Ok(());
                }
                ev.ts_ns = self.next_track_ts(&ev.track_key, ev.ts_ns);
                self.attach_pending_delivery(&mut ev);
                out.emit(&ev, None)
            }
        }
    }

    /// A one-shot message (no explicit delivery) ends when its recipient next does anything.
    fn close_message_on_recipient_activity<W: Write>(
        &mut self,
        ev: &Event,
        out: &mut Sink<'_, W>,
    ) -> io::Result<()> {
        let Some(aid) = ev.agent.as_deref() else {
            return Ok(());
        };
        let should_close = self
            .open_message
            .as_ref()
            .is_some_and(|m| !m.waiting_for_delivery && m.target_agent.as_deref() == Some(aid));
        if should_close {
            if let Some(msg) = self.open_message.take() {
                self.end_slice(&msg, ev.ts_ns, out)?;
            }
        }
        Ok(())
    }

    /// Terminates pending message flows on the recipient's next slice begin, so the UI
    /// draws an arrow from the message to the turn or command it triggered.
    fn attach_pending_delivery(&mut self, ev: &mut Event) {
        if ev.kind.track_event_type() != TrackEventType::SliceBegin {
            return;
        }
        let Some(agent) = ev.agent.as_deref() else {
            return;
        };
        if let Some(flows) = self.pending_deliveries.remove(agent) {
            ev.terminating_flow_ids.extend(flows);
        }
    }

    fn on_file_edited<W: Write>(&mut self, ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let Some(run_prefix) = run_prefix_of(&ev.track_key) else {
            return Ok(());
        };
        let count = self.run_files_edited.entry(run_prefix.clone()).or_insert(0);
        *count += 1;
        let value = *count;
        self.emit_counter(
            &ev,
            format!("{}/files_edited", run_prefix),
            value,
            "files",
            out,
        )
    }

    /// Worktree commits and HEAD moves become a `worktree` slice per HEAD position, plus a
    /// `commits` counter.
    fn on_turn_milestone<W: Write>(&mut self, ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let name = ev.custom_name.as_deref().unwrap_or("");
        let is_commit = name.starts_with("WorktreeCommit");
        if !is_commit && !name.starts_with("HeadMoved") {
            return Ok(());
        }
        let Some(run_prefix) = run_prefix_of(&ev.track_key) else {
            return Ok(());
        };
        if is_commit {
            let count = self.run_commits.entry(run_prefix.clone()).or_insert(0);
            *count += 1;
            let value = *count;
            self.emit_counter(
                &ev,
                format!("{}/commits", run_prefix),
                value,
                "commits",
                out,
            )?;
        }
        if let Some(prev) = self.open_worktree.remove(&run_prefix) {
            self.end_slice(&prev, ev.ts_ns, out)?;
        }
        let mut open = self.begin_slice(ev, format!("{}/worktree", run_prefix), out)?;
        open.kind = Kind::Milestone;
        self.open_worktree.insert(run_prefix, open);
        Ok(())
    }

    fn on_task_queued<W: Write>(&mut self, mut ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let track = task_state_track(&ev.track_key);
        ev.ts_ns = self.next_track_ts(&track, ev.ts_ns);
        ev.track_key = track;
        ev.kind = Kind::TaskState;
        ev.custom_name = Some("Queued".to_string());
        ev.body = tacho::Event {
            body: Some(tacho::event::Body::TaskState(tacho::TaskState {
                state: Some(tacho::TaskStatus::TaskQueued as i32),
                because: ev.agent.as_ref().map(|a| format!("assigned to {}", a)),
            })),
        };
        out.emit(&ev, None)
    }

    /// Leaving review closes the task's open review findings.
    fn on_task_state<W: Write>(&mut self, mut ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let is_review = matches!(
            &ev.body.body,
            Some(tacho::event::Body::TaskState(ts))
                if ts.state == Some(tacho::TaskStatus::TaskReview as i32)
        );
        if !is_review {
            if let Some(slug) = ev
                .track_key
                .strip_prefix("task:")
                .and_then(|s| s.strip_suffix("/state"))
            {
                let findings = self
                    .open_reviews
                    .remove(&format!("task:{}/review", slug))
                    .unwrap_or_default();
                self.end_all(findings, ev.ts_ns, out)?;
            }
        }
        ev.ts_ns = self.next_track_ts(&ev.track_key, ev.ts_ns);
        out.emit(&ev, None)
    }

    /// A stall is a span on the agent's `stalls` track until the agent leaves Stalled.
    fn on_stall<W: Write>(&mut self, ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let mut agent_id = agent_of(&ev);
        if agent_id.is_empty() {
            agent_id = "unknown".to_string();
        }
        let track = stall_track(&ev.track_key, &agent_id);
        let open = self.begin_slice(ev, track, out)?;
        self.open_stalls.entry(agent_id).or_default().push(open);
        Ok(())
    }

    fn on_agent_state<W: Write>(&mut self, ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let is_stalled = ev.custom_name.as_deref() == Some("Stalled")
            || matches!(
                &ev.body.body,
                Some(tacho::event::Body::AgentState(s))
                    if s.state == Some(tacho::AgentStateType::Stalled as i32)
            );
        if !is_stalled {
            let stalls = self.open_stalls.remove(&agent_of(&ev)).unwrap_or_default();
            self.end_all(stalls, ev.ts_ns, out)?;
        }
        out.emit(&ev, None)
    }

    fn record_raw_slice(&mut self, ev: &Event) -> bool {
        if ev.counter_value.is_some() {
            return true;
        }
        match ev.kind.track_event_type() {
            TrackEventType::SliceBegin => {
                let end_kind = ev.kind.pairing().unwrap_or(Kind::CustomEnded);
                self.open_raw_slices
                    .entry(ev.track_key.clone())
                    .or_default()
                    .push(OpenSlice {
                        track_key: ev.track_key.clone(),
                        kind: end_kind,
                        open_ts: ev.ts_ns,
                        open_seq: ev.seq,
                        agent: ev.agent.clone(),
                        target_agent: None,
                        waiting_for_delivery: false,
                    });
                true
            }
            TrackEventType::SliceEnd => self
                .open_raw_slices
                .get_mut(&ev.track_key)
                .and_then(|stack| stack.pop())
                .is_some(),
            _ => true,
        }
    }

    fn on_run_ended<W: Write>(&mut self, mut ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let stalls = self.open_stalls.remove(&agent_of(&ev)).unwrap_or_default();
        self.end_all(stalls, ev.ts_ns, out)?;
        if let Some(wt) = self.open_worktree.remove(&ev.track_key) {
            self.end_slice(&wt, ev.ts_ns, out)?;
        }
        if !self.record_raw_slice(&ev) {
            return Ok(());
        }
        ev.ts_ns = self.next_track_ts(&ev.track_key, ev.ts_ns);
        out.emit(&ev, None)
    }

    /// Each daemon lifetime is one `Daemon [pid N, vX]` span, closed by the next daemon event.
    fn on_daemon<W: Write>(&mut self, mut ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        if let Some(prev) = self.open_daemon.take() {
            self.end_slice(&prev, ev.ts_ns, out)?;
        }
        let stopping = matches!(
            &ev.body.body,
            Some(tacho::event::Body::Daemon(d))
                if d.action == Some(tacho::DaemonAction::DaemonStopping as i32)
        );
        if stopping {
            return Ok(());
        }
        if ev.custom_name.is_none() {
            if let Some(tacho::event::Body::Daemon(d)) = &ev.body.body {
                ev.custom_name = Some(match (d.pid, d.version.as_deref()) {
                    (Some(pid), Some(ver)) => format!("Daemon [pid {}, v{}]", pid, ver),
                    (Some(pid), None) => format!("Daemon [pid {}]", pid),
                    _ => "Daemon".to_string(),
                });
            }
        }
        let track = ev.track_key.clone();
        self.open_daemon = Some(self.begin_slice(ev, track, out)?);
        Ok(())
    }

    /// A message is a span from send to delivery (or to the recipient's next activity),
    /// with a flow to the recipient's next turn or command.
    fn on_message<W: Write>(&mut self, mut ev: Event, out: &mut Sink<'_, W>) -> io::Result<()> {
        let (from, to, dir) = match &ev.body.body {
            Some(tacho::event::Body::Message(m)) => (
                m.from.clone().unwrap_or_else(|| "?".to_string()),
                m.to.clone().unwrap_or_else(|| "?".to_string()),
                m.direction,
            ),
            _ => ("?".to_string(), "?".to_string(), None),
        };
        let delivered = dir == Some(tacho::MessageDirection::Delivered as i32);
        if let Some(prev) = self.open_message.take() {
            self.end_slice(&prev, ev.ts_ns, out)?;
            if delivered {
                return Ok(());
            }
        }
        let track = if ev.track_key == "tacho" {
            MESSAGES_TRACK.to_string()
        } else {
            ev.track_key.clone()
        };
        ev.custom_name = Some(format!("Message: {} -> {}", from, to));
        // Recorder flows on messages would dangle (deliveries are folded into this span),
        // so replace them with one flow ending at the recipient's next activity.
        ev.flow_ids.clear();
        if !delivered && to != "?" {
            let flow = tacho_core::recorder::flow_ord::make(ev.seq, FLOW_MESSAGE_DELIVERY);
            ev.flow_ids.push(flow);
            self.pending_deliveries
                .entry(to.clone())
                .or_default()
                .push(flow);
        }
        let mut open = self.begin_slice(ev, track, out)?;
        open.target_agent = Some(to);
        open.waiting_for_delivery = dir == Some(tacho::MessageDirection::Sent as i32);
        self.open_message = Some(open);
        Ok(())
    }

    /// Closes every synthesized slice still open at the last timestamp seen.
    pub(super) fn finish<W: Write>(&mut self, out: &mut Sink<'_, W>) -> io::Result<()> {
        let final_ts = self.max_ts;
        if let Some(msg) = self.open_message.take() {
            self.end_slice(&msg, final_ts, out)?;
        }
        for (_, findings) in std::mem::take(&mut self.open_reviews) {
            self.end_all(findings, final_ts, out)?;
        }
        for (_, wt) in std::mem::take(&mut self.open_worktree) {
            self.end_slice(&wt, final_ts, out)?;
        }
        for (_, stalls) in std::mem::take(&mut self.open_stalls) {
            self.end_all(stalls, final_ts, out)?;
        }
        for (_, slices) in std::mem::take(&mut self.open_raw_slices) {
            self.end_all(slices, final_ts, out)?;
        }
        if let Some(d) = self.open_daemon.take() {
            self.end_slice(&d, final_ts, out)?;
        }
        Ok(())
    }
}
