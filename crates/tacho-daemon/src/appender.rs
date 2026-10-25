//! Appender thread: single consumer of input channel and single writer to SQLite WAL log.
//!
//! Enforces WAL logging of all recorded events and lock-free publishing of LiveState
//! and TrackRegistry for concurrent readers.

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::{AgentRole, Event, Input, Kind, LiveState, Recorder, SessionKey, TrackRegistry};

use crate::log::EventLog;

/// How long a producer waits for queue space before the input is rejected as "queue full".
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`AppenderHandle::flush`] waits for the appender to confirm a commit.
const FLUSH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on how long one burst of already-queued inputs is gathered into a single
/// SQLite transaction. The appender never waits for more input to arrive: it drains what is
/// queued and commits, so this only caps latency under a continuous flood.
const MAX_COMMIT_WINDOW: Duration = Duration::from_millis(50);
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
#[derive(Debug)]
pub struct AppenderHooks {
    /// When true, causes the appender thread to block until cleared.
    pub wedged: AtomicBool,
    pub condvar: Condvar,
    pub mutex: Mutex<()>,
    hook_expire_pipe: Option<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)>,
}

impl Default for AppenderHooks {
    fn default() -> Self {
        use std::os::fd::FromRawFd;
        let mut fds = [-1i32; 2];
        let pipe =
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == 0 {
                Some(unsafe {
                    (
                        std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                        std::os::fd::OwnedFd::from_raw_fd(fds[1]),
                    )
                })
            } else {
                None
            };
        Self {
            wedged: AtomicBool::new(false),
            condvar: Condvar::new(),
            mutex: Mutex::new(()),
            hook_expire_pipe: pipe,
        }
    }
}

impl AppenderHooks {
    pub fn is_wedged(&self) -> bool {
        self.wedged.load(Ordering::Acquire)
    }

    pub fn set_wedged(&self, wedged: bool) {
        let _guard = self.mutex.lock().unwrap_or_else(|p| p.into_inner());
        self.wedged.store(wedged, Ordering::Release);
        if !wedged {
            self.condvar.notify_all();
        }
    }

    /// Deterministically expires the currently running `on_stall` hook wait via its wakeup pipe.
    pub fn expire_active_hook(&self) {
        use std::os::fd::AsRawFd;
        if let Some((_, ref write_fd)) = self.hook_expire_pipe {
            let byte = [1u8];
            // SAFETY: writing one byte from a local buffer to a pipe fd we own.
            let n = unsafe { libc::write(write_fd.as_raw_fd(), byte.as_ptr().cast(), 1) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                // A full pipe already holds a pending expiry.
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    eprintln!("[tacho-daemon] failed to expire on_stall hook: {err}");
                }
            }
        }
    }

    fn hook_expire_read_fd(&self) -> Option<libc::c_int> {
        use std::os::fd::AsRawFd;
        self.hook_expire_pipe
            .as_ref()
            .map(|(read_fd, _)| read_fd.as_raw_fd())
    }

    fn drain_hook_expire(&self) {
        use std::os::fd::AsRawFd;
        if let Some((ref read_fd, _)) = self.hook_expire_pipe {
            let mut buf = [0u8; 64];
            while unsafe { libc::read(read_fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0
            {
            }
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
    sampler_waker: Arc<Mutex<Option<crate::sampler::SamplerWaker>>>,
    /// Bumped on every commit that contains an event `decide` reacts to.
    decide_seq: Arc<AtomicU64>,
}

impl AppenderHandle {
    /// Returns a counter that advances whenever a committed batch contains an event that
    /// can change a `decide` outcome (see [`is_decide_relevant`]). The sampler compares it
    /// with the value seen at its last decide pass.
    pub fn decide_seq(&self) -> u64 {
        self.decide_seq.load(Ordering::Acquire)
    }

    /// Attaches the sampler waker so committed decide-relevant events (lifecycle, stall,
    /// task, progress) immediately wake the sampler/decide loop.
    pub fn set_sampler_waker(&self, waker: crate::sampler::SamplerWaker) {
        *self.sampler_waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(waker);
    }

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
        let timeout = if self.hooks.is_wedged() {
            Duration::from_millis(100)
        } else {
            SEND_TIMEOUT
        };
        self.sender
            .send_timeout(AppenderCommand::Input(Box::new(input), received_at_ns), timeout)
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
        let timeout = if self.hooks.is_wedged() {
            Duration::from_millis(100)
        } else {
            SEND_TIMEOUT
        };
        self.sender
            .send_timeout(AppenderCommand::InputBatch(batch), timeout)
    }

    /// Commits everything queued before this call. Returns true once the appender confirms;
    /// false if it is stopped, wedged or too busy to confirm in time.
    pub fn flush(&self) -> bool {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let send_timeout = if self.hooks.is_wedged() {
            Duration::from_millis(100)
        } else {
            SEND_TIMEOUT
        };
        let confirm_timeout = if self.hooks.is_wedged() {
            Duration::from_millis(100)
        } else {
            FLUSH_CONFIRM_TIMEOUT
        };
        self.sender
            .send_timeout(AppenderCommand::Flush(tx), send_timeout)
            .is_ok()
            && rx.recv_timeout(confirm_timeout).is_ok()
    }

    /// Signals the appender thread to terminate cleanly after committing what is queued.
    ///
    /// Reliable even when the queue is full: the flag is checked on every loop iteration,
    /// and the command only exists to wake an idle loop promptly.
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        match self.sender.try_send(AppenderCommand::Shutdown) {
            Ok(()) => {}
            // A full queue keeps the loop busy, so it reaches the flag check without this
            // wake; a disconnected queue means the loop has already exited.
            Err(crossbeam_channel::TrySendError::Full(_))
            | Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
        }
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
    hooks: Arc<AppenderHooks>,
) {
    while let Ok(job) = receiver.recv() {
        run_stall_hook(&cmd_str, &job, timeout, &hooks);
    }
}

/// Runs one hook in its own process group and makes sure nothing it started outlives it.
///
/// The leader stays unreaped (a zombie at worst) until after `kill(-pgid)`, so the group id
/// cannot have been recycled by an unrelated process when we signal it.
fn run_stall_hook(cmd_str: &str, job: &StallJob, timeout: Duration, hooks: &AppenderHooks) {
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

    match wait_exit_without_reaping(pgid, timeout, hooks) {
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
    if let Err(e) = child.wait() {
        eprintln!("[tacho-daemon] failed to reap on_stall hook: {}", e);
    }
}

/// Waits up to `timeout` for `pid` (our unreaped child) to exit, leaving it unreaped.
///
/// Uses a pidfd when available; otherwise `waitid(WNOWAIT)` on a helper thread.
/// Returns Ok(true) if it exited, Ok(false) on timeout.
fn wait_exit_without_reaping(
    pid: libc::pid_t,
    timeout: Duration,
    hooks: &AppenderHooks,
) -> std::io::Result<bool> {
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
                // Not actionable: a failed send only means the waiter already timed out.
                tx.send(()).ok();
            })?;
        return Ok(rx.recv_timeout(timeout).is_ok());
    }
    // SAFETY: pidfd_open returned a fresh fd that we own.
    let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(pidfd) };
    let deadline = Instant::now() + timeout;
    let expire_fd = hooks.hook_expire_read_fd().unwrap_or(-1);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut pfds = [
            libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&pidfd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: expire_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds: libc::nfds_t = if expire_fd >= 0 { 2 } else { 1 };
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, ms) };
        if rc > 0 {
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                return Ok(true);
            }
            if nfds == 2 && pfds[1].revents & libc::POLLIN != 0 {
                hooks.drain_hook_expire();
                return Ok(false);
            }
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
///
/// The silence detector is scheduled with [`tacho_core::DetectorConfig`]'s default timeout;
/// use [`spawn_appender_with_silence`] when the recorder was configured with another one.
pub fn spawn_appender_with_hook_timeout(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
    on_stall_timeout: Duration,
) -> (AppenderHandle, std::thread::JoinHandle<()>) {
    spawn_appender_with_silence(
        recorder,
        log,
        channel_capacity,
        on_stall,
        on_stall_timeout,
        Duration::from_nanos(tacho_core::DetectorConfig::default().silent_timeout_ns),
    )
}

/// Spawns the appender thread. `silent_timeout` must equal the recorder's configured
/// `silent_timeout_ns`: the appender sleeps until the earliest moment an agent could turn
/// SILENT and runs the detector then, instead of ticking on a fixed cadence.
pub fn spawn_appender_with_silence(
    recorder: Recorder,
    log: EventLog,
    channel_capacity: usize,
    on_stall: Option<String>,
    on_stall_timeout: Duration,
    silent_timeout: Duration,
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
        sampler_waker: Arc::new(Mutex::new(None)),
        decide_seq: Arc::new(AtomicU64::new(0)),
    };

    let (stall_hooks, stall_worker) = match on_stall {
        Some(cmd_str) if !cmd_str.trim().is_empty() => {
            let (tx, rx) = std::sync::mpsc::sync_channel::<StallJob>(STALL_HOOK_QUEUE);
            let worker_hooks = handle.hooks.clone();
            let worker = std::thread::Builder::new()
                .name("tacho-on-stall".to_string())
                .spawn(move || run_stall_worker(rx, cmd_str, on_stall_timeout, worker_hooks))
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
    let silence = SilenceClock::seeded(
        silent_timeout.as_nanos().min(u64::MAX as u128) as u64,
        recorder.live_state(),
        wall_clock_ns(),
    );
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
        silence,
    };
    let join_handle = std::thread::Builder::new()
        .name("tacho-appender".to_string())
        .spawn(move || {
            appender.run();
            // Dropping the hook sender (inside `run`) lets the worker finish queued hooks.
            if let Some(worker) = stall_worker {
                if worker.join().is_err() {
                    eprintln!("[tacho-daemon] on_stall worker thread panicked");
                }
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
    silence: SilenceClock,
}

/// Whether a committed event can change a `decide` outcome: agent lifecycle and state,
/// stalls, task queue changes, milestones (commits/pushes), and progress (edits, turns).
/// Tool calls, commands, file reads, messages and counter samples are not, so the sampler's
/// own counter samples never retrigger `decide`.
pub(crate) fn is_decide_relevant(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Run
            | Kind::RunEnded
            | Kind::TurnEnded
            | Kind::FileEdited
            | Kind::Stall
            | Kind::TaskQueued
            | Kind::TaskState
            | Kind::AgentState
            | Kind::Milestone
    )
}

/// Schedules the SILENT detector from committed events instead of a fixed tick.
///
/// An agent can only turn SILENT `timeout_ns` after its latest event, so the appender only
/// needs to run the detector at the earliest such deadline. After a detector pass, agents
/// whose deadline passed are rescheduled one full timeout later (their silence may be
/// suppressed by CPU activity, which is not an event here) and exited agents are dropped;
/// each agent is therefore checked at most once per timeout while it stays quiet.
struct SilenceClock {
    timeout_ns: u64,
    /// Agent id to the wall-clock time from which its silence deadline is counted.
    since_ns: HashMap<String, u64>,
}

impl SilenceClock {
    /// Starts tracking every non-exited agent restored from the log as of `now_ns`, so a
    /// restart delays silence detection by at most one timeout rather than losing it.
    fn seeded(timeout_ns: u64, live: &LiveState, now_ns: u64) -> Self {
        let since_ns = live
            .agents
            .iter()
            .filter(|(_, a)| a.state != "Exited")
            .map(|(id, _)| (id.clone(), now_ns))
            .collect();
        Self {
            timeout_ns,
            since_ns,
        }
    }

    /// Records a committed event for `agent` at `ts_ns`.
    fn note(&mut self, agent: &str, ts_ns: u64) {
        match self.since_ns.get_mut(agent) {
            Some(since) => *since = (*since).max(ts_ns),
            None => {
                self.since_ns.insert(agent.to_string(), ts_ns);
            }
        }
    }

    /// Wall-clock time of the next detector pass, or `None` when no agent can go silent.
    fn next_deadline_ns(&self) -> Option<u64> {
        self.since_ns
            .values()
            .min()
            .map(|since| since.saturating_add(self.timeout_ns))
    }

    /// Updates the schedule after a detector pass at `now_ns`.
    fn settle(&mut self, now_ns: u64, live: &LiveState) {
        let timeout_ns = self.timeout_ns;
        self.since_ns.retain(|agent, since| {
            if since.saturating_add(timeout_ns) > now_ns {
                return true;
            }
            let exited = live.agents.get(agent).is_none_or(|a| a.state == "Exited");
            *since = now_ns;
            !exited
        });
    }
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

    /// Main loop: block until a command arrives or the next silence deadline passes, drain
    /// whatever else is already queued, and commit it in one transaction.
    ///
    /// There is no fixed tick: with no agent that could turn SILENT the loop blocks on the
    /// channel indefinitely, and otherwise it sleeps exactly until the earliest deadline.
    fn run(mut self) {
        while !self.shutdown_requested() {
            // Chaos injection hook: intentionally wedges the appender thread to simulate SQLite
            // lock contention and verify the daemon degrades cleanly (test14_wedged_appender).
            self.shared.hooks.block_while_wedged();

            let received = match self.silence.next_deadline_ns() {
                None => self
                    .receiver
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(deadline_ns) => self.receiver.recv_timeout(Duration::from_nanos(
                    deadline_ns.saturating_sub(wall_clock_ns()),
                )),
            };
            let first = match received {
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
                && window_start.elapsed() < MAX_COMMIT_WINDOW
            {
                match self.receiver.try_recv() {
                    Ok(cmd) => flow = self.handle(cmd),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            if flow == Flow::Stop {
                break;
            }

            // A busy queue must not starve the detector past its deadline.
            if self
                .silence
                .next_deadline_ns()
                .is_some_and(|deadline_ns| deadline_ns <= wall_clock_ns())
            {
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

    /// Runs time-based detectors (silence, etc.) and reschedules the next pass.
    fn tick(&mut self) {
        let now_ns = wall_clock_ns();
        self.recorder.tick(now_ns, &mut self.batch);
        self.silence.settle(now_ns, self.recorder.live_state());
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

            let mut decide_relevant = false;
            for ev in &self.batch {
                if let Some(agent) = ev.agent.as_deref() {
                    self.silence.note(agent, ev.ts_ns);
                }
                decide_relevant |= is_decide_relevant(ev.kind);
            }
            self.batch.clear();
            if decide_relevant {
                self.shared.decide_seq.fetch_add(1, Ordering::AcqRel);
                let waker = self
                    .shared
                    .sampler_waker
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                if let Some(waker) = waker {
                    waker.trigger_tick();
                }
            }
        }

        for notifier in self.flush_notifiers.drain(..) {
            // A requester that gave up waiting has dropped its receiver; nothing to do.
            if notifier.send(()).is_err() {
                continue;
            }
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
