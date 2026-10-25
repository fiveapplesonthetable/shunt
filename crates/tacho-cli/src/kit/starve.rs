//! `tacho starve [--cpus 4] [--cpu-set 0-3] [--timeout 300] -- <cmd...>`:
//! bounded CPU starvation runner (`<= 5 min` / 300s cap, 4 burners per CPU on `<= 1/8` fleet CPUs,
//! default highest 4 available CPUs, refuses to start during a merge-check batch, and stops
//! burners immediately if a merge-check batch starts while running).

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tacho_core::decide::watch::config::{StarveConfig, WatchConfig};
use tacho_core::decide::watch::starve::{
    plan_starve as plan_starve_core, should_stop_burner, StarvePlan,
};

use super::procs::{
    pid_alive_with_starttime, proc_pid_pgid, proc_pid_starttime, signal_group_and_leader,
};
use super::{emit_decision_event, open_fleet_store, use_private_tmp, write_atomic, FdEventWatcher};
use crate::cli::StarveArgs;

pub const STARVE_MAX_SECS: u64 = 300;
pub const BURNERS_PER_CPU: usize = 4;
pub const DEFAULT_SLICE_CPUS: usize = 4;
pub const FLEET_CPU_DIVISOR: usize = 8;

/// Metadata for a live burner process spawned by `tacho starve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredBurner {
    pub pid: u32,
    pub starttime: Option<u64>,
}

/// Parse a Linux CPU list string (e.g. `"0-3,8,10-11"`) into a sorted vector of CPU IDs.
pub fn parse_cpu_list_str(raw: &str) -> Option<Vec<usize>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut cpus = Vec::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        let (low_s, high_s) = p.split_once('-').unwrap_or((p, p));
        let low = low_s.parse::<usize>().ok()?;
        let high = high_s.parse::<usize>().ok()?;
        if low > high {
            return None;
        }
        for c in low..=high {
            if !cpus.contains(&c) {
                cpus.push(c);
            }
        }
    }
    cpus.sort_unstable();
    Some(cpus)
}

/// Read the current process's allowed CPU IDs from `/proc/self/status` (`Cpus_allowed_list`),
/// falling back to `0..total_cpus`.
pub fn read_available_cpus(total_cpus: usize) -> Vec<usize> {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
                if let Some(list) = parse_cpu_list_str(rest) {
                    if !list.is_empty() {
                        return list;
                    }
                }
            }
        }
    }
    (0..total_cpus.max(1)).collect()
}

/// Check if an executable exists on `PATH`.
pub fn command_on_path(bin: &str) -> bool {
    let Some(path_os) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_os) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Plan a `tacho starve` slice using `tacho_core::decide::watch::starve::plan_starve`.
pub fn plan_starve_slice(
    total_cpus: usize,
    requested_cpus: Option<usize>,
    cpu_set: Option<&str>,
    has_taskset: bool,
    has_systemd_scope: bool,
    available_cpus: Option<&[usize]>,
) -> StarvePlan {
    let cfg = StarveConfig {
        slice: DEFAULT_SLICE_CPUS,
        max_share: FLEET_CPU_DIVISOR,
        burners_per_cpu: BURNERS_PER_CPU,
        max_call_secs: STARVE_MAX_SECS,
    };
    let size = requested_cpus.unwrap_or(cfg.slice);
    plan_starve_core(
        total_cpus,
        size,
        cpu_set,
        has_taskset,
        has_systemd_scope,
        available_cpus,
        &cfg,
    )
}

/// Check whether any merge-check batch is currently active (`status == "running" || status == "bisecting"`).
pub fn is_merge_batch_active() -> bool {
    if let Ok(store) = open_fleet_store() {
        if let Ok(state) = store.load() {
            return state
                .batches
                .iter()
                .any(|b| b.status == "running" || b.status == "bisecting");
        }
    }
    false
}

/// Read `/proc/<pid>/environ` and `/proc/<pid>/cmdline` and verify via [`should_stop_burner`]
/// plus `(pid, starttime)` before killing a registered starve burner process group.
pub fn stop_registered_burner(burner: &RegisteredBurner) -> bool {
    let pid = burner.pid;
    if !pid_alive_with_starttime(pid, burner.starttime) {
        return false;
    }
    let Some(pgid) = proc_pid_pgid(pid) else {
        return false;
    };
    let environ: Vec<String> = fs::read(format!("/proc/{pid}/environ"))
        .map(|bytes| {
            bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let cmdline: Vec<String> = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default();

    let wc = WatchConfig::default();
    if should_stop_burner(pid, pgid, &environ, &cmdline, &wc) {
        match signal_group_and_leader(pid, libc::SIGKILL) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[tacho-starve] failed to stop burner pid {pid}: {e}");
                false
            }
        }
    } else {
        false
    }
}

/// Persist the active starve burners to `$TACHO_HOME/starve-burners.json` so a starting batch
/// or external supervisor can stop them if needed.
fn save_registered_burners(home: &Path, burners: &[RegisteredBurner]) -> io::Result<()> {
    crate::paths::ensure_dir_0700(home)?;
    let bytes = serde_json::to_vec_pretty(burners).map_err(io::Error::other)?;
    write_atomic(&home.join("starve-burners.json"), &bytes)
}

fn clear_registered_burners(home: &Path) {
    let _ = fs::remove_file(home.join("starve-burners.json"));
}

/// Preempt and kill all registered starve burners recorded in `$TACHO_HOME/starve-burners.json`.
pub fn preempt_registered_burners(home: &Path) -> usize {
    let path = home.join("starve-burners.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return 0;
    };
    let Ok(list) = serde_json::from_str::<Vec<RegisteredBurner>>(&raw) else {
        return 0;
    };
    let mut stopped = 0;
    for b in &list {
        if stop_registered_burner(b) {
            stopped += 1;
        }
    }
    let _ = fs::remove_file(&path);
    stopped
}

/// Guard ensuring every spawned burner child and process group is killed and reaped on exit.
struct BurnersGuard {
    children: Vec<(Child, RegisteredBurner)>,
    home: std::path::PathBuf,
}

impl BurnersGuard {
    fn new(home: std::path::PathBuf) -> Self {
        Self {
            children: Vec::new(),
            home,
        }
    }

    fn stop_all(&mut self) {
        for (mut child, reg) in self.children.drain(..) {
            let pid = child.id();
            let should_signal = match reg.starttime {
                Some(_) => pid_alive_with_starttime(pid, reg.starttime),
                None => super::procs::pid_alive(pid),
            };
            if should_signal {
                if let Err(e) = signal_group_and_leader(pid, libc::SIGKILL) {
                    eprintln!("[tacho-starve] failed to stop burner pid {pid}: {e}");
                }
            }
            let _ = child.kill();
            if let Err(e) = child.wait() {
                eprintln!("[tacho-starve] failed to reap burner pid {pid}: {e}");
            }
        }
        clear_registered_burners(&self.home);
    }
}

impl Drop for BurnersGuard {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Executes `tacho starve [--cpus 4] [--cpu-set 0-3] [--timeout 300] -- <cmd...>`.
pub fn execute_starve(args: StarveArgs) -> io::Result<i32> {
    if args.cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "starve command cannot be empty",
        ));
    }

    // 1. Refuse to start while a merge-check batch is running
    if is_merge_batch_active() {
        eprintln!("refusing to starve while a merge-check batch is running");
        return Ok(1);
    }

    let total_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let available = read_available_cpus(total_cpus);
    let has_taskset = command_on_path("taskset");
    let has_systemd_scope =
        command_on_path("systemd-run") && std::env::var_os("XDG_RUNTIME_DIR").is_some();

    let plan = plan_starve_slice(
        total_cpus,
        args.slice,
        args.cpu_set.as_deref(),
        has_taskset,
        has_systemd_scope,
        Some(&available),
    );
    let Some(ref wrap) = plan.wrap else {
        eprintln!("{}", plan.problem);
        return Ok(1);
    };

    // Cap duration strictly at <= 5 minutes (300 seconds)
    let timeout_secs = args
        .timeout
        .unwrap_or(STARVE_MAX_SECS)
        .clamp(1, STARVE_MAX_SECS);

    println!(
        "running on CPUs {} with {} burners for at most {} seconds",
        plan.cpus, plan.burners, timeout_secs
    );

    let home = crate::paths::resolve_home();
    let mut guard = BurnersGuard::new(home.clone());

    // 2. Spawn `plan.burners` (4 per CPU) pinned to `plan.cpus` in isolated process groups
    for _ in 0..plan.burners {
        let mut b_cmd = Command::new(&wrap[0]);
        use_private_tmp(&mut b_cmd, "starve")?;
        b_cmd
            .args(&wrap[1..])
            .args(["timeout", &timeout_secs.to_string(), "yes"])
            .env("TACHO_STARVE", "1")
            .env("STARVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Ok(c) = b_cmd.spawn() {
            let pid = c.id();
            let reg = RegisteredBurner {
                pid,
                starttime: proc_pid_starttime(pid),
            };
            guard.children.push((c, reg));
        }
    }

    let reg_list: Vec<RegisteredBurner> = guard.children.iter().map(|(_, r)| r.clone()).collect();
    save_registered_burners(&home, &reg_list)?;

    // 3. Spawn the target command pinned to the same CPU slice in its own process group
    let mut target_cmd = Command::new(&wrap[0]);
    use_private_tmp(&mut target_cmd, "starve")?;
    target_cmd
        .args(&wrap[1..])
        .args(&args.cmd)
        .env("TACHO_STARVE", "1")
        .env("STARVE", "1")
        .process_group(0);

    let mut child = match target_cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            guard.stop_all();
            let name = args.cmd.first().map(String::as_str).unwrap_or("");
            eprintln!("could not run {name}: {e}");
            return Ok(127);
        }
        Err(e) => {
            guard.stop_all();
            return Err(e);
        }
    };

    let target_pid = child.id();
    let target_st = proc_pid_starttime(target_pid);

    let mut watcher = FdEventWatcher::new()?;
    let _ = watcher.register_pid(target_pid);
    watcher.watch_path(&home);

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut preempted_by_batch = false;

    let exit_code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(1);
        }

        // Check if a merge-check batch started while `starve` was running
        if is_merge_batch_active() {
            preempted_by_batch = true;
            if pid_alive_with_starttime(target_pid, target_st) {
                signal_group_and_leader(target_pid, libc::SIGKILL)?;
            }
            child.wait()?;
            break 1;
        }

        let now = Instant::now();
        if now >= deadline {
            if pid_alive_with_starttime(target_pid, target_st) {
                signal_group_and_leader(target_pid, libc::SIGKILL)?;
            }
            child.wait()?;
            eprintln!("command exceeded the {timeout_secs}-second starve limit");
            break 124;
        }

        let rem_ms = deadline.saturating_duration_since(now).as_millis().min(200) as i32;
        watcher.wait_ms(rem_ms.max(1));
    };

    guard.stop_all();

    if preempted_by_batch {
        eprintln!("stopped starve burners: merge-check batch started");
    }

    let store = open_fleet_store()?;
    let because = format!(
        "starve cpus={} burners={} rc={exit_code} preempted={preempted_by_batch}: {}",
        plan.cpus,
        plan.burners,
        args.cmd.join(" ")
    );
    store.record_decision("watch", "starve", &plan.cpus, &because)?;
    emit_decision_event("system", "watch:starve", &because);

    Ok(exit_code)
}
