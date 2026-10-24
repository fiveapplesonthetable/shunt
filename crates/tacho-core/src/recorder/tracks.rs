//! Track registration and hierarchy resolution for the flight recorder.

use crate::classify::CommandClass;
use crate::event::Event;
use crate::input::FileEditKind;
use crate::kind::Kind;
use crate::track::TrackKind;
use tacho_proto::tacho;

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

pub type TrackHierarchyEntry = (
    String,
    Option<String>,
    String,
    Option<String>,
    TrackKind,
    Option<String>,
);

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
        for (key, parent_key, name, desc, kind, unit) in parts {
            if self.registered_tracks.insert(key.clone()) {
                let entry = self.registry.register(
                    &key,
                    parent_key.as_deref(),
                    &name,
                    desc.as_deref(),
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
                    custom_name: Some(format!("TrackCreated: {}", key)),
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
    pub fn hierarchy_chain(
        &self,
        track_key: &str,
        default_kind: TrackKind,
        default_unit: Option<&str>,
    ) -> Vec<TrackHierarchyEntry> {
        let mut chain = Vec::new();
        let mut add = |k: &str,
                       p: Option<&str>,
                       n: &str,
                       desc: Option<&str>,
                       kd: TrackKind,
                       u: Option<&str>| {
            chain.push((
                k.to_string(),
                p.map(ToString::to_string),
                n.to_string(),
                desc.map(ToString::to_string),
                kd,
                u.map(ToString::to_string),
            ));
        };

        add(
            "tacho",
            None,
            "tacho",
            Some("Tachograph fleet flight recorder root track and cross-agent message bus"),
            TrackKind::Group,
            None,
        );
        add(
            "daemon",
            Some("tacho"),
            "daemon",
            Some("Recorder daemon lifecycle, snapshots, clock steps, and track creation events"),
            TrackKind::Slices,
            None,
        );

        if track_key == "tacho" || track_key == "daemon" {
            return chain;
        }

        if track_key == "tacho/messages" || track_key == "messages" {
            add(
                track_key,
                Some("tacho"),
                "messages",
                Some("Cross-agent message delivery spans"),
                TrackKind::Slices,
                None,
            );
            return chain;
        }

        if track_key == "daemon/counters" {
            add(
                "daemon/counters",
                Some("daemon"),
                "counters",
                Some("Daemon internal counters"),
                TrackKind::Counter,
                Some("count"),
            );
            return chain;
        }

        if track_key.starts_with("machine")
            || track_key.starts_with("host:")
            || track_key.starts_with("host/")
        {
            add(
                "machine",
                Some("tacho"),
                "machine",
                Some("Host machine CPU, memory, load average, and build/test concurrency counters"),
                TrackKind::Group,
                None,
            );
            if track_key != "machine" {
                let name = track_key
                    .strip_prefix("machine/")
                    .or_else(|| track_key.strip_prefix("host:"))
                    .or_else(|| track_key.strip_prefix("host/"))
                    .unwrap_or(track_key);
                let (desc, unit) = match name {
                    "cpu_busy" => (
                        Some("Host-wide CPU busy percentage across all cores"),
                        Some("percent"),
                    ),
                    "load1" => (
                        Some("Host 1-minute load average x 1000 (millicpus)"),
                        Some("load x 1000"),
                    ),
                    "mem_used" => (
                        Some("Host physical memory in use (MemTotal - MemAvailable)"),
                        Some("bytes"),
                    ),
                    "mem_avail" => (
                        Some("Host available memory for new workloads without swapping"),
                        Some("bytes"),
                    ),
                    "mem_cached" => (
                        Some("Host page cache + slab reclaimable memory"),
                        Some("bytes"),
                    ),
                    "swap_used" => (Some("Host swap space currently in use"), Some("bytes")),
                    "active_builds" => (
                        Some("Number of concurrent agent Build commands executing"),
                        Some("builds"),
                    ),
                    "active_tests" => (
                        Some("Number of concurrent agent Test commands executing"),
                        Some("tests"),
                    ),
                    _ => (None, default_unit),
                };
                add(
                    track_key,
                    Some("machine"),
                    name,
                    desc,
                    TrackKind::Counter,
                    unit,
                );
            }
            return chain;
        }

        if track_key.starts_with("providers") || track_key.starts_with("provider:") {
            add(
                "providers",
                Some("tacho"),
                "providers",
                Some("Model providers (API errors, rate limits, cumulative token usage)"),
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
                Some("Provider rate-limit and error events"),
                TrackKind::Slices,
                None,
            );
            if parts.len() > 1 {
                let unit = match parts[1] {
                    "tokens_out" | "tokens_in" | "tokens_cached" => Some("tokens"),
                    "cost" => Some("usd_micros"),
                    _ => default_unit,
                };
                add(
                    track_key,
                    Some(prov_track),
                    parts[1],
                    Some("Cumulative provider counter"),
                    TrackKind::Counter,
                    unit,
                );
            }
            return chain;
        }

        if track_key.starts_with("tasks") || track_key.starts_with("task:") {
            add(
                "tasks",
                Some("tacho"),
                "tasks",
                Some("Tracked engineering tasks, state transitions, and code review findings"),
                TrackKind::Group,
                None,
            );
            let sub = track_key.strip_prefix("tasks/").unwrap_or(track_key);
            let parts: Vec<&str> = sub.split('/').collect();
            let task_track = parts[0];
            add(
                task_track,
                Some("tasks"),
                task_track,
                Some("Task queue and assignment lifecycle"),
                TrackKind::Group,
                None,
            );
            if parts.len() > 1 {
                let (kind, desc) = match parts[1] {
                    "state" => (
                        TrackKind::State,
                        Some("Task status (Queued -> InProgress -> Review -> Done/Failed)"),
                    ),
                    "review" => (
                        TrackKind::Slices,
                        Some("Automated and peer code review slices and findings"),
                    ),
                    _ => (TrackKind::Slices, None),
                };
                add(track_key, Some(task_track), parts[1], desc, kind, None);
            }
            return chain;
        }

        if track_key == "fleet"
            || track_key.starts_with("fleet/")
            || track_key == "batches"
            || track_key.starts_with("batch:")
            || track_key == "trains"
            || track_key.starts_with("train:")
        {
            add(
                "fleet",
                Some("tacho"),
                "fleet",
                Some("Orchestrator ticks, verification batches, and merge trains"),
                TrackKind::Group,
                None,
            );
            if track_key == "fleet/ticks" {
                add(
                    "fleet/ticks",
                    Some("fleet"),
                    "ticks",
                    Some("Orchestrator tick pass duration slices"),
                    TrackKind::Slices,
                    None,
                );
            } else if track_key.starts_with("fleet/") {
                let sub = track_key.strip_prefix("fleet/").unwrap_or(track_key);
                add(
                    track_key,
                    Some("fleet"),
                    sub,
                    None,
                    default_kind,
                    default_unit,
                );
            } else if track_key == "batches" || track_key.starts_with("batch:") {
                add(
                    "batches",
                    Some("fleet"),
                    "batches",
                    Some("Speculative verification batches and binary bisection probes"),
                    TrackKind::Group,
                    None,
                );
                if track_key != "batches" {
                    add(
                        track_key,
                        Some("batches"),
                        track_key,
                        Some("Verification batch execution and bisection probe spans"),
                        TrackKind::Slices,
                        None,
                    );
                }
            } else if track_key == "trains" || track_key.starts_with("train:") {
                add(
                    "trains",
                    Some("fleet"),
                    "trains",
                    Some("Speculative merge trains and combined release trains"),
                    TrackKind::Group,
                    None,
                );
                if track_key != "trains" {
                    add(
                        track_key,
                        Some("trains"),
                        track_key,
                        Some("Merge train state band (open -> landed / aborted / mismatch)"),
                        TrackKind::State,
                        None,
                    );
                }
            }
            return chain;
        }

        if track_key.starts_with("custom") || track_key.starts_with("custom:") {
            add(
                "custom",
                Some("tacho"),
                "custom",
                Some("User-defined custom tracks"),
                TrackKind::Group,
                None,
            );
            if track_key != "custom" {
                add(
                    track_key,
                    Some("custom"),
                    track_key,
                    None,
                    default_kind,
                    default_unit,
                );
            }
            return chain;
        }

        if track_key.starts_with("agents") || track_key.starts_with("agent:") {
            add(
                "agents",
                Some("tacho"),
                "agents",
                Some("All monitored coding agents in the fleet"),
                TrackKind::Group,
                None,
            );
            let segs: Vec<&str> = track_key.split('/').collect();
            let agent_key = segs[0];
            add(
                agent_key,
                Some("agents"),
                agent_key,
                Some("Agent root track (stall detections and lifecycle alerts)"),
                TrackKind::Group,
                None,
            );

            if segs.len() > 1 {
                if segs[1] == "state" {
                    let st = format!("{}/state", agent_key);
                    add(
                        &st,
                        Some(agent_key),
                        "state",
                        Some("Agent state (Idle, Thinking, Tool, Waiting, Stalled, Exited)"),
                        TrackKind::State,
                        None,
                    );
                } else if segs[1] == "stalls" {
                    let st = format!("{}/stalls", agent_key);
                    add(
                        &st,
                        Some(agent_key),
                        "stalls",
                        Some("Active stall detector intervals (LOOP, FAILING, SILENT, SPIN, BURN, OFF_BRIEF)"),
                        TrackKind::Slices,
                        None,
                    );
                } else if segs[1] == "cpu" || segs[1] == "rss" {
                    let (unit, desc) = if segs[1] == "cpu" {
                        (
                            "us",
                            "Cumulative CPU microseconds of the whole agent tree incl. exited and daemonized processes when cgroup accounting is on",
                        )
                    } else {
                        (
                            "bytes",
                            "Total Resident Set Size (RSS) across all live member processes",
                        )
                    };
                    add(
                        track_key,
                        Some(agent_key),
                        segs[1],
                        Some(desc),
                        TrackKind::Counter,
                        Some(unit),
                    );
                } else if segs[1].starts_with("run:") {
                    let run_key = format!("{}/{}", agent_key, segs[1]);
                    add(
                        &run_key,
                        Some(agent_key),
                        segs[1],
                        Some("Agent session run span"),
                        TrackKind::Slices,
                        None,
                    );

                    if segs.len() > 2 {
                        let sub_name = segs[2];
                        if sub_name == "procs" {
                            let procs_key = format!("{}/procs", run_key);
                            add(
                                &procs_key,
                                Some(&run_key),
                                "procs",
                                Some("Processes spawned by this agent (one slot per concurrently running process)"),
                                TrackKind::Group,
                                None,
                            );
                            if segs.len() > 3 {
                                let slot_part = segs[3];
                                let slot_name = if let Some(k) = slot_part.strip_prefix("slot:") {
                                    format!("slot {}", k)
                                } else {
                                    slot_part.to_string()
                                };
                                add(
                                    track_key,
                                    Some(&procs_key),
                                    &slot_name,
                                    Some(
                                        "Process execution slot for concurrently running processes",
                                    ),
                                    TrackKind::Slices,
                                    None,
                                );
                            }
                        } else {
                            let (kind, unit, desc) = match sub_name {
                                "turns" => (
                                    TrackKind::Slices,
                                    None,
                                    Some("Model turns, prompts, sequential tools, and worktree commits"),
                                ),
                                "waits" => (
                                    TrackKind::Slices,
                                    None,
                                    Some("Blocking waits on subagents, checks, merge queues, or humans"),
                                ),
                                "worktree" => (
                                    TrackKind::Slices,
                                    None,
                                    Some("Git worktree HEAD commit and branch movement spans"),
                                ),
                                "overflow" => (
                                    TrackKind::Slices,
                                    None,
                                    Some("Overflow tool execution lane (>8 concurrent tools)"),
                                ),
                                "tokens_in" => (
                                    TrackKind::Counter,
                                    Some("tokens"),
                                    Some("Cumulative input tokens consumed in this run"),
                                ),
                                "tokens_out" => (
                                    TrackKind::Counter,
                                    Some("tokens"),
                                    Some("Cumulative output tokens generated in this run"),
                                ),
                                "tokens_cached" => (
                                    TrackKind::Counter,
                                    Some("tokens"),
                                    Some("Cumulative prompt cache hit tokens in this run"),
                                ),
                                "cost" => (
                                    TrackKind::Counter,
                                    Some("micro-USD"),
                                    Some("Cumulative API cost in micro-USD ($1e-6)"),
                                ),
                                "files_edited" => (
                                    TrackKind::Counter,
                                    Some("files"),
                                    Some("Cumulative files edited in this run"),
                                ),
                                "commits" => (
                                    TrackKind::Counter,
                                    Some("commits"),
                                    Some("Cumulative git commits in this run"),
                                ),
                                "cpu" => (
                                    TrackKind::Counter,
                                    Some("us"),
                                    Some("Cumulative CPU microseconds of the whole agent tree incl. exited and daemonized processes when cgroup accounting is on"),
                                ),
                                "rss" => (
                                    TrackKind::Counter,
                                    Some("bytes"),
                                    Some("Total Resident Set Size (RSS) across all live member processes"),
                                ),
                                "proc_count" => (
                                    TrackKind::Counter,
                                    Some("procs"),
                                    Some("Number of live processes in the agent process tree"),
                                ),
                                "cgroup_mem" => (
                                    TrackKind::Counter,
                                    Some("bytes"),
                                    Some("Current cgroup memory usage (memory.current, includes page cache)"),
                                ),
                                "cgroup_mem_peak" => (
                                    TrackKind::Counter,
                                    Some("bytes"),
                                    Some("Peak cgroup memory usage (memory.peak, includes page cache)"),
                                ),
                                s if s.starts_with("lane:") => (
                                    TrackKind::Slices,
                                    None,
                                    Some("Parallel tool/command execution lane"),
                                ),
                                _ => (default_kind, default_unit, None),
                            };
                            add(track_key, Some(&run_key), sub_name, desc, kind, unit);
                        }
                    }
                } else {
                    add(
                        track_key,
                        Some(agent_key),
                        segs[1],
                        None,
                        default_kind,
                        default_unit,
                    );
                }
            }
            return chain;
        }

        // Fallback for ANY unknown track key: group under `custom`
        add(
            "custom",
            Some("tacho"),
            "custom",
            Some("User-defined custom tracks"),
            TrackKind::Group,
            None,
        );
        add(
            track_key,
            Some("custom"),
            track_key,
            None,
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
}
