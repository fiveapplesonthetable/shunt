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
use tacho_core::{AgentRole, Event, Input, LiveState, Recorder, SessionKey, TrackRegistry};

use crate::log::EventLog;

/// Test hooks to simulate a wedged or paused appender thread.
#[derive(Debug, Default)]
pub struct AppenderHooks {
    /// When true, causes the appender thread to block until cleared.
    pub wedged: AtomicBool,
    pub condvar: std::sync::Condvar,
    pub mutex: std::sync::Mutex<()>,
}

impl AppenderHooks {
    pub fn set_wedged(&self, wedged: bool) {
        let _guard = self.mutex.lock().unwrap();
        self.wedged.store(wedged, Ordering::Release);
        if !wedged {
            self.condvar.notify_all();
        }
    }
}

/// Commands accepted by the appender loop.
pub enum AppenderCommand {
    /// Ingest a parsed flight recorder input with arrival timestamp.
    Input(Box<Input>, u64),
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
    pub role: AgentRole,
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
    state_notify: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
}

impl AppenderHandle {
    /// Waits for `LiveState` to update beyond `last_seq` within `timeout`, returning the new sequence.
    pub fn wait_state_change(&self, last_seq: u64, timeout: Duration) -> u64 {
        let (lock, cvar) = &*self.state_notify;
        let seq = lock.lock().unwrap();
        if *seq != last_seq {
            return *seq;
        }
        let (res, _) = cvar.wait_timeout(seq, timeout).unwrap();
        *res
    }

    /// Returns current state sequence number.
    pub fn state_seq(&self) -> u64 {
        let (lock, _) = &*self.state_notify;
        *lock.lock().unwrap()
    }
    /// Sends an input event into the flight recorder pipeline with a bounded timeout
    /// so high-throughput streams get backpressure without hanging indefinitely if wedged.
    #[allow(clippy::result_large_err)]
    pub fn send_input(
        &self,
        input: Input,
        received_at_ns: u64,
    ) -> Result<(), crossbeam_channel::SendTimeoutError<AppenderCommand>> {
        self.sender.send_timeout(
            AppenderCommand::Input(Box::new(input), received_at_ns),
            Duration::from_secs(2),
        )
    }

    /// Sends a batch of input events into the flight recorder pipeline with a bounded timeout
    /// so high-throughput streams get backpressure without hanging indefinitely if wedged.
    #[allow(clippy::result_large_err)]
    pub fn send_input_batch(
        &self,
        batch: Vec<(Input, u64)>,
    ) -> Result<(), crossbeam_channel::SendTimeoutError<AppenderCommand>> {
        if batch.is_empty() {
            return Ok(());
        }
        self.sender
            .send_timeout(AppenderCommand::InputBatch(batch), Duration::from_secs(2))
    }

    /// Flushes all pending batches in the appender thread.
    pub fn flush(&self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self
            .sender
            .send_timeout(AppenderCommand::Flush(tx), Duration::from_secs(2))
            .is_ok()
        {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    /// Signals the appender thread to terminate cleanly.
    pub fn shutdown(&self) {
        let _ = self
            .sender
            .send_timeout(AppenderCommand::Shutdown, Duration::from_millis(200));
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

/// Formats the track key for an agent run.
fn run_track_key(agent: &str, run_n: u32) -> String {
    format!("agent:{}/run:{}", agent, run_n.max(1))
}

/// Payload describing a newly committed stall event passed to the on_stall hook worker.
struct StallJob {
    agent: String,
    stall_kind: &'static str,
    reason: String,
    seq: u64,
    ts_ns: u64,
}

/// Dedicated sender for the bounded on_stall effect hook worker.
struct StallHookSender {
    tx: std::sync::mpsc::SyncSender<StallJob>,
    dropped_count: usize,
    logged_drop: bool,
}

impl StallHookSender {
    fn send(&mut self, job: StallJob) {
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped_count += 1;
                if !self.logged_drop {
                    self.logged_drop = true;
                    eprintln!(
                        "[tacho-daemon] on_stall hook queue full (64 items); dropping stall hooks"
                    );
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Runs the dedicated worker thread servicing on_stall hook commands.
fn run_stall_worker(
    receiver: std::sync::mpsc::Receiver<StallJob>,
    cmd_str: String,
    timeout: Duration,
) {
    use std::os::unix::process::CommandExt;

    while let Ok(job) = receiver.recv() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&cmd_str);
        cmd.env("TACHO_AGENT", &job.agent);
        cmd.env("TACHO_STALL_KIND", job.stall_kind);
        cmd.env("TACHO_STALL_REASON", &job.reason);
        cmd.env("TACHO_SEQ", job.seq.to_string());
        cmd.env("TACHO_TS_NS", job.ts_ns.to_string());
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[tacho-daemon] failed to spawn on_stall hook: {}", e);
                continue;
            }
        };

        let child_pid = child.id() as libc::pid_t;
        let mut exited = false;

        let pid_fd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid as libc::c_int, 0) as i32 };
        if pid_fd >= 0 {
            let mut pfd = libc::pollfd {
                fd: pid_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let poll_rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            unsafe {
                let _ = libc::close(pid_fd);
            }
            if poll_rc > 0 {
                exited = true;
                let _ = child.wait();
            }
        } else {
            // pidfd not supported: poll non-blocking with child.try_wait() up to timeout
            let start = Instant::now();
            while start.elapsed() < timeout {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        exited = true;
                        break;
                    }
                    Ok(None) => {
                        std::thread::yield_now();
                    }
                    Err(e) => {
                        eprintln!("[tacho-daemon] on_stall wait error: {}", e);
                        exited = true;
                        break;
                    }
                }
            }
        }

        if !exited {
            // Process timed out: kill the process group and reap the child process.
            unsafe {
                libc::kill(-child_pid, libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

/// Spawns the dedicated appender thread and returns its handle.
pub fn spawn_appender(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    spawn_appender_with_hook_timeout(
        recorder,
        log,
        channel_capacity,
        None,
        Duration::from_secs(60),
    )
}

/// Spawns the dedicated appender thread with an optional `on_stall` shell hook.
pub fn spawn_appender_with_hook(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    spawn_appender_with_hook_timeout(
        recorder,
        log,
        channel_capacity,
        on_stall,
        Duration::from_secs(60),
    )
}

/// Spawns the dedicated appender thread with an optional `on_stall` shell hook and custom timeout.
pub fn spawn_appender_with_hook_timeout(
    mut recorder: Recorder,
    mut log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
    on_stall_timeout: Duration,
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
                    run_track: run_track_key(agent_id, a.run_n),
                    pid: 0,
                    cwd: a.cwd.clone(),
                    role: AgentRole::infer(agent_id, a.task.as_deref()),
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
    let state_notify = Arc::new((std::sync::Mutex::new(0u64), std::sync::Condvar::new()));

    let handle = AppenderHandle {
        sender,
        live_state: live_state.clone(),
        published_registry: published_registry.clone(),
        active_runs: active_runs.clone(),
        hooks: hooks.clone(),
        write_errors: write_errors.clone(),
        state_notify: state_notify.clone(),
    };

    let (stall_hook_sender, stall_worker_handle) = match on_stall {
        Some(cmd_str) if !cmd_str.trim().is_empty() => {
            let (tx, rx) = std::sync::mpsc::sync_channel::<StallJob>(64);
            let worker = std::thread::Builder::new()
                .name("tacho-on-stall".to_string())
                .spawn(move || {
                    run_stall_worker(rx, cmd_str, on_stall_timeout);
                })
                .expect("failed to spawn on-stall worker thread");
            (
                Some(StallHookSender {
                    tx,
                    dropped_count: 0,
                    logged_drop: false,
                }),
                Some(worker),
            )
        }
        _ => (None, None),
    };

    let state_notify_loop = state_notify.clone();
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
                stall_hook_sender,
                stall_worker_handle,
                state_notify_loop,
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
    mut stall_hook_sender: Option<StallHookSender>,
    stall_worker_handle: Option<std::thread::JoinHandle<()>>,
    state_notify: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
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
        // Chaos injection hook: intentionally wedges the appender thread to simulate SQLite lock contention
        // and verify daemon degrades cleanly without blocking hook clients (test14_wedged_appender).
        if hooks.wedged.load(Ordering::Acquire) {
            let guard = hooks.mutex.lock().unwrap();
            if let Ok(guard) = hooks
                .condvar
                .wait_while(guard, |_| hooks.wedged.load(Ordering::Acquire))
            {
                drop(guard);
            }
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
                        &mut stall_hook_sender,
                        &state_notify,
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
                    *input,
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
                            &mut stall_hook_sender,
                            &state_notify,
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
                        *input,
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
                                &mut stall_hook_sender,
                                &state_notify,
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
                &mut stall_hook_sender,
                &state_notify,
            );
        }
    }

    // Final clean drain on shutdown
    while let Ok(cmd) = receiver.try_recv() {
        match cmd {
            AppenderCommand::Input(input, rx_ts) => {
                apply_input_with_tracking(
                    *input,
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
            &mut stall_hook_sender,
            &state_notify,
        );
    }

    drop(stall_hook_sender);
    if let Some(worker) = stall_worker_handle {
        let _ = worker.join();
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
            role,
            task,
            ..
        } if pid.is_some() || cwd.is_some() => {
            let session_key = SessionKey::new(provider, session_id);
            let agent_id = agent.clone().unwrap_or_else(|| provider.clone());
            let agent_role = role.unwrap_or_else(|| AgentRole::infer(&agent_id, task.as_deref()));
            run_start_info = Some((
                session_key,
                agent_id,
                pid.unwrap_or(0),
                cwd.clone(),
                agent_role,
            ));
        }
        Input::RunEnd { session, .. } => {
            run_ended_session = Some(session.clone());
        }
        _ => {}
    }

    recorder.apply(input, rx_ts, batch_events);

    let mut changed = false;
    if let Some((session_key, agent_id, pid, cwd, role)) = run_start_info {
        let run_n = recorder
            .live_state()
            .agents
            .get(&agent_id)
            .map(|a| a.run_n)
            .unwrap_or(1);
        let run_track = run_track_key(&agent_id, run_n);
        active_runs_map.insert(
            session_key.clone(),
            ActiveRun {
                session: session_key,
                agent_id,
                run_track,
                pid,
                cwd,
                role,
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
        let mut runs_vec: Vec<ActiveRun> = active_runs_map.values().cloned().collect();
        runs_vec.sort_by(|a, b| {
            a.agent_id
                .cmp(&b.agent_id)
                .then_with(|| a.run_track.cmp(&b.run_track))
        });
        active_runs.store(Arc::new(runs_vec));
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
    stall_hook_sender: &mut Option<StallHookSender>,
    state_notify: &Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
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

        // Fire `on_stall` effect hook asynchronously for any newly committed Stall event (if fresh)
        if let Some(ref mut sender) = stall_hook_sender {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            const TEN_MINUTES_NS: u64 = 10 * 60 * 1_000_000_000;
            let min_fresh_ts_ns = now_ns.saturating_sub(TEN_MINUTES_NS);

            for ev in batch_events.iter() {
                if ev.kind == tacho_core::Kind::Stall {
                    if ev.ts_ns < min_fresh_ts_ns {
                        continue;
                    }
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
                        let reason = st.evidence.clone().unwrap_or_default();
                        sender.send(StallJob {
                            agent,
                            stall_kind,
                            reason,
                            seq: ev.seq,
                            ts_ns: ev.ts_ns,
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
        {
            let (lock, cvar) = &**state_notify;
            let mut seq = lock.lock().unwrap();
            *seq += 1;
            cvar.notify_all();
        }

        batch_events.clear();
    }

    // Notify any flush waiters
    for notifier in flush_notifiers.drain(..) {
        let _ = notifier.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tacho_core::{SessionKey, ToolInput};

    #[test]
    fn test_run_track_key_helper() {
        assert_eq!(run_track_key("worker", 0), "agent:worker/run:1");
        assert_eq!(run_track_key("worker", 1), "agent:worker/run:1");
        assert_eq!(run_track_key("worker", 5), "agent:worker/run:5");
    }

    #[test]
    fn test_on_stall_hook_timeout_kills_process_group_and_unblocks_second_hook() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("hook_timeout_test.sqlite");
        let second_marker = temp.path().join("second_fired.txt");
        let log = EventLog::open(&db_path).unwrap();
        let recorder = Recorder::new();

        // The hook script sleeps forever if TACHO_AGENT is "stuck-worker",
        // but writes to second_marker if TACHO_AGENT is "second-worker".
        let hook_cmd = format!(
            r#"if [ "$TACHO_AGENT" = "stuck-worker" ]; then
    sleep 300
else
    printf '%s:%s' "$TACHO_AGENT" "$TACHO_STALL_KIND" > {}
fi"#,
            second_marker.display()
        );

        let timeout = Duration::from_millis(300);
        let (appender, _thread) =
            spawn_appender_with_hook_timeout(recorder, log, 256, Some(hook_cmd), timeout);

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // 1. Trigger stall on stuck-worker (will sleep in worker thread)
        let sess_stuck = SessionKey::new("claude", "sess-stuck");
        appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(now_ns),
                    provider: "claude".into(),
                    session_id: "sess-stuck".into(),
                    agent: Some("stuck-worker".into()),
                    task: Some("hang".into()),
                    pid: None,
                    cwd: None,
                    model: None,
                    parent_session: None,
                    git_tree: None,
                    role: None,
                },
                now_ns,
            )
            .unwrap();

        for i in 0..4 {
            let ts = now_ns + 10_000 + i * 100_000;
            appender
                .send_input(
                    Input::ToolBegin {
                        source_ts_ns: Some(ts),
                        session: sess_stuck.clone(),
                        tool_use_id: format!("t-stuck-{}", i),
                        tool_name: "Bash".into(),
                        input: ToolInput::Shell {
                            argv: "cat stuck.txt".into(),
                            cwd: None,
                            git_tree: None,
                        },
                    },
                    ts,
                )
                .unwrap();
        }
        appender.flush();

        // 2. Trigger stall on second-worker (should be queued and execute after stuck-worker is killed)
        let sess_second = SessionKey::new("claude", "sess-second");
        appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(now_ns + 1_000_000),
                    provider: "claude".into(),
                    session_id: "sess-second".into(),
                    agent: Some("second-worker".into()),
                    task: Some("work".into()),
                    pid: None,
                    cwd: None,
                    model: None,
                    parent_session: None,
                    git_tree: None,
                    role: None,
                },
                now_ns + 1_000_000,
            )
            .unwrap();

        for i in 0..4 {
            let ts = now_ns + 1_000_000 + 10_000 + i * 100_000;
            appender
                .send_input(
                    Input::ToolBegin {
                        source_ts_ns: Some(ts),
                        session: sess_second.clone(),
                        tool_use_id: format!("t-sec-{}", i),
                        tool_name: "Bash".into(),
                        input: ToolInput::Shell {
                            argv: "cat second.txt".into(),
                            cwd: None,
                            git_tree: None,
                        },
                    },
                    ts,
                )
                .unwrap();
        }
        appender.flush();

        // 3. Wait for second_marker to appear using inotify event waiting (no sleep loop)
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd >= 0 {
            if let Ok(path_c) = std::ffi::CString::new(temp.path().to_str().unwrap_or_default()) {
                let wd = unsafe {
                    libc::inotify_add_watch(
                        inotify_fd,
                        path_c.as_ptr(),
                        libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
                    )
                };
                let cap = Instant::now() + Duration::from_secs(60);
                while !second_marker.exists() && Instant::now() < cap {
                    let mut pfd = libc::pollfd {
                        fd: inotify_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut pfd, 1, 1_000) };
                    if rc > 0 {
                        let mut buf = [0u8; 1024];
                        let _ = unsafe {
                            libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                    }
                }
                if wd >= 0 {
                    unsafe { libc::inotify_rm_watch(inotify_fd, wd) };
                }
            }
            unsafe { libc::close(inotify_fd) };
        }

        let content = std::fs::read_to_string(&second_marker)
            .expect("second hook must complete after stuck hook is killed");
        assert_eq!(content, "second-worker:LOOP");
    }

    #[test]
    fn test_on_stall_hook_ignores_historical_replay() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("hook_hist_test.sqlite");
        let marker = temp.path().join("hist_fired.txt");
        let log = EventLog::open(&db_path).unwrap();
        let recorder = Recorder::new();

        let hook_cmd = format!("touch {}", marker.display());
        let (appender, _thread) = spawn_appender_with_hook_timeout(
            recorder,
            log,
            256,
            Some(hook_cmd),
            Duration::from_secs(5),
        );

        // Timestamps 1 hour ago (> 10 minutes)
        let old_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            - 3600 * 1_000_000_000;

        let sess = SessionKey::new("claude", "sess-old");
        appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(old_ts),
                    provider: "claude".into(),
                    session_id: "sess-old".into(),
                    agent: Some("old-worker".into()),
                    task: Some("replay".into()),
                    pid: None,
                    cwd: None,
                    model: None,
                    parent_session: None,
                    git_tree: None,
                    role: None,
                },
                old_ts,
            )
            .unwrap();

        for i in 0..4 {
            let ts = old_ts + 10_000 + i * 100_000;
            appender
                .send_input(
                    Input::ToolBegin {
                        source_ts_ns: Some(ts),
                        session: sess.clone(),
                        tool_use_id: format!("t-old-{}", i),
                        tool_name: "Bash".into(),
                        input: ToolInput::Shell {
                            argv: "cat old.txt".into(),
                            cwd: None,
                            git_tree: None,
                        },
                    },
                    ts,
                )
                .unwrap();
        }
        appender
            .send_input(
                Input::RunEnd {
                    source_ts_ns: Some(old_ts + 500_000),
                    session: sess.clone(),
                    exit_code: Some(0),
                    reason: None,
                },
                old_ts + 500_000,
            )
            .unwrap();
        appender.flush();

        std::thread::yield_now();
        assert!(
            !marker.exists(),
            "historical replay stall must not fire on_stall hook"
        );
    }
}
