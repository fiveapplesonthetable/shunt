//! Daemon process implementation for `tacho daemon`.

use std::sync::atomic::Ordering;
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
    let mut config = Config::load_or_default(&paths.config_file()).unwrap_or_else(|e| {
        eprintln!(
            "[tacho-daemon] ignoring invalid config {}: {e}; using defaults",
            paths.config_file().display()
        );
        Config::default()
    });

    if let Some(otlp) = args.otlp {
        config.otlp_addr = Some(otlp);
    }
    if let Some(on_stall) = args.on_stall {
        config.detectors.on_stall = Some(on_stall);
    }

    // `Daemon::start` records `daemon.pid` atomically once its sockets are up.
    let mut daemon = Daemon::start(paths.clone(), config).map_err(|e| e.to_string())?;
    let pid_file = runtime.join("daemon.pid");

    // Wait until daemon receives shutdown signal via kernel signal-hook self-pipe (no sleep loop)
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGQUIT,
    ])
    .map_err(|e| e.to_string())?;

    for sig in signals.forever() {
        if sig == signal_hook::consts::SIGTERM
            || sig == signal_hook::consts::SIGINT
            || sig == signal_hook::consts::SIGQUIT
        {
            daemon.shutdown_flag.store(true, Ordering::Release);
            break;
        }
    }

    match std::fs::remove_file(&pid_file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "[tacho-daemon] failed to remove {}: {e}",
            pid_file.display()
        ),
    }
    daemon.stop();
    Ok(())
}
