//! Snapshot generation from SQLite WAL event log.
//!
//! Produces Perfetto-compatible `.perfetto-trace` byte streams adhering to design section 9.
//! The SQLite event log is the single source of truth; snapshots stream directly from
//! SQLite rows to Perfetto TracePackets without buffering the dataset in memory.

use prost::Message;
use rusqlite::{Connection, OpenFlags};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;
use tacho_core::{
    encode_snapshot_header, Encoder, Event, Kind, Recorder, TrackEventType, TrackKind,
    TrackRegistry,
};
use tacho_proto::tacho;

/// Statistics about an emitted snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Total bytes written to the output stream.
    pub bytes: u64,
    /// Total number of events encoded and emitted.
    pub events: u64,
}

fn run_prefix_of(track_key: &str) -> Option<String> {
    let mut parts = track_key.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if first.starts_with("agent:") && second.starts_with("run:") {
        Some(format!("{}/{}", first, second))
    } else {
        None
    }
}

fn register_hierarchy(
    reg: &mut TrackRegistry,
    rec_helper: &Recorder,
    track_key: &str,
    leaf_kind: TrackKind,
    leaf_unit: Option<&str>,
) {
    let chain = rec_helper.hierarchy_chain(track_key, leaf_kind, leaf_unit);
    for (k, parent_k, name, desc, kd, unit) in chain {
        let is_leaf = k == track_key;
        let final_kind = if is_leaf { leaf_kind } else { kd };
        let final_unit = if is_leaf {
            leaf_unit.or(unit.as_deref())
        } else {
            unit.as_deref()
        };
        if let Some(entry) = reg.get_by_key_mut(&k) {
            if is_leaf {
                entry.kind = final_kind;
                if entry.counter_unit.is_none() {
                    entry.counter_unit = final_unit.map(ToString::to_string);
                }
            }
        } else {
            reg.register(
                &k,
                parent_k.as_deref(),
                &name,
                desc.as_deref(),
                final_kind,
                final_unit,
            );
        }
    }
}

type EventTrackSummary = (Kind, String, Option<i64>, Option<String>);

fn finalize_snapshot_registry(reg: &mut TrackRegistry, summaries: &[EventTrackSummary]) {
    let rec_helper = Recorder::new();
    let mut active_keys = HashSet::new();

    for (kind, track_key, counter_val, custom_name) in summaries {
        match kind {
            Kind::TrackCreated | Kind::Prompt => continue,
            Kind::FileEdited => {
                if let Some(run_prefix) = run_prefix_of(track_key) {
                    let ft = format!("{}/files_edited", run_prefix);
                    register_hierarchy(reg, &rec_helper, &ft, TrackKind::Counter, Some("files"));
                    active_keys.insert(ft);
                }
            }
            Kind::Milestone if track_key.ends_with("/turns") => {
                let cname = custom_name.as_deref().unwrap_or("");
                if cname.starts_with("WorktreeCommit") {
                    if let Some(run_prefix) = run_prefix_of(track_key) {
                        let wt = format!("{}/worktree", run_prefix);
                        register_hierarchy(reg, &rec_helper, &wt, TrackKind::Slices, None);
                        active_keys.insert(wt);
                        let ct = format!("{}/commits", run_prefix);
                        register_hierarchy(
                            reg,
                            &rec_helper,
                            &ct,
                            TrackKind::Counter,
                            Some("commits"),
                        );
                        active_keys.insert(ct);
                    }
                } else if cname.starts_with("HeadMoved") {
                    if let Some(run_prefix) = run_prefix_of(track_key) {
                        let wt = format!("{}/worktree", run_prefix);
                        register_hierarchy(reg, &rec_helper, &wt, TrackKind::Slices, None);
                        active_keys.insert(wt);
                    }
                }
            }
            Kind::Milestone if track_key == "milestones" => {
                register_hierarchy(reg, &rec_helper, "milestones", TrackKind::State, None);
                active_keys.insert("milestones".to_string());
            }
            Kind::TaskQueued => {
                let st = if track_key.ends_with("/state") {
                    track_key.clone()
                } else {
                    format!("{}/state", track_key)
                };
                register_hierarchy(reg, &rec_helper, &st, TrackKind::State, None);
                active_keys.insert(st);
            }
            Kind::Stall => {
                let st = if track_key.ends_with("/stalls") {
                    track_key.clone()
                } else {
                    format!("{}/stalls", track_key)
                };
                register_hierarchy(reg, &rec_helper, &st, TrackKind::Slices, None);
                active_keys.insert(st);
            }
            Kind::Message if track_key == "tacho" => {
                let mt = "tacho/messages".to_string();
                register_hierarchy(reg, &rec_helper, &mt, TrackKind::Slices, None);
                active_keys.insert(mt);
            }
            _ => {
                if counter_val.is_some() {
                    register_hierarchy(
                        reg,
                        &rec_helper,
                        track_key,
                        TrackKind::Counter,
                        custom_name.as_deref(),
                    );
                } else if matches!(kind, Kind::TaskState | Kind::AgentState) {
                    register_hierarchy(reg, &rec_helper, track_key, TrackKind::State, None);
                } else {
                    let existing_kind = reg
                        .get_by_key(track_key)
                        .map(|e| e.kind)
                        .filter(|&k| k != TrackKind::Group)
                        .unwrap_or(TrackKind::Slices);
                    register_hierarchy(reg, &rec_helper, track_key, existing_kind, None);
                }
                active_keys.insert(track_key.clone());
            }
        }
    }

    reg.retain_used(&active_keys);
}

#[derive(Debug, Clone)]
struct OpenSynthSlice {
    track_key: String,
    kind: Kind,
    open_ts: u64,
    open_seq: u64,
    agent: Option<String>,
    target_agent: Option<String>,
     waiting_for_delivery: bool,
}

#[derive(Default)]
struct SnapshotProjector {
    synth_track_last_ts: HashMap<String, u64>,
    open_daemon: Option<OpenSynthSlice>,
    open_stalls: BTreeMap<String, Vec<OpenSynthSlice>>,
    open_worktree: BTreeMap<String, OpenSynthSlice>,
    open_reviews: BTreeMap<String, Vec<OpenSynthSlice>>,
    open_message: Option<OpenSynthSlice>,
    run_files_edited: HashMap<String, i64>,
    run_commits: HashMap<String, i64>,
    max_ts: u64,
}

impl SnapshotProjector {
    fn next_track_ts(&mut self, track: &str, candidate_ts: u64) -> u64 {
        let last = self.synth_track_last_ts.entry(track.to_string()).or_insert(0);
        let ts = (*last).max(candidate_ts);
        *last = ts;
        ts
    }

    fn emit_synth_end<W: Write>(
        &mut self,
        slice: &OpenSynthSlice,
        candidate_ts: u64,
        encoder: &mut Encoder,
        registry: &TrackRegistry,
        packet_buf: &mut Vec<u8>,
        writer: &mut W,
        total_bytes: &mut u64,
    ) -> io::Result<u64> {
        let min_end = slice.open_ts.saturating_add(1_000);
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
        packet_buf.clear();
        encoder.encode_event_with_type(
            &end_ev,
            Some(TrackEventType::SliceEnd),
            registry,
            packet_buf,
        );
        writer.write_all(packet_buf)?;
        *total_bytes += packet_buf.len() as u64;
        self.max_ts = self.max_ts.max(end_ts);
        Ok(end_ts)
    }

    #[allow(clippy::too_many_arguments)]
    fn process_event<W: Write>(
        &mut self,
        mut ev: Event,
        in_window: bool,
        encoder: &mut Encoder,
        registry: &TrackRegistry,
        packet_buf: &mut Vec<u8>,
        writer: &mut W,
        total_bytes: &mut u64,
        events_count: &mut u64,
    ) -> io::Result<()> {
        if !in_window {
            return Ok(());
        }
        *events_count += 1;
        self.max_ts = self.max_ts.max(ev.ts_ns);

        // Close open one-shot message if recipient agent becomes active
        if ev.kind != Kind::Message {
            if let Some(ref aid) = ev.agent {
                let should_close = self.open_message.as_ref().is_some_and(|m| {
                    !m.waiting_for_delivery && m.target_agent.as_deref() == Some(aid.as_str())
                });
                if should_close {
                    if let Some(msg_slice) = self.open_message.take() {
                        self.emit_synth_end(
                            &msg_slice,
                            ev.ts_ns,
                            encoder,
                            registry,
                            packet_buf,
                            writer,
                            total_bytes,
                        )?;
                    }
                }
            }
        }

        match ev.kind {
            Kind::TrackCreated | Kind::Prompt => Ok(()),

            Kind::FileEdited => {
                if let Some(run_prefix) = run_prefix_of(&ev.track_key) {
                    let count_val = {
                        let count = self.run_files_edited.entry(run_prefix.clone()).or_insert(0);
                        *count += 1;
                        *count
                    };
                    let ft = format!("{}/files_edited", run_prefix);
                    let ts = self.next_track_ts(&ft, ev.ts_ns);
                    let cnt_ev = Event::new(
                        ev.seq,
                        ts,
                        Kind::Custom,
                        ft,
                        None,
                        ev.agent,
                        tacho::Event { body: None },
                    )
                    .with_counter(count_val)
                    .with_custom_name("files");
                    packet_buf.clear();
                    encoder.encode_event(&cnt_ev, registry, packet_buf);
                    writer.write_all(packet_buf)?;
                    *total_bytes += packet_buf.len() as u64;
                }
                Ok(())
            }

            Kind::Milestone if ev.track_key.ends_with("/turns") => {
                let cname = ev.custom_name.clone().unwrap_or_default();
                let is_commit = cname.starts_with("WorktreeCommit");
                let is_head_moved = cname.starts_with("HeadMoved");
                if is_commit || is_head_moved {
                    if let Some(run_prefix) = run_prefix_of(&ev.track_key) {
                        if is_commit {
                            let c_val = {
                                let c_cnt = self.run_commits.entry(run_prefix.clone()).or_insert(0);
                                *c_cnt += 1;
                                *c_cnt
                            };
                            let ct = format!("{}/commits", run_prefix);
                            let c_ts = self.next_track_ts(&ct, ev.ts_ns);
                            let cnt_ev = Event::new(
                                ev.seq,
                                c_ts,
                                Kind::Custom,
                                ct,
                                None,
                                ev.agent.clone(),
                                tacho::Event { body: None },
                            )
                            .with_counter(c_val)
                            .with_custom_name("commits");
                            packet_buf.clear();
                            encoder.encode_event(&cnt_ev, registry, packet_buf);
                            writer.write_all(packet_buf)?;
                            *total_bytes += packet_buf.len() as u64;
                        }

                        let wt_track = format!("{}/worktree", run_prefix);
                        if let Some(prev) = self.open_worktree.remove(&run_prefix) {
                            self.emit_synth_end(
                                &prev,
                                ev.ts_ns,
                                encoder,
                                registry,
                                packet_buf,
                                writer,
                                total_bytes,
                            )?;
                        }
                        let begin_ts = self.next_track_ts(&wt_track, ev.ts_ns);
                        ev.ts_ns = begin_ts;
                        ev.track_key = wt_track.clone();
                        packet_buf.clear();
                        encoder.encode_event_with_type(
                            &ev,
                            Some(TrackEventType::SliceBegin),
                            registry,
                            packet_buf,
                        );
                        writer.write_all(packet_buf)?;
                        *total_bytes += packet_buf.len() as u64;
                        self.open_worktree.insert(
                            run_prefix,
                            OpenSynthSlice {
                                track_key: wt_track,
                                kind: Kind::Milestone,
                                open_ts: begin_ts,
                                open_seq: ev.seq,
                                agent: ev.agent,
                                target_agent: None,
                                waiting_for_delivery: false,
                            },
                        );
                    }
                }
                Ok(())
            }

            Kind::Milestone if ev.track_key == "milestones" => {
                let ts = self.next_track_ts("milestones", ev.ts_ns);
                ev.ts_ns = ts;
                packet_buf.clear();
                encoder.encode_event_with_type(
                    &ev,
                    Some(TrackEventType::State),
                    registry,
                    packet_buf,
                );
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            Kind::TaskQueued => {
                let st_track = if ev.track_key.ends_with("/state") {
                    ev.track_key.clone()
                } else {
                    format!("{}/state", ev.track_key)
                };
                let ts = self.next_track_ts(&st_track, ev.ts_ns);
                let because = ev.agent.as_ref().map(|a| format!("assigned to {}", a));
                ev.ts_ns = ts;
                ev.track_key = st_track;
                ev.kind = Kind::TaskState;
                ev.custom_name = Some("Queued".to_string());
                ev.body = tacho::Event {
                    body: Some(tacho::event::Body::TaskState(tacho::TaskState {
                        state: Some(tacho::TaskStatus::TaskQueued as i32),
                        because,
                    })),
                };
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            Kind::TaskState => {
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
                        let rev_track = format!("task:{}/review", slug);
                        if let Some(mut findings) = self.open_reviews.remove(&rev_track) {
                            while let Some(f) = findings.pop() {
                                self.emit_synth_end(
                                    &f,
                                    ev.ts_ns,
                                    encoder,
                                    registry,
                                    packet_buf,
                                    writer,
                                    total_bytes,
                                )?;
                            }
                        }
                    }
                }
                let ts = self.next_track_ts(&ev.track_key, ev.ts_ns);
                ev.ts_ns = ts;
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            Kind::Stall => {
                let agent_id = ev
                    .agent
                    .clone()
                    .unwrap_or_else(|| {
                        ev.track_key
                            .split('/')
                            .next()
                            .unwrap_or("unknown")
                            .strip_prefix("agent:")
                            .unwrap_or("unknown")
                            .to_string()
                    });
                let stall_track = if ev.track_key.ends_with("/stalls") {
                    ev.track_key.clone()
                } else {
                    format!("agent:{}/stalls", agent_id)
                };
                let begin_ts = self.next_track_ts(&stall_track, ev.ts_ns);
                ev.ts_ns = begin_ts;
                ev.track_key = stall_track.clone();
                packet_buf.clear();
                encoder.encode_event_with_type(
                    &ev,
                    Some(TrackEventType::SliceBegin),
                    registry,
                    packet_buf,
                );
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                self.open_stalls.entry(agent_id).or_default().push(OpenSynthSlice {
                    track_key: stall_track,
                    kind: Kind::Stall,
                    open_ts: begin_ts,
                    open_seq: ev.seq,
                    agent: ev.agent,
                    target_agent: None,
                    waiting_for_delivery: false,
                });
                Ok(())
            }

            Kind::AgentState => {
                let is_stalled = ev.custom_name.as_deref() == Some("Stalled")
                    || matches!(
                        &ev.body.body,
                        Some(tacho::event::Body::AgentState(s))
                            if s.state == Some(tacho::AgentStateType::Stalled as i32)
                    );
                if !is_stalled {
                    let agent_id = ev.agent.clone().unwrap_or_else(|| {
                        ev.track_key
                            .split('/')
                            .next()
                            .unwrap_or("")
                            .strip_prefix("agent:")
                            .unwrap_or("")
                            .to_string()
                    });
                    if let Some(mut stalls) = self.open_stalls.remove(&agent_id) {
                        while let Some(st) = stalls.pop() {
                            self.emit_synth_end(
                                &st,
                                ev.ts_ns,
                                encoder,
                                registry,
                                packet_buf,
                                writer,
                                total_bytes,
                            )?;
                        }
                    }
                }
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            Kind::RunEnded => {
                let agent_id = ev.agent.clone().unwrap_or_else(|| {
                    ev.track_key
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .strip_prefix("agent:")
                        .unwrap_or("")
                        .to_string()
                });
                if let Some(mut stalls) = self.open_stalls.remove(&agent_id) {
                    while let Some(st) = stalls.pop() {
                        self.emit_synth_end(
                            &st,
                            ev.ts_ns,
                            encoder,
                            registry,
                            packet_buf,
                            writer,
                            total_bytes,
                        )?;
                    }
                }
                if let Some(prev_wt) = self.open_worktree.remove(&ev.track_key) {
                    self.emit_synth_end(
                        &prev_wt,
                        ev.ts_ns,
                        encoder,
                        registry,
                        packet_buf,
                        writer,
                        total_bytes,
                    )?;
                }
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            Kind::Daemon => {
                if let Some(prev) = self.open_daemon.take() {
                    self.emit_synth_end(
                        &prev,
                        ev.ts_ns,
                        encoder,
                        registry,
                        packet_buf,
                        writer,
                        total_bytes,
                    )?;
                }
                let is_stopping = matches!(
                    &ev.body.body,
                    Some(tacho::event::Body::Daemon(d))
                        if d.action == Some(tacho::DaemonAction::DaemonStopping as i32)
                );
                if !is_stopping {
                    let begin_ts = self.next_track_ts(&ev.track_key, ev.ts_ns);
                    ev.ts_ns = begin_ts;
                    if ev.custom_name.is_none() {
                        if let Some(tacho::event::Body::Daemon(d)) = &ev.body.body {
                            let label = match (d.pid, d.version.as_deref()) {
                                (Some(pid), Some(ver)) => {
                                    format!("Daemon [pid {}, v{}]", pid, ver)
                                }
                                (Some(pid), None) => format!("Daemon [pid {}]", pid),
                                _ => "Daemon".to_string(),
                            };
                            ev.custom_name = Some(label);
                        }
                    }
                    let track_key = ev.track_key.clone();
                    let seq = ev.seq;
                    let agent = ev.agent.clone();
                    packet_buf.clear();
                    encoder.encode_event_with_type(
                        &ev,
                        Some(TrackEventType::SliceBegin),
                        registry,
                        packet_buf,
                    );
                    writer.write_all(packet_buf)?;
                    *total_bytes += packet_buf.len() as u64;
                    self.open_daemon = Some(OpenSynthSlice {
                        track_key,
                        kind: Kind::Daemon,
                        open_ts: begin_ts,
                        open_seq: seq,
                        agent,
                        target_agent: None,
                        waiting_for_delivery: false,
                    });
                }
                Ok(())
            }

            Kind::Message => {
                let msg_track = if ev.track_key == "tacho" {
                    "tacho/messages".to_string()
                } else {
                    ev.track_key.clone()
                };
                let (from, to, dir) = match &ev.body.body {
                    Some(tacho::event::Body::Message(m)) => (
                        m.from.clone().unwrap_or_else(|| "?".to_string()),
                        m.to.clone().unwrap_or_else(|| "?".to_string()),
                        m.direction,
                    ),
                    _ => ("?".to_string(), "?".to_string(), None),
                };
                if dir == Some(tacho::MessageDirection::Delivered as i32) {
                    if let Some(prev) = self.open_message.take() {
                        self.emit_synth_end(
                            &prev,
                            ev.ts_ns,
                            encoder,
                            registry,
                            packet_buf,
                            writer,
                            total_bytes,
                        )?;
                        return Ok(());
                    }
                }
                if let Some(prev) = self.open_message.take() {
                    self.emit_synth_end(
                        &prev,
                        ev.ts_ns,
                        encoder,
                        registry,
                        packet_buf,
                        writer,
                        total_bytes,
                    )?;
                }
                let begin_ts = self.next_track_ts(&msg_track, ev.ts_ns);
                ev.ts_ns = begin_ts;
                ev.track_key = msg_track.clone();
                ev.custom_name = Some(format!("Message: {} -> {}", from, to));
                if dir == Some(tacho::MessageDirection::Sent as i32) {
                    ev.flow_ids.clear();
                }
                let seq = ev.seq;
                let agent = ev.agent.clone();
                packet_buf.clear();
                encoder.encode_event_with_type(
                    &ev,
                    Some(TrackEventType::SliceBegin),
                    registry,
                    packet_buf,
                );
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                self.open_message = Some(OpenSynthSlice {
                    track_key: msg_track,
                    kind: Kind::Message,
                    open_ts: begin_ts,
                    open_seq: seq,
                    agent,
                    target_agent: Some(to),
                    waiting_for_delivery: dir == Some(tacho::MessageDirection::Sent as i32),
                });
                Ok(())
            }

            Kind::ReviewFinding => {
                let begin_ts = self.next_track_ts(&ev.track_key, ev.ts_ns);
                ev.ts_ns = begin_ts;
                let track_key = ev.track_key.clone();
                let seq = ev.seq;
                let agent = ev.agent.clone();
                packet_buf.clear();
                encoder.encode_event_with_type(
                    &ev,
                    Some(TrackEventType::SliceBegin),
                    registry,
                    packet_buf,
                );
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                self.open_reviews
                    .entry(track_key.clone())
                    .or_default()
                    .push(OpenSynthSlice {
                        track_key,
                        kind: Kind::ReviewFinding,
                        open_ts: begin_ts,
                        open_seq: seq,
                        agent,
                        target_agent: None,
                        waiting_for_delivery: false,
                    });
                Ok(())
            }

            Kind::ReviewEnded => {
                let mut end_ts = ev.ts_ns;
                if let Some(mut findings) = self.open_reviews.remove(&ev.track_key) {
                    while let Some(f) = findings.pop() {
                        end_ts = self.emit_synth_end(
                            &f,
                            ev.ts_ns,
                            encoder,
                            registry,
                            packet_buf,
                            writer,
                            total_bytes,
                        )?;
                    }
                }
                ev.ts_ns = self.next_track_ts(&ev.track_key, ev.ts_ns.max(end_ts));
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }

            _ => {
                packet_buf.clear();
                encoder.encode_event(&ev, registry, packet_buf);
                writer.write_all(packet_buf)?;
                *total_bytes += packet_buf.len() as u64;
                Ok(())
            }
        }
    }

    fn finish<W: Write>(
        &mut self,
        encoder: &mut Encoder,
        registry: &TrackRegistry,
        packet_buf: &mut Vec<u8>,
        writer: &mut W,
        total_bytes: &mut u64,
    ) -> io::Result<()> {
        let final_ts = self.max_ts;

        if let Some(msg) = self.open_message.take() {
            self.emit_synth_end(
                &msg,
                final_ts,
                encoder,
                registry,
                packet_buf,
                writer,
                total_bytes,
            )?;
        }

        let review_tracks: Vec<String> = self.open_reviews.keys().cloned().collect();
        for rt in review_tracks {
            if let Some(mut findings) = self.open_reviews.remove(&rt) {
                while let Some(f) = findings.pop() {
                    self.emit_synth_end(
                        &f,
                        final_ts,
                        encoder,
                        registry,
                        packet_buf,
                        writer,
                        total_bytes,
                    )?;
                }
            }
        }

        let wt_keys: Vec<String> = self.open_worktree.keys().cloned().collect();
        for wk in wt_keys {
            if let Some(wt) = self.open_worktree.remove(&wk) {
                self.emit_synth_end(
                    &wt,
                    final_ts,
                    encoder,
                    registry,
                    packet_buf,
                    writer,
                    total_bytes,
                )?;
            }
        }

        let stall_agents: Vec<String> = self.open_stalls.keys().cloned().collect();
        for aid in stall_agents {
            if let Some(mut stalls) = self.open_stalls.remove(&aid) {
                while let Some(st) = stalls.pop() {
                    self.emit_synth_end(
                        &st,
                        final_ts,
                        encoder,
                        registry,
                        packet_buf,
                        writer,
                        total_bytes,
                    )?;
                }
            }
        }

        if let Some(d) = self.open_daemon.take() {
            self.emit_synth_end(
                &d,
                final_ts,
                encoder,
                registry,
                packet_buf,
                writer,
                total_bytes,
            )?;
        }

        Ok(())
    }
}

/// Rebuilds the `TrackRegistry` by folding persisted `TrackCreated` rows up to `max_seq`.
pub fn build_registry_from_conn_up_to(
    conn: &Connection,
    max_seq: Option<u64>,
) -> io::Result<TrackRegistry> {
    let mut reg_stmt = conn
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body
             FROM events
             WHERE kind = ?1 AND (?2 IS NULL OR seq <= ?2)
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = reg_stmt
        .query(rusqlite::params![Kind::TrackCreated as u32, max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut events = Vec::new();
    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;
        let body_bytes: Vec<u8> = row.get(6).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);

        let body = tacho::Event::decode(&body_bytes[..]).unwrap_or(tacho::Event { body: None });
        let ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);
        events.push(ev);
    }

    let rec = Recorder::restore(events.iter());
    let mut reg = rec.registry().clone();

    let mut summary_stmt = conn
        .prepare(
            "SELECT DISTINCT kind, track_key, counter_value, custom_name
             FROM events
             WHERE (?1 IS NULL OR seq <= ?1)",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut summary_rows = summary_stmt
        .query(rusqlite::params![max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut summaries = Vec::new();
    while let Some(row) = summary_rows
        .next()
        .map_err(|e| io::Error::other(e.to_string()))?
    {
        let kind_int: u32 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let counter_val: Option<i64> = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let custom_name: Option<String> =
            row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);
        summaries.push((kind, track_key, counter_val, custom_name));
    }

    finalize_snapshot_registry(&mut reg, &summaries);

    Ok(reg)
}

/// Rebuilds the `TrackRegistry` by folding all persisted `TrackCreated` rows from the event log.
pub fn build_registry_from_conn(conn: &Connection) -> io::Result<TrackRegistry> {
    build_registry_from_conn_up_to(conn, None)
}

/// Writes a complete flight recorder snapshot to `w`, streaming rows directly from SQLite.
pub fn write_snapshot_conn(
    conn: &Connection,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let max_seq: Option<u64> = tx
        .query_row(
            "SELECT MAX(seq) FROM events WHERE (?1 IS NULL OR ts_ns <= ?1)",
            rusqlite::params![until],
            |r| r.get(0),
        )
        .unwrap_or(None);

    let max_seq = match max_seq {
        Some(s) => s,
        None => {
            let registry = TrackRegistry::new();
            let (now_ns, boottime_ns) = match until {
                Some(u) => (u, None),
                None => (
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64,
                    get_boottime_ns(),
                ),
            };
            let mut writer = io::BufWriter::with_capacity(8192, w);
            let mut header_bytes = Vec::with_capacity(8192);
            encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
            writer.write_all(&header_bytes)?;
            writer.flush()?;
            return Ok(SnapshotStats {
                bytes: header_bytes.len() as u64,
                events: 0,
            });
        }
    };

    let registry = build_registry_from_conn_up_to(&tx, Some(max_seq))?;

    let (now_ns, boottime_ns) = match until {
        Some(u) => (u, None),
        None => (
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            get_boottime_ns(),
        ),
    };

    let mut writer = io::BufWriter::with_capacity(65536, w);

    // 1-4. Snapshot Header
    let mut header_bytes = Vec::with_capacity(8192);
    encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
    writer.write_all(&header_bytes)?;
    let mut total_bytes = header_bytes.len() as u64;

    // Pre-collect flow origins (cause_seq * 256 + ordinal) up to max_seq
    let mut origin_flows: HashMap<u64, Vec<u64>> = HashMap::new();
    {
        let mut flow_stmt = tx
            .prepare(
                "SELECT kind, cause_seq, body
                 FROM events
                 WHERE (cause_seq IS NOT NULL OR kind = ?1) AND seq <= ?2
                 ORDER BY seq ASC",
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut flow_rows = flow_stmt
            .query(rusqlite::params![Kind::Run as u32, max_seq])
            .map_err(|e| io::Error::other(e.to_string()))?;
        while let Some(row) = flow_rows
            .next()
            .map_err(|e| io::Error::other(e.to_string()))?
        {
            let kind_int: u32 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
            let cause_seq: Option<u64> = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
            let kind = Kind::ALL
                .get(kind_int as usize)
                .copied()
                .unwrap_or(Kind::Daemon);
            let parent_run_seq = if kind == Kind::Run {
                let slice: &[u8] = row
                    .get_ref(2)
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .as_bytes()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                tacho::Event::decode(slice).ok().and_then(|b| match b.body {
                    Some(tacho::event::Body::Run(r)) => r.parent_run_seq,
                    _ => None,
                })
            } else {
                None
            };
            Recorder::record_flow_origin(kind, cause_seq, parent_run_seq, &mut origin_flows);
        }
    }

    // 5. Query events ordered by seq ASC (streaming, O(1) memory)
    let mut stmt = tx
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name
             FROM events
             WHERE (?1 IS NULL OR ts_ns >= ?1) AND (?2 IS NULL OR ts_ns <= ?2) AND seq <= ?3
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = stmt
        .query(rusqlite::params![since, until, max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::with_capacity(2048);
    let mut events_count = 0u64;
    let mut projector = SnapshotProjector::default();

    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;

        let slice: &[u8] = row
            .get_ref(6)
            .map_err(|e| io::Error::other(e.to_string()))?
            .as_bytes()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let counter_val: Option<i64> = row.get(7).map_err(|e| io::Error::other(e.to_string()))?;
        let custom_name: Option<String> =
            row.get(8).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);

        let body = tacho::Event::decode(slice).unwrap_or(tacho::Event { body: None });

        let mut ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);

        if let Some(c) = counter_val {
            ev = ev.with_counter(c);
        }
        if let Some(cn) = custom_name {
            ev = ev.with_custom_name(cn);
        }
        Recorder::populate_event_flows(&mut ev, &origin_flows);

        projector.process_event(
            ev,
            true,
            &mut encoder,
            &registry,
            &mut packet_buf,
            &mut writer,
            &mut total_bytes,
            &mut events_count,
        )?;
    }

    projector.finish(
        &mut encoder,
        &registry,
        &mut packet_buf,
        &mut writer,
        &mut total_bytes,
    )?;

    writer.flush()?;

    Ok(SnapshotStats {
        bytes: total_bytes,
        events: events_count,
    })
}

/// Writes a complete flight recorder snapshot directly from `$TACHO_HOME/events.sqlite`.
///
/// Uses an independent read-only WAL connection; never blocks or depends on the appender.
pub fn write_snapshot(
    db_path: &Path,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    if !db_path.exists() {
        let registry = TrackRegistry::new();
        let (now_ns, boottime_ns) = match until {
            Some(u) => (u, None),
            None => (
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
                get_boottime_ns(),
            ),
        };
        let mut header_bytes = Vec::with_capacity(8192);
        encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
        w.write_all(&header_bytes)?;
        w.flush()?;
        return Ok(SnapshotStats {
            bytes: header_bytes.len() as u64,
            events: 0,
        });
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| io::Error::other(format!("failed to open sqlite db {:?}: {}", db_path, e)))?;

    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    let _ = conn.pragma_update(None, "query_only", true);

    write_snapshot_conn(&conn, since, until, w)
}

/// Rebuilds a Perfetto trace from the durable SQLite event log by restoring a fresh
/// `Recorder` state machine from the persisted events and encoding through `Encoder`.
pub fn rebuild_from_log(
    db_path: &Path,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    if !db_path.exists() {
        return write_snapshot(db_path, since, until, w);
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| io::Error::other(format!("failed to open sqlite db {:?}: {}", db_path, e)))?;
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    let _ = conn.pragma_update(None, "query_only", true);

    let max_seq: Option<u64> = conn
        .query_row(
            "SELECT MAX(seq) FROM events WHERE (?1 IS NULL OR ts_ns <= ?1)",
            rusqlite::params![until],
            |r| r.get(0),
        )
        .unwrap_or(None);

    let Some(max_seq) = max_seq else {
        return write_snapshot_conn(&conn, since, until, w);
    };

    let mut stmt = conn
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name
             FROM events
             WHERE seq <= ?1
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = stmt
        .query(rusqlite::params![max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut all_events = Vec::new();
    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;
        let body_bytes: Vec<u8> = row.get(6).map_err(|e| io::Error::other(e.to_string()))?;
        let counter_val: Option<i64> = row.get(7).map_err(|e| io::Error::other(e.to_string()))?;
        let custom_name: Option<String> =
            row.get(8).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);
        let body = tacho::Event::decode(&body_bytes[..]).unwrap_or(tacho::Event { body: None });
        let mut ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);
        if let Some(c) = counter_val {
            ev = ev.with_counter(c);
        }
        if let Some(cn) = custom_name {
            ev = ev.with_custom_name(cn);
        }
        all_events.push(ev);
    }

    Recorder::populate_flows(&mut all_events);
    let rec = Recorder::restore(all_events.iter());
    let mut registry = rec.registry().clone();
    let summaries: Vec<EventTrackSummary> = all_events
        .iter()
        .map(|ev| {
            (
                ev.kind,
                ev.track_key.clone(),
                ev.counter_value,
                ev.custom_name.clone(),
            )
        })
        .collect();
    finalize_snapshot_registry(&mut registry, &summaries);

    let (now_ns, boottime_ns) = match until {
        Some(u) => (u, None),
        None => (
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            get_boottime_ns(),
        ),
    };

    let mut writer = io::BufWriter::with_capacity(65536, w);
    let mut header_bytes = Vec::with_capacity(8192);
    encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
    writer.write_all(&header_bytes)?;
    let mut total_bytes = header_bytes.len() as u64;

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::with_capacity(2048);
    let mut events_count = 0u64;
    let mut projector = SnapshotProjector::default();

    for ev in all_events {
        let in_window = !since.is_some_and(|s| ev.ts_ns < s) && !until.is_some_and(|u| ev.ts_ns > u);
        projector.process_event(
            ev,
            in_window,
            &mut encoder,
            &registry,
            &mut packet_buf,
            &mut writer,
            &mut total_bytes,
            &mut events_count,
        )?;
    }

    projector.finish(
        &mut encoder,
        &registry,
        &mut packet_buf,
        &mut writer,
        &mut total_bytes,
    )?;

    writer.flush()?;
    Ok(SnapshotStats {
        bytes: total_bytes,
        events: events_count,
    })
}

/// Reads CLOCK_BOOTTIME in nanoseconds on Linux systems.
pub fn get_boottime_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let res = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if res == 0 {
        Some((ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64))
    } else {
        None
    }
}

/// Parses varint from slice, returning (value, bytes_consumed).
#[inline]
fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0;
    for (i, &b) in buf.iter().enumerate() {
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((val, i + 1));
        }
        shift += 7;
        if shift > 64 {
            return None;
        }
    }
    None
}

/// Inspects framed TracePacket bytes (Tag 0x0a + varint length + payload).
/// If the first field in payload is field 8 (varint timestamp), extracts it.
#[inline]
pub fn extract_framed_packet_timestamp(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() || buf[0] != 0x0a {
        return None;
    }

    let (packet_len, len_bytes) = decode_varint(&buf[1..])?;
    let packet_start = 1 + len_bytes;
    let total_len = packet_start + packet_len as usize;

    if total_len > buf.len() {
        return None;
    }

    let payload = &buf[packet_start..total_len];
    if !payload.is_empty() && payload[0] == 0x40 {
        // Tag 0x40 = field 8, wire type 0 (varint timestamp)
        let (ts, _) = decode_varint(&payload[1..])?;
        return Some((ts, total_len));
    }

    // Packet has no leading timestamp field
    Some((0, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::EventLog;
    use tacho_core::{Event, Kind};
    use tacho_proto::tacho;

    #[test]
    fn test_extract_packet_timestamp() {
        let registry = TrackRegistry::new();
        let mut encoder = Encoder::new();
        let mut out = Vec::new();

        let ev = Event::new(
            1,
            123456789,
            Kind::Run,
            "agent:1/run:1",
            None,
            None,
            tacho::Event { body: None },
        );
        encoder.encode_event(&ev, &registry, &mut out);

        let (ts, total_len) = extract_framed_packet_timestamp(&out).unwrap();
        assert_eq!(ts, 123456789);
        assert_eq!(total_len, out.len());
    }

    #[test]
    fn test_write_snapshot_streaming() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("events.sqlite");
        let mut log = EventLog::open(&db_path).unwrap();

        let ev = Event::new(
            1,
            1_000_000,
            Kind::Daemon,
            "daemon",
            None,
            None,
            tacho::Event { body: None },
        );
        log.insert_batch(&[ev]).unwrap();

        let mut output = Vec::new();
        let stats = write_snapshot(&db_path, None, None, &mut output).unwrap();

        assert_eq!(stats.events, 1);
        assert!(stats.bytes > 0);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_write_snapshot_and_rebuild_from_log_byte_identical() {
        use tacho_core::{Input, MessageDirection, SessionKey, TaskStatus, ToolInput};

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("events.sqlite");
        let mut log = EventLog::open(&db_path).unwrap();

        let mut rec = Recorder::new();
        let mut events = Vec::new();
        let t0 = 1_000_000_000u64;
        let sess = SessionKey::new("claude", "sess-rebuild");

        rec.apply(
            Input::Task {
                source_ts_ns: Some(t0),
                slug: "rebuild-task".into(),
                brief_digest: None,
                brief_bytes: Some(128),
                paths: vec!["src/main.rs".into()],
                agent: Some("worker-1".into()),
                base_commit: None,
                token_budget: None,
                cost_budget_micros: None,
            },
            t0,
            &mut events,
        );
        rec.apply(
            Input::RunStart {
                source_ts_ns: Some(t0 + 100),
                provider: "claude".into(),
                session_id: "sess-rebuild".into(),
                agent: Some("worker-1".into()),
                task: Some("rebuild-task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("claude-sonnet-4-6".into()),
                parent_session: None,
                git_tree: None,
                role: None,
            },
            t0 + 100,
            &mut events,
        );
        rec.apply(
            Input::Prompt {
                source_ts_ns: Some(t0 + 200),
                session: sess.clone(),
                text_len: 50,
                digest: None,
            },
            t0 + 200,
            &mut events,
        );
        rec.apply(
            Input::ToolBegin {
                source_ts_ns: Some(t0 + 300),
                session: sess.clone(),
                tool_use_id: "t1".into(),
                tool_name: "Bash".into(),
                input: ToolInput::Shell {
                    argv: "cargo test".into(),
                    cwd: None,
                    git_tree: None,
                },
            },
            t0 + 300,
            &mut events,
        );
        rec.apply(
            Input::ToolEnd {
                source_ts_ns: Some(t0 + 400),
                session: sess.clone(),
                tool_use_id: "t1".into(),
                error: false,
                result_bytes: Some(42),
                exit_code: Some(0),
            },
            t0 + 400,
            &mut events,
        );
        rec.apply(
            Input::Message {
                source_ts_ns: Some(t0 + 500),
                from: "orch".into(),
                to: "worker-1".into(),
                id: Some("m-1".into()),
                bytes: Some(16),
                dir: Some(MessageDirection::Sent),
            },
            t0 + 500,
            &mut events,
        );
        rec.apply(
            Input::Message {
                source_ts_ns: Some(t0 + 600),
                from: "orch".into(),
                to: "worker-1".into(),
                id: Some("m-1".into()),
                bytes: Some(16),
                dir: Some(MessageDirection::Delivered),
            },
            t0 + 600,
            &mut events,
        );
        rec.apply(
            Input::TaskState {
                source_ts_ns: Some(t0 + 700),
                slug: "rebuild-task".into(),
                state: TaskStatus::Done,
                because: Some("all green".into()),
            },
            t0 + 700,
            &mut events,
        );

        log.insert_batch(&events).unwrap();

        let until = Some(t0 + 1_000);
        let mut snap_bytes = Vec::new();
        let mut rebuild_bytes = Vec::new();
        let s1 = write_snapshot(&db_path, None, until, &mut snap_bytes).unwrap();
        let s2 = rebuild_from_log(&db_path, None, until, &mut rebuild_bytes).unwrap();

        assert_eq!(s1, s2);
        assert_eq!(
            snap_bytes, rebuild_bytes,
            "write_snapshot and rebuild_from_log must be byte-for-byte identical at the same until"
        );
    }
}
