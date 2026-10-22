//! Track registration and hierarchy resolution for the flight recorder.

use crate::classify::CommandClass;
use crate::event::Event;
use crate::input::FileEditKind;
use crate::kind::Kind;
use crate::track::TrackKind;
use tacho_proto::tacho;

use super::slices::OpenToolSlice;
use super::Recorder;

pub(crate) fn command_class_name(class: CommandClass) -> &'static str {
    match class {
        CommandClass::Build => "Build",
        CommandClass::Test => "Test",
        CommandClass::Lint => "Lint",
        CommandClass::Format => "Format",
        CommandClass::Install => "Install",
        CommandClass::Commit => "Commit",
        CommandClass::Push => "Push",
        CommandClass::Fetch => "Fetch",
        CommandClass::Rebase => "Rebase",
        CommandClass::Package => "Package",
        CommandClass::Exec => "Exec",
        _ => "Command",
    }
}

pub(crate) fn to_proto_cmd_class(class: CommandClass) -> tacho::CommandClass {
    match class {
        CommandClass::Build => tacho::CommandClass::Build,
        CommandClass::Test => tacho::CommandClass::Test,
        CommandClass::Lint => tacho::CommandClass::Lint,
        CommandClass::Format => tacho::CommandClass::Format,
        CommandClass::Install => tacho::CommandClass::Install,
        CommandClass::Commit => tacho::CommandClass::Commit,
        CommandClass::Push => tacho::CommandClass::Push,
        CommandClass::Fetch => tacho::CommandClass::Fetch,
        CommandClass::Rebase => tacho::CommandClass::Rebase,
        CommandClass::Package => tacho::CommandClass::Package,
        CommandClass::Exec => tacho::CommandClass::Exec,
        _ => tacho::CommandClass::Command,
    }
}

pub(crate) fn from_proto_cmd_class(c: i32) -> Option<CommandClass> {
    match c {
        1 => Some(CommandClass::Command),
        2 => Some(CommandClass::Build),
        3 => Some(CommandClass::Test),
        4 => Some(CommandClass::Lint),
        5 => Some(CommandClass::Format),
        6 => Some(CommandClass::Install),
        7 => Some(CommandClass::Commit),
        8 => Some(CommandClass::Push),
        9 => Some(CommandClass::Fetch),
        10 => Some(CommandClass::Rebase),
        11 => Some(CommandClass::Package),
        12 => Some(CommandClass::Exec),
        _ => None,
    }
}

#[allow(dead_code)]
pub(crate) fn from_proto_edit_kind(k: i32) -> Option<FileEditKind> {
    match k {
        1 => Some(FileEditKind::Edit),
        2 => Some(FileEditKind::Create),
        3 => Some(FileEditKind::Delete),
        4 => Some(FileEditKind::Rename),
        _ => None,
    }
}

pub(crate) fn to_proto_edit_kind(k: FileEditKind) -> tacho::FileEditKind {
    match k {
        FileEditKind::Edit => tacho::FileEditKind::Edit,
        FileEditKind::Create => tacho::FileEditKind::Create,
        FileEditKind::Delete => tacho::FileEditKind::Delete,
        FileEditKind::Rename => tacho::FileEditKind::Rename,
    }
}

pub(crate) type TrackHierarchyEntry = (String, Option<String>, String, TrackKind, Option<String>);

impl Recorder {
    /// Allocates the next monotonic sequence number.
    pub(crate) fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Computes monotonic track timestamp obeying per-track clamping.
    pub(crate) fn clamp_ts(&mut self, track: &str, candidate_ts: u64) -> u64 {
        let last = self.track_last_ts.entry(track.to_string()).or_insert(0);
        let ts = (*last).max(candidate_ts);
        *last = ts;
        ts
    }

    /// Ensures that the track hierarchy exists in the registry and emits `TrackCreated` events on `daemon`.
    pub(crate) fn ensure_track(&mut self, track_key: &str, ts: u64, out: &mut Vec<Event>) {
        self.ensure_track_with_kind(track_key, TrackKind::Slices, None, ts, out);
    }

    /// Ensures that the track hierarchy exists with a specific default kind/unit for custom tracks.
    pub(crate) fn ensure_track_with_kind(
        &mut self,
        track_key: &str,
        default_kind: TrackKind,
        default_unit: Option<&str>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        if self.registered_tracks.contains(track_key) {
            return;
        }

        // Deconstruct hierarchy to ensure all parent tracks exist first
        let parts = self.hierarchy_chain(track_key, default_kind, default_unit);
        for (key, parent_key, name, kind, unit) in parts {
            if self.registered_tracks.insert(key.clone()) {
                let entry = self.registry.register(
                    &key,
                    parent_key.as_deref(),
                    &name,
                    None,
                    kind,
                    unit.as_deref(),
                );
                let uuid = entry.uuid;
                let parent_uuid = entry.parent_uuid;
                let collision = entry.collision;

                let seq = self.alloc_seq();
                let daemon_ts = self.clamp_ts("daemon", ts);

                out.push(Event {
                    seq,
                    ts_ns: daemon_ts,
                    kind: Kind::TrackCreated,
                    track_key: "daemon".to_string(),
                    cause_seq: None,
                    agent: None,
                    custom_name: None,
                    flow_ids: Vec::new(),
                    terminating_flow_ids: Vec::new(),
                    counter_value: None,
                    body: tacho::Event {
                        body: Some(tacho::event::Body::TrackCreated(tacho::TrackCreated {
                            key: Some(key),
                            uuid: Some(uuid),
                            parent_uuid,
                            name: Some(name),
                            collision: Some(collision),
                        })),
                    },
                });
            }
        }
    }

    /// Resolves the parent-child track chain for a track key.
    pub(crate) fn hierarchy_chain(
        &self,
        track_key: &str,
        default_kind: TrackKind,
        default_unit: Option<&str>,
    ) -> Vec<TrackHierarchyEntry> {
        let mut chain = Vec::new();
        let mut add = |k: &str, p: Option<&str>, n: &str, kd: TrackKind, u: Option<&str>| {
            chain.push((
                k.to_string(),
                p.map(ToString::to_string),
                n.to_string(),
                kd,
                u.map(ToString::to_string),
            ));
        };

        add("tacho", None, "tacho", TrackKind::Group, None);
        add("daemon", Some("tacho"), "daemon", TrackKind::Slices, None);

        if track_key == "tacho" || track_key == "daemon" {
            return chain;
        }

        if track_key == "daemon/counters" {
            add(
                "daemon/counters",
                Some("daemon"),
                "counters",
                TrackKind::Counter,
                Some("count"),
            );
            return chain;
        }

        if track_key.starts_with("machine") {
            add("machine", Some("tacho"), "machine", TrackKind::Group, None);
            if track_key != "machine" {
                let name = track_key.strip_prefix("machine/").unwrap_or(track_key);
                add(track_key, Some("machine"), name, TrackKind::Counter, None);
            }
            return chain;
        }

        if track_key.starts_with("providers") || track_key.starts_with("provider:") {
            add(
                "providers",
                Some("tacho"),
                "providers",
                TrackKind::Group,
                None,
            );
            let sub = track_key.strip_prefix("providers/").unwrap_or(track_key);
            let parts: Vec<&str> = sub.split('/').collect();
            let prov_track = parts[0];
            add(
                prov_track,
                Some("providers"),
                prov_track,
                TrackKind::Slices,
                None,
            );
            if parts.len() > 1 {
                add(
                    track_key,
                    Some(prov_track),
                    parts[1],
                    TrackKind::Counter,
                    None,
                );
            }
            return chain;
        }

        if track_key.starts_with("tasks") || track_key.starts_with("task:") {
            add("tasks", Some("tacho"), "tasks", TrackKind::Group, None);
            let sub = track_key.strip_prefix("tasks/").unwrap_or(track_key);
            let parts: Vec<&str> = sub.split('/').collect();
            let task_track = parts[0];
            add(
                task_track,
                Some("tasks"),
                task_track,
                TrackKind::Group,
                None,
            );
            if parts.len() > 1 {
                let kind = if parts[1] == "state" {
                    TrackKind::State
                } else {
                    TrackKind::Slices
                };
                add(track_key, Some(task_track), parts[1], kind, None);
            }
            return chain;
        }

        if track_key.starts_with("custom") || track_key.starts_with("custom:") {
            add("custom", Some("tacho"), "custom", TrackKind::Group, None);
            if track_key != "custom" {
                add(
                    track_key,
                    Some("custom"),
                    track_key,
                    default_kind,
                    default_unit,
                );
            }
            return chain;
        }

        if track_key.starts_with("agents") || track_key.starts_with("agent:") {
            add("agents", Some("tacho"), "agents", TrackKind::Group, None);
            let segs: Vec<&str> = track_key.split('/').collect();
            let agent_key = segs[0];
            add(agent_key, Some("agents"), agent_key, TrackKind::Group, None);

            if segs.len() > 1 {
                if segs[1] == "state" {
                    let st = format!("{}/state", agent_key);
                    add(&st, Some(agent_key), "state", TrackKind::State, None);
                } else if segs[1] == "cpu" || segs[1] == "rss" {
                    let unit = if segs[1] == "cpu" { "us" } else { "bytes" };
                    add(
                        track_key,
                        Some(agent_key),
                        segs[1],
                        TrackKind::Counter,
                        Some(unit),
                    );
                } else if segs[1].starts_with("run:") {
                    let run_key = format!("{}/{}", agent_key, segs[1]);
                    add(&run_key, Some(agent_key), segs[1], TrackKind::Slices, None);

                    if segs.len() > 2 {
                        let sub_name = segs[2];
                        let (kind, unit) = match sub_name {
                            "turns" | "waits" => (TrackKind::Slices, None),
                            "tokens_in" | "tokens_out" | "tokens_cached" => {
                                (TrackKind::Counter, Some("tokens"))
                            }
                            "cost" => (TrackKind::Counter, Some("micro-USD")),
                            "cpu" => (TrackKind::Counter, Some("us")),
                            "rss" => (TrackKind::Counter, Some("bytes")),
                            _ => (TrackKind::Slices, None),
                        };
                        add(track_key, Some(&run_key), sub_name, kind, unit);
                    }
                } else {
                    add(
                        track_key,
                        Some(agent_key),
                        segs[1],
                        default_kind,
                        default_unit,
                    );
                }
            }
            return chain;
        }

        // Fallback for ANY unknown track key: group under `custom`
        add("custom", Some("tacho"), "custom", TrackKind::Group, None);
        add(
            track_key,
            Some("custom"),
            track_key,
            default_kind,
            default_unit,
        );
        chain
    }

    /// Emits a single event onto `track_key`, ensuring the track exists, clamping timestamp,
    /// and allocating sequence if not supplied.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit(
        &mut self,
        track_key: &str,
        kind: Kind,
        ts: u64,
        seq: Option<u64>,
        cause_seq: Option<u64>,
        agent: Option<String>,
        name: Option<String>,
        body: Option<tacho::event::Body>,
        out: &mut Vec<Event>,
    ) -> u64 {
        self.ensure_track(track_key, ts, out);
        let t_ts = self.clamp_ts(track_key, ts);
        let s = seq.unwrap_or_else(|| self.alloc_seq());
        out.push(Event {
            seq: s,
            ts_ns: t_ts,
            kind,
            track_key: track_key.to_string(),
            cause_seq,
            agent,
            custom_name: name,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event { body },
        });
        s
    }

    /// Restores a registered track from a persisted `TrackCreated` event.
    pub(crate) fn restore_track_created(&mut self, event: &Event) {
        if let Some(tacho::event::Body::TrackCreated(tc)) = &event.body.body {
            if let Some(ref key) = tc.key {
                self.registered_tracks.insert(key.clone());
                let chain = self.hierarchy_chain(key, TrackKind::Slices, None);
                for (k, parent_k, name, kind, unit) in chain {
                    if &k == key {
                        self.registry.register(
                            &k,
                            parent_k.as_deref(),
                            tc.name.as_deref().unwrap_or(&name),
                            None,
                            kind,
                            unit.as_deref(),
                        );
                    }
                }
            }
        }
    }

    /// Restores slice allocation state from Turn, Tool, Command, and File events.
    pub(crate) fn restore_slice_event(&mut self, event: &Event) {
        let segs: Vec<&str> = event.track_key.split('/').collect();
        let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
        let run_n = segs
            .get(1)
            .and_then(|s| s.strip_prefix("run:"))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        let Some(run) = self.runs.get_mut(&(agent_id.to_string(), run_n)) else {
            return;
        };

        match event.kind {
            Kind::Turn => {
                run.lane_allocator.allocate_begin(event.seq, Kind::Turn);
                run.turn_open = true;
                run.turn_begin_seq = Some(event.seq);
                run.current_turn_n += 1;
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    agent.turn_n = run.current_turn_n;
                }
            }
            Kind::TurnEnded => {
                let _ = run
                    .lane_allocator
                    .allocate_end(event.cause_seq.unwrap_or(0), Kind::Turn);
                run.turn_open = false;
                run.turn_begin_seq = None;
            }
            Kind::Command | Kind::ToolCall | Kind::FileAccess => {
                run.lane_allocator.allocate_begin(event.seq, event.kind);
                run.tool_calls_count += 1;
                let (tid_opt, argv, cmd_class, file_path, file_edit) = match &event.body.body {
                    Some(tacho::event::Body::Command(c)) => {
                        let class = c.class.and_then(from_proto_cmd_class);
                        (c.tool_use_id.clone(), c.argv.clone(), class, None, None)
                    }
                    Some(tacho::event::Body::FileAccess(f)) => {
                        (f.tool_use_id.clone(), None, None, f.path.clone(), None)
                    }
                    Some(tacho::event::Body::ToolCall(t)) => {
                        (t.tool_use_id.clone(), None, None, None, None)
                    }
                    _ => (None, None, None, None, None),
                };
                if let Some(tid) = tid_opt {
                    run.open_tools.insert(
                        tid.clone(),
                        OpenToolSlice {
                            slice_id: event.seq,
                            tool_use_id: tid,
                            tool_name: event.custom_name.clone().unwrap_or_default(),
                            kind: event.kind,
                            assigned_track: event.track_key.clone(),
                            argv,
                            command_class: cmd_class,
                            file_path,
                            file_edit,
                        },
                    );
                }
            }
            Kind::CommandEnded | Kind::ToolCallEnded | Kind::FileAccessEnded => {
                let begin_kind = event.kind.pairing().unwrap_or(event.kind);
                if let Some(cause) = event.cause_seq {
                    let _ = run.lane_allocator.allocate_end(cause, begin_kind);
                    run.open_tools.retain(|_, slice| slice.slice_id != cause);
                }
            }
            Kind::Wait => {
                run.lane_allocator.allocate_begin(event.seq, Kind::Wait);
            }
            Kind::WaitEnded => {
                if let Some(cause) = event.cause_seq {
                    let _ = run.lane_allocator.allocate_end(cause, Kind::Wait);
                }
            }
            Kind::FileEdited => {
                run.files_edited_count += 1;
            }
            _ => {}
        }
    }
}
