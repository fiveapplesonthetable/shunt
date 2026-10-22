//! Daemon process implementation for `tacho daemon`.

use std::sync::atomic::Ordering;
use std::time::Duration;
use tacho_daemon::{Config, Daemon, DaemonPaths};

use crate::cli::DaemonArgs;
use crate::paths::{ensure_dir_0700, resolve_home, resolve_runtime};

/// Runs the tachograph flight recorder daemon server.
pub fn run_daemon(args: DaemonArgs) -> Result<(), String> {
    let home = resolve_home();
    let runtime = resolve_runtime();

    ensure_dir_0700(&home).map_err(|e| e.to_string())?;
    ensure_dir_0700(&runtime).map_err(|e| e.to_string())?;

    let paths = DaemonPaths::custom(&home, &runtime).map_err(|e| e.to_string())?;
    let mut config = Config::load_or_default(&paths.config_file()).unwrap_or_default();

    if let Some(otlp) = args.otlp {
        config.otlp_addr = Some(otlp);
    }
    if let Some(on_stall) = args.on_stall {
        config.detectors.on_stall = Some(on_stall);
    }

    let mut daemon = Daemon::start(paths.clone(), config).map_err(|e| e.to_string())?;

    // Record daemon.pid for reliable discovery
    let pid_file = runtime.join("daemon.pid");
    let _ = std::fs::write(&pid_file, std::process::id().to_string());

    // Wait until daemon receives shutdown signal
    while !daemon.shutdown_flag.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(&pid_file);
    daemon.stop();
    Ok(())
}
