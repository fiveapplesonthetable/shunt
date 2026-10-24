//! Appender thread: single consumer of input channel and single writer to SQLite WAL log.
//!
//! Enforces WAL logging of all recorded events and lock-free publishing of LiveState
//! and TrackRegistry for concurrent readers.

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::{AgentRole, Event, Input, LiveState, Recorder, SessionKey, TrackRegistry};

use crate::log::EventLog;

/// How long a producer waits for queue space before the input is rejected as "queue full".
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// How long [`AppenderHandle::flush`] waits for the appender to confirm a commit.
const FLUSH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
/// Group commit window: inputs arriving within this window share one SQLite transaction.
/// It also paces the idle detector tick (silence detection needs time to advance).
const COMMIT_INTERVAL: Duration = Duration::from_millis(50);
/// Detector tick cadence while the queue is continuously busy.
const TICK_INTERVAL: Duration = Duration::from_millis(200);
/// Events per SQLite transaction before committing early.
const MAX_BATCH_SIZE: usize = 256;
/// Bound on queued `on_stall` hook jobs; further stalls are dropped (and logged once).
const STALL_HOOK_QUEUE: usize = 64;
/// Stalls older than this (e.g. replayed history) never fire `on_stall`.
const STALL_HOOK_FRESHNESS_NS: u64 = 10 * 60 * 1_000_000_000;

fn wall_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Test hooks to simulate a wedged or paused appender thread.
#[derive(Debug, Default)]
pub struct AppenderHooks {
    /// When true, causes the appender thread to block until cleared.
    pub wedged: AtomicBool,
    pub condvar: Condvar,
    pub mutex: Mutex<()>,
}

impl AppenderHooks {
    pub fn set_wedged(&self, wedged: bool) {
        let _guard = self.mutex.lock().unwrap_or_else(|p| p.into_inner());
        self.wedged.store(wedged, Ordering::Release);
        if !wedged {
            self.condvar.notify_all();
        }
    }

    /// Blocks while the wedge is set.
    fn block_while_wedged(&self) {
        if !self.wedged.load(Ordering::Acquire) {
            return;
        }
        let guard = self.mutex.lock().unwrap_or_else(|p| p.into_inner());
        let _unused = self
            .condvar
            .wait_while(guard, |_| self.wedged.load(Ordering::Acquire));
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

/// Sequence number bumped on every published `LiveState`, plus the condvar readers wait on.
#[derive(Default)]
struct StateNotify {
    seq: Mutex<u64>,
    cvar: Condvar,
}

impl StateNotify {
    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        self.seq.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        let mut seq = self.lock();
        *seq += 1;
        self.cvar.notify_all();
    }
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
    state_notify: Arc<StateNotify>,
    shutdown_requested: Arc<AtomicBool>,
}

impl AppenderHandle {
    /// Waits for `LiveState` to update beyond `last_seq` within `timeout`, returning the new sequence.
    pub fn wait_state_change(&self, last_seq: u64, timeout: Duration) -> u64 {
        let guard = self.state_notify.lock();
        let (guard, _) = self
            .state_notify
            .cvar
            .wait_timeout_while(guard, timeout, |seq| *seq == last_seq)
            .unwrap_or_else(|p| p.into_inner());
        *guard
    }

    /// Blocks until `LiveState` moves past `last_seq` or `cancel` is set, with no timeout.
    ///
    /// Whoever sets `cancel` must then call [`AppenderHandle::wake_state_waiters`]; the flag
    /// is re-checked under the state mutex, so that wakeup cannot be lost.
    pub fn wait_state_change_until(&self, last_seq: u64, cancel: &AtomicBool) -> u64 {
        let guard = self.state_notify.lock();
        let guard = self
            .state_notify
            .cvar
            .wait_while(guard, |seq| {
                *seq == last_seq && !cancel.load(Ordering::Acquire)
            })
            .unwrap_or_else(|p| p.into_inner());
        *guard
    }

    /// Wakes every thread blocked in a state wait so it can re-check its cancel flag.
    pub fn wake_state_waiters(&self) {
        let _guard = self.state_notify.lock();
        self.state_notify.cvar.notify_all();
    }

    /// Returns current state sequence number.
    pub fn state_seq(&self) -> u64 {
        *self.state_notify.lock()
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
            SEND_TIMEOUT,
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
            .send_timeout(AppenderCommand::InputBatch(batch), SEND_TIMEOUT)
    }

    /// Commits everything queued before this call. Returns true once the appender confirms;
    /// false if it is stopped, wedged or too busy to confirm in time.
    pub fn flush(&self) -> bool {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.sender
            .send_timeout(AppenderCommand::Flush(tx), SEND_TIMEOUT)
            .is_ok()
            && rx.recv_timeout(FLUSH_CONFIRM_TIMEOUT).is_ok()
    }

    /// Signals the appender thread to terminate cleanly after committing what is queued.
    ///
    /// Reliable even when the queue is full: the flag is checked on every loop iteration,
    /// and the command only exists to wake an idle loop promptly.
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        let _ = self.sender.try_send(AppenderCommand::Shutdown);
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

impl StallJob {
    /// Builds a job for a fresh, committed stall event; `None` for anything else.
    fn from_event(ev: &Event, min_fresh_ts_ns: u64) -> Option<Self> {
        use tacho_proto::tacho::StallKind;
        if ev.kind != tacho_core::Kind::Stall || ev.ts_ns < min_fresh_ts_ns {
            return None;
        }
        let Some(tacho_proto::tacho::event::Body::Stall(ref st)) = ev.body.body else {
            return None;
        };
        let stall_kind = match st.kind.and_then(|k| StallKind::try_from(k).ok()) {
            Some(StallKind::Silent) => "SILENT",
            Some(StallKind::Loop) => "LOOP",
            Some(StallKind::Spin) => "SPIN",
            Some(StallKind::Burn) => "BURN",
            Some(StallKind::Failing) => "FAILING",
            Some(StallKind::OffBrief) => "OFF_BRIEF",
            Some(StallKind::StallClaim) => "CLAIM",
            _ => "STALL",
        };
        Some(Self {
            agent: ev.agent.clone().unwrap_or_default(),
            stall_kind,
            reason: st.evidence.clone().unwrap_or_default(),
            seq: ev.seq,
            ts_ns: ev.ts_ns,
        })
    }
}

/// Dedicated sender for the bounded on_stall effect hook worker.
struct StallHookSender {
    tx: std::sync::mpsc::SyncSender<StallJob>,
    dropped_count: usize,
}

impl StallHookSender {
    fn send(&mut self, job: StallJob) {
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped_count += 1;
                if self.dropped_count == 1 {
                    eprintln!(
                        "[tacho-daemon] on_stall hook queue full ({} items); dropping stall hooks",
                        STALL_HOOK_QUEUE
                    );
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Runs the dedicated worker thread servicing on_stall hook commands, one at a time.
fn run_stall_worker(
    receiver: std::sync::mpsc::Receiver<StallJob>,
    cmd_str: String,
    timeout: Duration,
) {
    while let Ok(job) = receiver.recv() {
        run_stall_hook(&cmd_str, &job, timeout);
    }
}

/// Runs one hook in its own process group and makes sure nothing it started outlives it.
///
/// The leader stays unreaped (a zombie at worst) until after `kill(-pgid)`, so the group id
/// cannot have been recycled by an unrelated process when we signal it.
fn run_stall_hook(cmd_str: &str, job: &StallJob, timeout: Duration) {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(cmd_str)
        .env("TACHO_AGENT", &job.agent)
        .env("TACHO_STALL_KIND", job.stall_kind)
        .env("TACHO_STALL_REASON", &job.reason)
        .env("TACHO_SEQ", job.seq.to_string())
        .env("TACHO_TS_NS", job.ts_ns.to_string())
        .stdin(std::process::Stdio::null())
        .process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[tacho-daemon] failed to spawn on_stall hook: {}", e);
            return;
        }
    };
    let pgid = child.id() as libc::pid_t;

    match wait_exit_without_reaping(pgid, timeout) {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "[tacho-daemon] on_stall hook for {} exceeded {:?}; killing its process group",
            job.agent, timeout
        ),
        Err(e) => eprintln!("[tacho-daemon] on_stall hook wait failed: {}", e),
    }
    // Kill the timed-out hook, or any background stragglers of a hook that exited.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Waits up to `timeout` for `pid` (our unreaped child) to exit, leaving it unreaped.
///
/// Uses a pidfd when available; otherwise `waitid(WNOWAIT)` on a helper thread.
/// Returns Ok(true) if it exited, Ok(false) on timeout.
fn wait_exit_without_reaping(pid: libc::pid_t, timeout: Duration) -> std::io::Result<bool> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if pidfd < 0 {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("tacho-on-stall-wait".to_string())
            .spawn(move || {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                loop {
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid as libc::id_t,
                            &mut info,
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if rc == 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
                let _ = tx.send(());
            })?;
        return Ok(rx.recv_timeout(timeout).is_ok());
    }
    // SAFETY: pidfd_open returned a fresh fd that we own.
    let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(pidfd) };
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&pidfd),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc > 0 {
            return Ok(true);
        }
        if rc == 0 {
            return Ok(false);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
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

/// Active runs restored from a recovered `LiveState`; their pids are unknown (0) until the
/// agent's next `RunStart`, so they are sampled for worktree progress only.
fn restored_active_runs(state: &LiveState) -> HashMap<SessionKey, ActiveRun> {
    state
        .agents
        .iter()
        .filter(|(_, a)| a.state != "Exited" && a.cwd.is_some())
        .map(|(agent_id, a)| {
            let session = SessionKey::new(&a.provider, &a.session_id);
            let run = ActiveRun {
                session: session.clone(),
                agent_id: agent_id.clone(),
                run_track: run_track_key(agent_id, a.run_n),
                pid: 0,
                cwd: a.cwd.clone(),
                role: AgentRole::infer(agent_id, a.task.as_deref()),
            };
            (session, run)
        })
        .collect()
}

fn sorted_runs(map: &HashMap<SessionKey, ActiveRun>) -> Vec<ActiveRun> {
    let mut runs: Vec<ActiveRun> = map.values().cloned().collect();
    runs.sort_by(|a, b| {
        a.agent_id
            .cmp(&b.agent_id)
            .then_with(|| a.run_track.cmp(&b.run_track))
    });
    runs
}

/// Spawns the dedicated appender thread with an optional `on_stall` shell hook and custom timeout.
pub fn spawn_appender_with_hook_timeout(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
    on_stall_timeout: Duration,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    let (sender, receiver) = crossbeam_channel::bounded(channel_capacity);

    let active_runs_map = restored_active_runs(recorder.live_state());
    let handle = AppenderHandle {
        sender,
        live_state: Arc::new(ArcSwap::from_pointee(recorder.live_state().clone())),
        published_registry: Arc::new(ArcSwap::from_pointee(recorder.registry().clone())),
        active_runs: Arc::new(ArcSwap::from_pointee(sorted_runs(&active_runs_map))),
        hooks: Arc::new(AppenderHooks::default()),
        write_errors: Arc::new(AtomicUsize::new(0)),
        state_notify: Arc::new(StateNotify::default()),
        shutdown_requested: Arc::new(AtomicBool::new(false)),
    };

    let (stall_hooks, stall_worker) = match on_stall {
        Some(cmd_str) if !cmd_str.trim().is_empty() => {
            let (tx, rx) = std::sync::mpsc::sync_channel::<StallJob>(STALL_HOOK_QUEUE);
            let worker = std::thread::Builder::new()
                .name("tacho-on-stall".to_string())
                .spawn(move || run_stall_worker(rx, cmd_str, on_stall_timeout))
                .expect("failed to spawn on-stall worker thread");
            let sender = StallHookSender {
                tx,
                dropped_count: 0,
            };
            (Some(sender), Some(worker))
        }
        _ => (None, None),
    };

    let last_published_reg_len = recorder.registry().len();
    let appender = Appender {
        receiver,
        recorder,
        log,
        shared: handle.clone(),
        active_runs_map,
        stall_hooks,
        batch: Vec::with_capacity(MAX_BATCH_SIZE),
        flush_notifiers: Vec::new(),
        last_published_reg_len,
        last_tick: Instant::now(),
    };
    let join_handle = std::thread::Builder::new()
        .name("tacho-appender".to_string())
        .spawn(move || {
            appender.run();
            // Dropping the hook sender (inside `run`) lets the worker finish queued hooks.
            if let Some(worker) = stall_worker {
                let _ = worker.join();
            }
        })
        .expect("failed to spawn appender thread");

    (handle, join_handle)
}

/// State owned by the appender thread.
struct Appender {
    receiver: Receiver<AppenderCommand>,
    recorder: Recorder,
    log: EventLog,
    /// The publishing side of the handle (live state, registry, runs, counters).
    shared: AppenderHandle,
    active_runs_map: HashMap<SessionKey, ActiveRun>,
    stall_hooks: Option<StallHookSender>,
    batch: Vec<Event>,
    flush_notifiers: Vec<Sender<()>>,
    last_published_reg_len: usize,
    last_tick: Instant,
}

/// Whether the loop should keep going after a command.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

impl Appender {
    fn shutdown_requested(&self) -> bool {
        self.shared.shutdown_requested.load(Ordering::Acquire)
    }

    /// Main loop: wait for the first command, gather more for up to one commit window,
    /// then commit them in one transaction.
    fn run(mut self) {
        while !self.shutdown_requested() {
            // Chaos injection hook: intentionally wedges the appender thread to simulate SQLite
            // lock contention and verify the daemon degrades cleanly (test14_wedged_appender).
            self.shared.hooks.block_while_wedged();

            let first = match self.receiver.recv_timeout(COMMIT_INTERVAL) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    self.tick();
                    if !self.batch.is_empty() {
                        self.commit();
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };

            let window_start = Instant::now();
            let mut flow = self.handle(first);
            while flow == Flow::Continue
                && self.batch.len() < MAX_BATCH_SIZE
                && window_start.elapsed() < COMMIT_INTERVAL
            {
                match self.receiver.try_recv() {
                    Ok(cmd) => flow = self.handle(cmd),
                    Err(_) => break,
                }
            }
            if flow == Flow::Stop {
                break;
            }

            if self.last_tick.elapsed() >= TICK_INTERVAL {
                self.tick();
            }
            if !self.batch.is_empty() || !self.flush_notifiers.is_empty() {
                self.commit();
            }
        }

        // Final drain: everything accepted before shutdown is committed.
        while let Ok(cmd) = self.receiver.try_recv() {
            self.handle(cmd);
        }
        if !self.batch.is_empty() || !self.flush_notifiers.is_empty() {
            self.commit();
        }
    }

    fn handle(&mut self, cmd: AppenderCommand) -> Flow {
        match cmd {
            AppenderCommand::Input(input, rx_ts) => self.apply(*input, rx_ts),
            AppenderCommand::InputBatch(inputs) => {
                for (input, rx_ts) in inputs {
                    self.apply(input, rx_ts);
                }
            }
            AppenderCommand::Flush(tx) => self.flush_notifiers.push(tx),
            AppenderCommand::Shutdown => return Flow::Stop,
        }
        Flow::Continue
    }

    /// Runs time-based detectors (silence, etc.).
    fn tick(&mut self) {
        self.recorder.tick(wall_clock_ns(), &mut self.batch);
        self.last_tick = Instant::now();
    }

    /// Applies one input, tracking run starts/ends for the sampler, and commits early
    /// when a huge batch fills the transaction.
    fn apply(&mut self, input: Input, rx_ts: u64) {
        let started = match &input {
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
                let agent_id = agent.clone().unwrap_or_else(|| provider.clone());
                let role = role.unwrap_or_else(|| AgentRole::infer(&agent_id, task.as_deref()));
                Some((
                    SessionKey::new(provider, session_id),
                    agent_id,
                    pid.unwrap_or(0),
                    cwd.clone(),
                    role,
                ))
            }
            _ => None,
        };
        let ended = match &input {
            Input::RunEnd { session, .. } => Some(session.clone()),
            _ => None,
        };

        self.recorder.apply(input, rx_ts, &mut self.batch);

        let mut changed = false;
        if let Some((session, agent_id, pid, cwd, role)) = started {
            let run_n = self
                .recorder
                .live_state()
                .agents
                .get(&agent_id)
                .map(|a| a.run_n)
                .unwrap_or(1);
            let run = ActiveRun {
                session: session.clone(),
                run_track: run_track_key(&agent_id, run_n),
                agent_id,
                pid,
                cwd,
                role,
            };
            self.active_runs_map.insert(session, run);
            changed = true;
        }
        if let Some(session) = ended {
            changed |= self.active_runs_map.remove(&session).is_some();
        }
        if changed {
            self.shared
                .active_runs
                .store(Arc::new(sorted_runs(&self.active_runs_map)));
        }

        if self.batch.len() >= MAX_BATCH_SIZE {
            self.commit();
        }
    }

    /// Commits the batch: SQLite rows first, then stall hooks, then published state,
    /// then flush confirmations.
    fn commit(&mut self) {
        if !self.batch.is_empty() {
            match self.log.insert_batch(self.batch.iter()) {
                Ok(_) => self.fire_stall_hooks(),
                Err(e) => {
                    self.shared.write_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[tacho-daemon] failed to insert batch into SQLite WAL (graceful degradation): {}",
                        e
                    );
                    // Still run hooks: a stall is actionable even if it could not be logged.
                    self.fire_stall_hooks();
                }
            }

            let reg_len = self.recorder.registry().len();
            if reg_len != self.last_published_reg_len || !self.flush_notifiers.is_empty() {
                self.shared
                    .published_registry
                    .store(Arc::new(self.recorder.registry().clone()));
                self.last_published_reg_len = reg_len;
            }
            self.shared
                .live_state
                .store(Arc::new(self.recorder.live_state().clone()));
            self.shared.state_notify.bump();
            self.batch.clear();
        }

        for notifier in self.flush_notifiers.drain(..) {
            let _ = notifier.send(());
        }
    }

    fn fire_stall_hooks(&mut self) {
        let Some(sender) = self.stall_hooks.as_mut() else {
            return;
        };
        let min_fresh_ts_ns = wall_clock_ns().saturating_sub(STALL_HOOK_FRESHNESS_NS);
        for ev in &self.batch {
            if let Some(job) = StallJob::from_event(ev, min_fresh_ts_ns) {
                sender.send(job);
            }
        }
    }
}

#[cfg(test)]
mod tests;
