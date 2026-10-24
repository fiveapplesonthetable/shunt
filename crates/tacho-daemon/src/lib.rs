//! Tachograph flight recorder daemon library crate.

pub mod appender;
pub mod cgroup;
pub mod config;
pub mod daemon;
pub mod fleet;
pub mod log;
pub mod paths;
pub mod recorder;
pub mod sampler;
pub mod server;
pub mod snapshot;
#[cfg(test)]
pub(crate) mod test_util;

pub use appender::{spawn_appender, ActiveRun, AppenderCommand, AppenderHandle, AppenderHooks};
pub use config::{Config, DetectorConfig, RingConfig, SamplerConfig};
pub use daemon::Daemon;
pub use fleet::{DecisionRecord, FleetState, FleetStore, WorktreeEntry};
pub use log::EventLog;
pub use paths::DaemonPaths;
pub use recorder::{render_report, render_status, AgentLiveState, LiveState, Recorder};
pub use sampler::spawn_sampler;
pub use server::{DaemonLock, DaemonServer};
pub use snapshot::{
    build_registry_from_conn, rebuild_from_log, write_snapshot, write_snapshot_conn, SnapshotStats,
};
