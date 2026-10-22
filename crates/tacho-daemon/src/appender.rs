//! Appender thread: single consumer of input channel and single writer to SQLite WAL log.
//!
//! Enforces WAL logging of all recorded events and lock-free publishing of LiveState
//! and TrackRegistry for concurrent readers.

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tacho_core::{Event, Input, LiveState, Recorder, SessionKey, TrackRegistry};

use crate::log::EventLog;

/// Test hooks to simulate a wedged or paused appender thread.
#[derive(Debug, Default)]
pub struct AppenderHooks {
    /// When true, causes the appender thread to block until cleared.
    pub wedged: AtomicBool,
}

/// Commands accepted by the appender loop.
#[allow(clippy::large_enum_variant)]
pub enum AppenderCommand {
    /// Ingest a parsed flight recorder input with arrival timestamp.
    Input(Input, u64),
    /// Ingest a batch of parsed flight recorder inputs with arrival timestamps.
    InputBatch(Vec<(Input, u64)>),
    /// Perform an immediate flush and sync of all pending events.
    Flush(Sender<()>),
    /// Orderly shutdown of the appender loop.
    Shutdown,
}

/// Active running session metadata for per-run process-tree and worktree sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub session: SessionKey,
    pub agent_id: String,
    pub run_track: String,
    pub pid: u32,
    pub cwd: Option<String>,
}

/// Handle to communicate with the running appender thread.
#[derive(Clone)]
pub struct AppenderHandle {
    sender: Sender<AppenderCommand>,
    live_state: Arc<ArcSwap<LiveState>>,
    published_registry: Arc<ArcSwap<TrackRegistry>>,
    active_runs: Arc<ArcSwap<Vec<ActiveRun>>>,
    hooks: Arc<AppenderHooks>,
    write_errors: Arc<AtomicUsize>,
}

impl AppenderHandle {
    /// Sends an input event into the flight recorder pipeline.
    #[allow(clippy::result_large_err)]
    pub fn send_input(
        &self,
        input: Input,
        received_at_ns: u64,
    ) -> Result<(), crossbeam_channel::SendError<AppenderCommand>> {
        self.sender
            .send(AppenderCommand::Input(input, received_at_ns))
    }

    /// Sends a batch of input events into the flight recorder pipeline.
    #[allow(clippy::result_large_err)]
    pub fn send_input_batch(
        &self,
        batch: Vec<(Input, u64)>,
    ) -> Result<(), crossbeam_channel::SendError<AppenderCommand>> {
        if batch.is_empty() {
            return Ok(());
        }
        self.sender.send(AppenderCommand::InputBatch(batch))
    }

    /// Flushes all pending batches in the appender thread.
    pub fn flush(&self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.sender.send(AppenderCommand::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    /// Signals the appender thread to terminate cleanly.
    pub fn shutdown(&self) {
        let _ = self.sender.send(AppenderCommand::Shutdown);
    }

    /// Returns a shared, atomic reference to the latest published LiveState.
    pub fn live_state(&self) -> Arc<LiveState> {
        self.live_state.load_full()
    }

    /// Returns a shared, atomic reference to the latest published TrackRegistry.
    pub fn registry(&self) -> Arc<TrackRegistry> {
        self.published_registry.load_full()
    }

    /// Returns the currently active runs and their root PIDs for process-tree sampling.
    pub fn active_runs(&self) -> Arc<Vec<ActiveRun>> {
        self.active_runs.load_full()
    }

    /// Access test hooks (e.g. for wedged appender tests).
    pub fn hooks(&self) -> &Arc<AppenderHooks> {
        &self.hooks
    }

    /// Returns the cumulative number of SQLite WAL write errors encountered.
    pub fn write_errors(&self) -> usize {
        self.write_errors.load(Ordering::Relaxed)
    }
}

/// Spawns the dedicated appender thread and returns its handle.
pub fn spawn_appender(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    let on_stall = std::env::var("TACHO_ON_STALL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    spawn_appender_with_hook(recorder, log, channel_capacity, on_stall)
}

/// Spawns the dedicated appender thread with an optional `on_stall` shell hook.
pub fn spawn_appender_with_hook(
    mut recorder: Recorder,
    mut log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    let (sender, receiver) = crossbeam_channel::bounded(channel_capacity);

    let initial_state = Arc::new(recorder.live_state().clone());
    let initial_reg = Arc::new(recorder.registry().clone());

    let mut initial_runs_map = HashMap::new();
    for (agent_id, a) in &initial_state.agents {
        if a.state != "Exited" && a.cwd.is_some() {
            let sess = SessionKey::new(&a.provider, &a.session_id);
            initial_runs_map.insert(
                sess.clone(),
                ActiveRun {
                    session: sess,
                    agent_id: agent_id.clone(),
                    run_track: format!("agent:{}/run:{}", agent_id, a.run_n.max(1)),
                    pid: 0,
                    cwd: a.cwd.clone(),
                },
            );
        }
    }
    let active_runs = Arc::new(ArcSwap::new(Arc::new(
        initial_runs_map.values().cloned().collect(),
    )));

    let live_state = Arc::new(ArcSwap::new(initial_state));
    let published_registry = Arc::new(ArcSwap::new(initial_reg));
    let hooks = Arc::new(AppenderHooks::default());
    let write_errors = Arc::new(AtomicUsize::new(0));

    let handle = AppenderHandle {
        sender,
        live_state: live_state.clone(),
        published_registry: published_registry.clone(),
        active_runs: active_runs.clone(),
        hooks: hooks.clone(),
        write_errors: write_errors.clone(),
    };

    let join_handle = std::thread::Builder::new()
        .name("tacho-appender".to_string())
        .spawn(move || {
            run_appender_loop(
                receiver,
                &mut recorder,
                &mut log,
                live_state,
                published_registry,
                active_runs,
                initial_runs_map,
                hooks,
                write_errors,
                on_stall,
            );
        })
        .expect("failed to spawn appender thread");

    (handle, join_handle)
}

/// Main execution loop of the sole appender thread.
#[allow(clippy::too_many_arguments)]
fn run_appender_loop(
    receiver: Receiver<AppenderCommand>,
    recorder: &mut Recorder,
    log: &mut EventLog,
    live_state: Arc<ArcSwap<LiveState>>,
    published_registry: Arc<ArcSwap<TrackRegistry>>,
    active_runs: Arc<ArcSwap<Vec<ActiveRun>>>,
    mut active_runs_map: HashMap<SessionKey, ActiveRun>,
    hooks: Arc<AppenderHooks>,
    write_errors: Arc<AtomicUsize>,
    on_stall: Option<String>,
) {
    let mut batch_events = Vec::with_capacity(256);
    let mut flush_notifiers = Vec::new();
    let mut last_live_publish = Instant::now();
    let mut last_published_reg_len = recorder.registry().len();
    let mut last_tick = Instant::now();
    let tick_interval = Duration::from_millis(200);

    let commit_interval = Duration::from_millis(50);
    const MAX_BATCH_SIZE: usize = 256;

    'outer: loop {
        // Test hook: block appender if wedged is requested
        while hooks.wedged.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
        }

        // 1. Wait for the first item with timeout
        let first_cmd = match receiver.recv_timeout(commit_interval) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                // Time-based periodic maintenance (detectors tick)
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                recorder.tick(now_ns, &mut batch_events);
                last_tick = Instant::now();
                if !batch_events.is_empty() {
                    commit_batch(
                        &mut batch_events,
                        &mut flush_notifiers,
                        recorder,
                        log,
                        &live_state,
                        &published_registry,
                        &mut last_live_publish,
                        &mut last_published_reg_len,
                        &write_errors,
                        on_stall.as_deref(),
                    );
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break 'outer,
        };

        let batch_start = Instant::now();

        // Process first command
        match first_cmd {
            AppenderCommand::Input(input, rx_ts) => {
                apply_input_with_tracking(
                    input,
                    rx_ts,
                    recorder,
                    &mut batch_events,
                    &mut active_runs_map,
                    &active_runs,
                );
            }
            AppenderCommand::InputBatch(inputs) => {
                for (input, rx_ts) in inputs {
                    apply_input_with_tracking(
                        input,
                        rx_ts,
                        recorder,
                        &mut batch_events,
                        &mut active_runs_map,
                        &active_runs,
                    );
                    if batch_events.len() >= MAX_BATCH_SIZE {
                        commit_batch(
                            &mut batch_events,
                            &mut flush_notifiers,
                            recorder,
                            log,
                            &live_state,
                            &published_registry,
                            &mut last_live_publish,
                            &mut last_published_reg_len,
                            &write_errors,
                            on_stall.as_deref(),
                        );
                    }
                }
            }
            AppenderCommand::Flush(tx) => flush_notifiers.push(tx),
            AppenderCommand::Shutdown => break 'outer,
        }

        // 2. Drain channel non-blocking up to MAX_BATCH_SIZE or commit_interval
        while batch_events.len() < MAX_BATCH_SIZE && batch_start.elapsed() < commit_interval {
            match receiver.try_recv() {
                Ok(AppenderCommand::Input(input, rx_ts)) => {
                    apply_input_with_tracking(
                        input,
                        rx_ts,
                        recorder,
                        &mut batch_events,
                        &mut active_runs_map,
                        &active_runs,
                    );
                }
                Ok(AppenderCommand::InputBatch(inputs)) => {
                    for (input, rx_ts) in inputs {
                        apply_input_with_tracking(
                            input,
                            rx_ts,
                            recorder,
                            &mut batch_events,
                            &mut active_runs_map,
                            &active_runs,
                        );
                        if batch_events.len() >= MAX_BATCH_SIZE {
                            commit_batch(
                                &mut batch_events,
                                &mut flush_notifiers,
                                recorder,
                                log,
                                &live_state,
                                &published_registry,
                                &mut last_live_publish,
                                &mut last_published_reg_len,
                                &write_errors,
                                on_stall.as_deref(),
                            );
                        }
                    }
                }
                Ok(AppenderCommand::Flush(tx)) => flush_notifiers.push(tx),
                Ok(AppenderCommand::Shutdown) => break 'outer,
                Err(_) => break, // Channel empty
            }
        }

        // 3. Periodic detector tick under continuous load
        if last_tick.elapsed() >= tick_interval {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            recorder.tick(now_ns, &mut batch_events);
            last_tick = Instant::now();
        }

        // 4. Commit batch if events or flushes are pending
        if !batch_events.is_empty() || !flush_notifiers.is_empty() {
            commit_batch(
                &mut batch_events,
                &mut flush_notifiers,
                recorder,
                log,
                &live_state,
                &published_registry,
                &mut last_live_publish,
                &mut last_published_reg_len,
                &write_errors,
                on_stall.as_deref(),
            );
        }
    }

    // Final clean drain on shutdown
    while let Ok(cmd) = receiver.try_recv() {
        match cmd {
            AppenderCommand::Input(input, rx_ts) => {
                apply_input_with_tracking(
                    input,
                    rx_ts,
                    recorder,
                    &mut batch_events,
                    &mut active_runs_map,
                    &active_runs,
                );
            }
            AppenderCommand::InputBatch(inputs) => {
                for (input, rx_ts) in inputs {
                    apply_input_with_tracking(
                        input,
                        rx_ts,
                        recorder,
                        &mut batch_events,
                        &mut active_runs_map,
                        &active_runs,
                    );
                }
            }
            AppenderCommand::Flush(tx) => flush_notifiers.push(tx),
            AppenderCommand::Shutdown => {}
        }
    }

    if !batch_events.is_empty() || !flush_notifiers.is_empty() {
        commit_batch(
            &mut batch_events,
            &mut flush_notifiers,
            recorder,
            log,
            &live_state,
            &published_registry,
            &mut last_live_publish,
            &mut last_published_reg_len,
            &write_errors,
            on_stall.as_deref(),
        );
    }
}

/// Helper to apply inputs and track active session runs for process-tree and worktree sampling.
fn apply_input_with_tracking(
    input: Input,
    rx_ts: u64,
    recorder: &mut Recorder,
    batch_events: &mut Vec<Event>,
    active_runs_map: &mut HashMap<SessionKey, ActiveRun>,
    active_runs: &Arc<ArcSwap<Vec<ActiveRun>>>,
) {
    let mut run_start_info = None;
    let mut run_ended_session = None;

    match &input {
        Input::RunStart {
            provider,
            session_id,
            agent,
            pid,
            cwd,
            ..
        } if pid.is_some() || cwd.is_some() => {
            let session_key = SessionKey::new(provider, session_id);
            let agent_id = agent.clone().unwrap_or_else(|| provider.clone());
            run_start_info = Some((session_key, agent_id, pid.unwrap_or(0), cwd.clone()));
        }
        Input::RunEnd { session, .. } => {
            run_ended_session = Some(session.clone());
        }
        _ => {}
    }

    recorder.apply(input, rx_ts, batch_events);

    let mut changed = false;
    if let Some((session_key, agent_id, pid, cwd)) = run_start_info {
        let run_n = recorder
            .live_state()
            .agents
            .get(&agent_id)
            .map(|a| a.run_n)
            .unwrap_or(1);
        let run_track = format!("agent:{}/run:{}", agent_id, run_n);
        active_runs_map.insert(
            session_key.clone(),
            ActiveRun {
                session: session_key,
                agent_id,
                run_track,
                pid,
                cwd,
            },
        );
        changed = true;
    }
    if let Some(session) = run_ended_session {
        if active_runs_map.remove(&session).is_some() {
            changed = true;
        }
    }
    if changed {
        active_runs.store(Arc::new(active_runs_map.values().cloned().collect()));
    }
}

/// Commits a batch of events: SQLite log row first, then publish state, then fire stall hooks.
#[allow(clippy::too_many_arguments)]
fn commit_batch(
    batch_events: &mut Vec<Event>,
    flush_notifiers: &mut Vec<Sender<()>>,
    recorder: &Recorder,
    log: &mut EventLog,
    live_state: &Arc<ArcSwap<LiveState>>,
    published_registry: &Arc<ArcSwap<TrackRegistry>>,
    last_live_publish: &mut Instant,
    last_published_reg_len: &mut usize,
    write_errors: &AtomicUsize,
    on_stall: Option<&str>,
) {
    if !batch_events.is_empty() {
        // SQLite write: insert all events (sampled machine/run counters logged at sample interval)
        if let Err(e) = log.insert_batch(batch_events.iter()) {
            write_errors.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[tacho-daemon] failed to insert batch into SQLite WAL (graceful degradation): {}",
                e
            );
        }

        // Fire `on_stall` effect hook asynchronously for any newly committed Stall event
        if let Some(cmd_str) = on_stall {
            for ev in batch_events.iter() {
                if ev.kind == tacho_core::Kind::Stall {
                    if let Some(tacho_proto::tacho::event::Body::Stall(ref st)) = ev.body.body {
                        let stall_kind = match st
                            .kind
                            .and_then(|k| tacho_proto::tacho::StallKind::try_from(k).ok())
                        {
                            Some(tacho_proto::tacho::StallKind::Silent) => "SILENT",
                            Some(tacho_proto::tacho::StallKind::Loop) => "LOOP",
                            Some(tacho_proto::tacho::StallKind::Spin) => "SPIN",
                            Some(tacho_proto::tacho::StallKind::Burn) => "BURN",
                            Some(tacho_proto::tacho::StallKind::Failing) => "FAILING",
                            Some(tacho_proto::tacho::StallKind::OffBrief) => "OFF_BRIEF",
                            Some(tacho_proto::tacho::StallKind::StallClaim) => "CLAIM",
                            _ => "STALL",
                        };
                        let agent = ev.agent.clone().unwrap_or_default();
                        let evidence = st.evidence.clone().unwrap_or_default();
                        let seq_str = ev.seq.to_string();
                        let cmd_owned = cmd_str.to_string();
                        let _ = std::thread::Builder::new()
                            .name("tacho-on-stall".to_string())
                            .spawn(move || {
                                let _ = std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(&cmd_owned)
                                    .env("TACHO_AGENT", &agent)
                                    .env("TACHO_STALL_KIND", stall_kind)
                                    .env("TACHO_STALL_REASON", &evidence)
                                    .env("TACHO_STALL_EVIDENCE", &evidence)
                                    .env("TACHO_SEQ", &seq_str)
                                    .status();
                            });
                    }
                }
            }
        }

        // Publish TrackRegistry immediately if new tracks were registered
        let current_reg_len = recorder.registry().len();
        if current_reg_len != *last_published_reg_len || !flush_notifiers.is_empty() {
            published_registry.store(Arc::new(recorder.registry().clone()));
            *last_published_reg_len = current_reg_len;
        }

        // Publish updated LiveState on every committed batch
        live_state.store(Arc::new(recorder.live_state().clone()));
        *last_live_publish = Instant::now();

        batch_events.clear();
    }

    // Notify any flush waiters
    for notifier in flush_notifiers.drain(..) {
        let _ = notifier.send(());
    }
}
