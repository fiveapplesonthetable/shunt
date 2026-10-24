//! Unified SQLite WAL fleet state and decision journal stored in `$TACHO_HOME/events.sqlite`.
//!
//! Stores the fleet state document (`FleetState`) in the `fleet_state` table and records
//! every orchestrator/supervisor decision with its `because` reason into both the `decisions`
//! table and the Perfetto `events` timeline table.

use prost::Message;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tacho_core::decide::dispatch::{
    builtin_providers, AgentRun, Hold, ProviderSpec, PullFacts, TodoItem,
};
use tacho_core::decide::land::{TrainPrState, TrainRecord};
use tacho_core::decide::verify::{AreaRule, BatchRun, FailedRecord, FlakeEntry, PassedRecord};
use tacho_core::decide::watch::{CostRecord, PhaseNote, ProgressSample};
use tacho_core::decide::{DecisionPlan, DecisionState, WorldObservation};
use tacho_core::{Kind, Scrubber, TaskStatus};
use tacho_proto::tacho;

/// A recorded worktree allocation in fleet state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeEntry {
    pub slug: String,
    pub path: String,
    pub branch: String,
    pub base_sha: String,
    #[serde(default)]
    pub base_pr: Option<u64>,
    pub port: u16,
    pub port_last: u16,
    pub created_at: i64,
}

/// A recorded orchestrator or supervisor decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: i64,
    pub ts: i64,
    pub category: String,
    pub action: String,
    pub target: String,
    pub because: String,
}

/// Complete persistent fleet state stored in `$TACHO_HOME/events.sqlite`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetState {
    pub max_workers: usize,
    pub default_provider: String,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub todo: Vec<TodoItem>,
    pub agents: BTreeMap<String, AgentRun>,
    pub retired: BTreeSet<String>,
    pub holds: BTreeMap<String, Hold>,
    pub worktrees: BTreeMap<String, WorktreeEntry>,
    pub batches: Vec<BatchRun>,
    pub passed: BTreeMap<u64, PassedRecord>,
    pub failed: BTreeMap<u64, FailedRecord>,
    pub flakes: BTreeMap<String, FlakeEntry>,
    pub trains: Vec<TrainRecord>,
    pub train_pr: Option<TrainPrState>,
    pub notes: BTreeMap<String, Vec<PhaseNote>>,
    pub samples: BTreeMap<String, Vec<ProgressSample>>,
    pub nudged: BTreeSet<String>,
    pub rebase_attempts: HashMap<u64, String>,
    pub fix_notified: HashMap<u64, String>,
    pub costs: Vec<CostRecord>,
    pub area_rules: Vec<AreaRule>,
    pub generators: Vec<(String, String)>,
    pub open_prs_cache: Vec<PullFacts>,
    pub merged_prs: BTreeSet<u64>,
    pub closed_prs: BTreeSet<u64>,
    pub last_tick_ts: Option<i64>,
    pub last_watchdog_alert_ts: Option<i64>,
    pub providers: BTreeMap<String, ProviderSpec>,
    pub inbox: BTreeMap<String, Vec<String>>,
    pub db_up_cmd: Option<String>,
    pub workflow_path: Option<String>,
    pub workflow_jobs: BTreeMap<String, String>,
    pub workflow_expressions: BTreeMap<String, String>,
    pub train_compile_cmd: Option<String>,
    pub notified_failures: BTreeSet<String>,
    pub notified_deploys: BTreeSet<u64>,
    pub self_update_paths: Vec<String>,
    pub refused_self_update_sha: Option<String>,
    pub batch_timeout_secs: Option<u64>,
}

impl Default for FleetState {
    fn default() -> Self {
        Self {
            max_workers: 4,
            default_provider: "claude".to_string(),
            default_model: None,
            default_effort: None,
            todo: Vec::new(),
            agents: BTreeMap::new(),
            retired: BTreeSet::new(),
            holds: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            batches: Vec::new(),
            passed: BTreeMap::new(),
            failed: BTreeMap::new(),
            flakes: BTreeMap::new(),
            trains: Vec::new(),
            train_pr: None,
            notes: BTreeMap::new(),
            samples: BTreeMap::new(),
            nudged: BTreeSet::new(),
            rebase_attempts: HashMap::new(),
            fix_notified: HashMap::new(),
            costs: Vec::new(),
            area_rules: Vec::new(),
            generators: Vec::new(),
            open_prs_cache: Vec::new(),
            merged_prs: BTreeSet::new(),
            closed_prs: BTreeSet::new(),
            last_tick_ts: None,
            last_watchdog_alert_ts: None,
            providers: builtin_providers(),
            inbox: BTreeMap::new(),
            db_up_cmd: None,
            workflow_path: None,
            workflow_jobs: BTreeMap::new(),
            workflow_expressions: BTreeMap::new(),
            train_compile_cmd: None,
            notified_failures: BTreeSet::new(),
            notified_deploys: BTreeSet::new(),
            self_update_paths: vec!["crates/".to_string(), "scripts/".to_string()],
            refused_self_update_sha: None,
            batch_timeout_secs: None,
        }
    }
}

/// SQLite-backed store for [`FleetState`] and [`DecisionRecord`]s sharing `$TACHO_HOME/events.sqlite`.
pub struct FleetStore {
    conn: Connection,
}

fn sql_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Alias for [`FleetStore`] sharing `$TACHO_HOME/events.sqlite`.
pub type FleetDb = FleetStore;

impl FleetStore {
    /// Opens or initializes the fleet tables inside `$TACHO_HOME/events.sqlite`.
    pub fn open(db_path: &Path) -> io::Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(sql_err)?;

        let existing_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('fleet_state', 'decisions', 'decision_ticks', 'events')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if existing_tables < 4 {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS fleet_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    updated_at INTEGER NOT NULL,
                    state_json TEXT NOT NULL
                );",
                [],
            )
            .map_err(sql_err)?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS decisions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    category TEXT NOT NULL,
                    action TEXT NOT NULL,
                    target TEXT NOT NULL,
                    because TEXT NOT NULL
                );",
                [],
            )
            .map_err(sql_err)?;

            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_decisions_ts ON decisions(ts);",
                [],
            )
            .map_err(sql_err)?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS decision_ticks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    state_json TEXT NOT NULL,
                    obs_json TEXT NOT NULL,
                    plan_json TEXT NOT NULL
                );",
                [],
            )
            .map_err(sql_err)?;

            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_decision_ticks_ts ON decision_ticks(ts);",
                [],
            )
            .map_err(sql_err)?;

            // Also ensure the `events` table exists so offline fleet/kit commands can append
            // duration slices and state bands directly to `$TACHO_HOME/events.sqlite`.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS events (
                    seq INTEGER PRIMARY KEY,
                    ts_ns INTEGER NOT NULL,
                    kind INTEGER NOT NULL,
                    track_key TEXT NOT NULL,
                    cause_seq INTEGER,
                    agent TEXT,
                    body BLOB NOT NULL,
                    counter_value INTEGER,
                    custom_name TEXT
                );",
                [],
            )
            .map_err(sql_err)?;
            for col_sql in [
                "ALTER TABLE events ADD COLUMN counter_value INTEGER;",
                "ALTER TABLE events ADD COLUMN custom_name TEXT;",
            ] {
                if let Err(e) = conn.execute(col_sql, []) {
                    if !e.to_string().contains("duplicate column") {
                        return Err(sql_err(e));
                    }
                }
            }
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts_ns);",
                [],
            )
            .map_err(sql_err)?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_events_kind_seq ON events(kind, seq);",
                [],
            )
            .map_err(sql_err)?;
        }

        Ok(Self { conn })
    }

    /// Loads the current [`FleetState`] from SQLite, returning `FleetState::default()` if none exists yet.
    pub fn load(&self) -> io::Result<FleetState> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state_json FROM fleet_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match raw {
            Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
            None => Ok(FleetState::default()),
        }
    }

    /// Saves the updated [`FleetState`] atomically to SQLite.
    pub fn save(&self, state: &FleetState) -> io::Result<()> {
        let now = now_secs();
        let json = serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "INSERT INTO fleet_state (id, updated_at, state_json)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at, state_json = excluded.state_json",
            params![now, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Atomically mutates [`FleetState`] inside an immediate transaction and optionally records a decision.
    pub fn mutate<F, R>(
        &mut self,
        decision: Option<(&str, &str, &str, &str)>,
        f: F,
    ) -> io::Result<R>
    where
        F: FnOnce(&mut FleetState) -> R,
    {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT state_json FROM fleet_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let mut state: FleetState = raw
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let result = f(&mut state);
        let now = now_secs();
        let json = serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_string());
        tx.execute(
            "INSERT INTO fleet_state (id, updated_at, state_json)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at, state_json = excluded.state_json",
            params![now, json],
        )
        .map_err(sql_err)?;
        if let Some((category, action, target, because)) = decision {
            let scrubber = Scrubber::new();
            let clean_target = scrubber.scrub(target);
            let clean_because = scrubber.scrub(because);
            tx.execute(
                "INSERT INTO decisions (ts, category, action, target, because)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now, category, action, clean_target, clean_because],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(result)
    }

    /// Appends an explicit decision entry to the `decisions` journal.
    pub fn record_decision(
        &self,
        category: &str,
        action: &str,
        target: &str,
        because: &str,
    ) -> io::Result<()> {
        let now = now_secs();
        let scrubber = Scrubber::new();
        let clean_target = scrubber.scrub(target);
        let clean_because = scrubber.scrub(because);
        self.conn
            .execute(
                "INSERT INTO decisions (ts, category, action, target, because)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now, category, action, clean_target, clean_because],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Queries the most recent `limit` decisions, optionally filtered by `category` or `target`.
    pub fn recent_decisions(
        &self,
        limit: usize,
        category: Option<&str>,
        target: Option<&str>,
    ) -> io::Result<Vec<DecisionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, ts, category, action, target, because
                 FROM decisions
                 WHERE (?1 IS NULL OR category = ?1)
                   AND (?2 IS NULL OR target = ?2)
                 ORDER BY id DESC
                 LIMIT ?3",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![category, target, limit as i64], |r| {
                Ok(DecisionRecord {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    category: r.get(2)?,
                    action: r.get(3)?,
                    target: r.get(4)?,
                    because: r.get(5)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(sql_err)?);
        }
        out.reverse();
        Ok(out)
    }

    /// Appends a contiguous state band transition (`Kind::TaskState`) on `track_key` into the
    /// SQLite `events` table so `tacho trace` renders task and train lifecycles as state bands.
    pub fn emit_state_band(
        &self,
        track_key: &str,
        label: &str,
        status: TaskStatus,
        because: &str,
        ts_ns: u64,
    ) -> io::Result<()> {
        let next_seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |r| {
                r.get(0)
            })
            .map_err(sql_err)?;
        let last_ts: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(ts_ns), 0) FROM events WHERE track_key = ?1",
                params![track_key],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let eff_ts = (ts_ns as i64).max(last_ts + 1_000);
        let proto_state: tacho::TaskStatus = status.into();
        let ev_body = tacho::Event {
            body: Some(tacho::event::Body::TaskState(tacho::TaskState {
                state: Some(proto_state as i32),
                because: Some(because.chars().take(128).collect()),
            })),
        };
        let mut buf = Vec::new();
        ev_body
            .encode(&mut buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.conn
            .execute(
                "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, NULL, ?6)",
                params![
                    next_seq,
                    eff_ts,
                    Kind::TaskState as u32,
                    track_key,
                    &buf,
                    label
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Appends a paired duration slice (`Kind::Custom` + `Kind::CustomEnded`) on `track_key`
    /// into the SQLite `events` table so `tacho trace` renders batch runs, bisection probes,
    /// and orchestrator ticks as duration slices instead of zero-duration instants.
    pub fn emit_duration_slice(
        &self,
        track_key: &str,
        name: &str,
        start_ts_ns: u64,
        end_ts_ns: u64,
        because: &str,
    ) -> io::Result<()> {
        let next_seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |r| {
                r.get(0)
            })
            .map_err(sql_err)?;
        let last_ts: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(ts_ns), 0) FROM events WHERE track_key = ?1",
                params![track_key],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let begin_ts = (start_ts_ns as i64).max(last_ts + 1_000);
        let finish_ts = (end_ts_ns as i64).max(begin_ts + 1_000_000); // minimum 1ms visible slice
        let begin_body = tacho::Event {
            body: Some(tacho::event::Body::Custom(tacho::Custom {
                name: Some(name.chars().take(128).collect()),
                id: None,
                args: vec![tacho::CustomArg {
                    key: Some("because".to_string()),
                    value: Some(because.chars().take(256).collect()),
                }],
            })),
        };
        let end_body = tacho::Event {
            body: Some(tacho::event::Body::CustomEnded(tacho::CustomEnded {
                id: None,
                args: Vec::new(),
                abandoned: Some(false),
            })),
        };
        let mut begin_buf = Vec::new();
        begin_body
            .encode(&mut begin_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut end_buf = Vec::new();
        end_body
            .encode(&mut end_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.conn
            .execute(
                "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, NULL, ?6)",
                params![
                    next_seq,
                    begin_ts,
                    Kind::Custom as u32,
                    track_key,
                    &begin_buf,
                    name
                ],
            )
            .map_err(sql_err)?;
        self.conn
            .execute(
                "INSERT INTO events (seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL, NULL)",
                params![
                    next_seq + 1,
                    finish_ts,
                    Kind::CustomEnded as u32,
                    track_key,
                    next_seq,
                    &end_buf
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Records a complete orchestrator decision tick with both its input state and its output plan
    /// into `decision_ticks` so the tick can be deterministically replayed offline.
    pub fn record_decision_tick(
        &self,
        ts: i64,
        state: &DecisionState,
        obs: &WorldObservation,
        plan: &DecisionPlan,
    ) -> io::Result<i64> {
        let state_json = serde_json::to_string(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let obs_json = serde_json::to_string(obs)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let plan_json = serde_json::to_string(plan)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.conn
            .execute(
                "INSERT INTO decision_ticks (ts, state_json, obs_json, plan_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![ts, state_json, obs_json, plan_json],
            )
            .map_err(sql_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Loads the latest logged decision tick for replay.
    pub fn load_latest_decision_tick(
        &self,
    ) -> io::Result<Option<(i64, DecisionState, WorldObservation, DecisionPlan)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, state_json, obs_json, plan_json
                 FROM decision_ticks
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .map_err(sql_err)?;
        let mut rows = stmt.query([]).map_err(sql_err)?;
        if let Some(row) = rows.next().map_err(sql_err)? {
            let id: i64 = row.get(0).map_err(sql_err)?;
            let s_json: String = row.get(1).map_err(sql_err)?;
            let o_json: String = row.get(2).map_err(sql_err)?;
            let p_json: String = row.get(3).map_err(sql_err)?;
            let st = serde_json::from_str(&s_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let obs = serde_json::from_str(&o_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let plan = serde_json::from_str(&p_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some((id, st, obs, plan)))
        } else {
            Ok(None)
        }
    }

    /// Loads a specific logged decision tick by ID.
    pub fn load_decision_tick(
        &self,
        tick_id: i64,
    ) -> io::Result<(DecisionState, WorldObservation, DecisionPlan)> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT state_json, obs_json, plan_json
                 FROM decision_ticks
                 WHERE id = ?1",
            )
            .map_err(sql_err)?;
        let mut rows = stmt.query([tick_id]).map_err(sql_err)?;
        if let Some(row) = rows.next().map_err(sql_err)? {
            let s_json: String = row.get(0).map_err(sql_err)?;
            let o_json: String = row.get(1).map_err(sql_err)?;
            let p_json: String = row.get(2).map_err(sql_err)?;
            let st = serde_json::from_str(&s_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let obs = serde_json::from_str(&o_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let plan = serde_json::from_str(&p_json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok((st, obs, plan))
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("decision tick {tick_id} not found"),
            ))
        }
    }

    /// Prunes decision ticks older than `cutoff_secs`.
    pub fn prune_decision_ticks_older_than(&mut self, cutoff_secs: i64) -> io::Result<usize> {
        let deleted = self
            .conn
            .execute("DELETE FROM decision_ticks WHERE ts < ?1", [cutoff_secs])
            .map_err(sql_err)?;
        Ok(deleted)
    }
}

/// Converts persistent [`FleetState`] into the pure [`DecisionState`] input to [`tacho_core::decide::decide`].
pub fn decision_state_of(state: &FleetState) -> DecisionState {
    DecisionState {
        max_parallel: state.max_workers,
        todo: state.todo.clone(),
        agents: state.agents.clone(),
        retired: state.retired.clone(),
        holds: state.holds.clone(),
        passed: state.passed.clone(),
        failed: state.failed.clone(),
        merged_prs: state.merged_prs.clone(),
        closed_prs: state.closed_prs.clone(),
        batches: state.batches.clone(),
        flakes: state.flakes.clone(),
        samples: state.samples.clone(),
        nudged: state.nudged.clone(),
        rebase_attempts: state.rebase_attempts.clone(),
        fix_notified: state.fix_notified.clone(),
        notified_keys: state.notified_failures.clone(),
        area_rules: state.area_rules.clone(),
        refused_self_update_sha: state.refused_self_update_sha.clone(),
        last_tick_ts: state.last_tick_ts,
        inbox: state.inbox.clone(),
        last_watchdog_alert_ts: state.last_watchdog_alert_ts,
        tidy_min_age_secs: 3_600,
    }
}

/// Batches that finished as a definite pass or fail since the previous tick.
///
/// Flakes and still-running batches are excluded so a flake is never fed back as a pass,
/// and older outcomes are not replayed on every tick.
pub fn new_batch_outcomes(state: &FleetState) -> Vec<tacho_core::decide::BatchOutcomeObs> {
    let prev_tick_ts = state.last_tick_ts.unwrap_or(0);
    state
        .batches
        .iter()
        .filter(|b| b.finished_at.is_some_and(|t| t > prev_tick_ts))
        .filter(|b| b.status == "passed" || b.status == "failed")
        .map(|b| tacho_core::decide::BatchOutcomeObs {
            batch_id: b.id.clone(),
            passed: b.status == "passed",
            failed_area: b.areas.first().cloned(),
            log_output: b.log.clone(),
            tree_sha: b.tree.clone().unwrap_or_default(),
        })
        .collect()
}

fn has_git_dir(start: &Path) -> bool {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return true;
        }
        cur = dir.parent();
    }
    false
}

/// Resolves `HEAD` SHA in `repo` when `repo` is inside a git checkout.
pub fn git_head_sha(repo: &Path) -> Option<String> {
    if !has_git_dir(repo) {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Resolves a non-empty `main_sha` from fleet state, active worktrees/cwds, or git,
/// defaulting to 40 zeros when running outside any git repository so `main_sha` is never empty.
pub fn resolve_autonomous_main_sha(
    home_dir: &Path,
    state: &FleetState,
    live_state: &tacho_core::LiveState,
) -> String {
    for b in state.batches.iter().rev() {
        if !b.main_sha.trim().is_empty() {
            return b.main_sha.clone();
        }
    }
    for p in state.passed.values() {
        if !p.main_sha.trim().is_empty() {
            return p.main_sha.clone();
        }
    }
    for w in state.worktrees.values() {
        if !w.base_sha.trim().is_empty() {
            return w.base_sha.clone();
        }
        if !w.path.is_empty() {
            if let Some(sha) = git_head_sha(Path::new(&w.path)) {
                return sha;
            }
        }
    }
    for a in state.agents.values() {
        if !a.worktree.is_empty() {
            if let Some(sha) = git_head_sha(Path::new(&a.worktree)) {
                return sha;
            }
        }
    }
    for a in live_state.agents.values() {
        if let Some(ref cwd) = a.cwd {
            if let Some(sha) = git_head_sha(Path::new(cwd)) {
                return sha;
            }
        }
    }
    if let Some(sha) = git_head_sha(home_dir) {
        return sha;
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(sha) = git_head_sha(&cwd) {
            return sha;
        }
    }
    "0".repeat(40)
}

/// Scans `/proc` for runaway CPU burner processes (`yes`, `stress`, `stress-ng`)
/// running `>= BURNER_MIN_AGE_SECS` that were not launched by `tacho starve` (`TACHO_STARVE=1`).
pub fn observe_proc_burners() -> Vec<tacho_core::decide::BurnerProcess> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let uptime_secs = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    let clk_tck = crate::sampler::get_clock_ticks_per_sec().max(1) as f64;
    let page_size = crate::sampler::get_page_size();
    let mut stat_buf = [0u8; 1024];
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let Some(stat) =
            crate::sampler::read_process_stat_and_comm_slice(pid, page_size, &mut stat_buf)
        else {
            continue;
        };
        let comm = String::from_utf8_lossy(&stat_buf[stat.comm_start..stat.comm_end]).into_owned();
        if !matches!(comm.as_str(), "yes" | "stress" | "stress-ng" | "timeout") {
            continue;
        }
        let start_secs = (stat.start_ticks as f64) / clk_tck;
        let elapsed_secs = (uptime_secs - start_secs).max(0.0) as i64;
        if elapsed_secs < tacho_core::decide::watch::BURNER_MIN_AGE_SECS {
            continue;
        }
        let args = crate::sampler::read_process_cmdline(pid, &comm);
        let Some(burner_comm) = tacho_core::decide::watch::burner_command(&comm, &args) else {
            continue;
        };
        let env_starve = std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| {
                bytes
                    .split(|&b| b == 0)
                    .any(|kv| kv == b"TACHO_STARVE=1" || kv.starts_with(b"TACHO_STARVE=1"))
            })
            .unwrap_or(false);
        if env_starve {
            continue;
        }
        out.push(tacho_core::decide::BurnerProcess {
            pid,
            comm: burner_comm,
            elapsed_secs,
            starttime: Some(stat.start_ticks),
        });
    }
    out
}

/// Delivers a `tell` message emitted by `decide` to `$TACHO_HOME/inbox/<agent_key>.jsonl`
/// (and `<worktree>/.tacho-inbox.md` when a worktree exists) and records the decision.
pub fn deliver_tell_message(
    home_dir: &Path,
    store: &FleetStore,
    state: &FleetState,
    live_state: &tacho_core::LiveState,
    agent_key: &str,
    msg: &str,
) -> io::Result<()> {
    use std::io::Write;
    let now = now_secs();
    let inbox_dir = home_dir.join("inbox");
    std::fs::create_dir_all(&inbox_dir)?;
    let safe_key: String = agent_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let inbox_path = inbox_dir.join(format!("{safe_key}.jsonl"));
    let mut line = serde_json::to_vec(&serde_json::json!({
        "ts": now,
        "to": agent_key,
        "msg": msg,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&inbox_path)?;
    f.write_all(&line)?;

    let worktree_dir = state
        .agents
        .get(agent_key)
        .map(|a| a.worktree.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            state
                .worktrees
                .get(agent_key)
                .map(|w| w.path.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            live_state
                .agents
                .get(agent_key)
                .and_then(|a| a.cwd.as_deref())
                .filter(|s| !s.is_empty())
        });
    if let Some(wt) = worktree_dir {
        let wt_path = Path::new(wt);
        if wt_path.is_dir() {
            let md_path = wt_path.join(".tacho-inbox.md");
            if let Ok(mut mf) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(md_path)
            {
                let _ = writeln!(mf, "\n## Message ({now})\n{msg}");
            }
        }
    }

    store.record_decision("dispatch", "tell", agent_key, msg)
}

fn remove_worktree_dir(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if has_git_dir(parent) {
            let _ = std::process::Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(path)
                .current_dir(parent)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// Observes the world, runs [`tacho_core::decide::decide`], and (unless `dry_run`) records the tick
/// to `decision_ticks` and executes every resulting `tell_actions`, `tidy_decisions`, and `burner_kills`.
pub fn execute_decide_tick(
    home_dir: &Path,
    live_state: &tacho_core::LiveState,
    explicit_main_sha: Option<String>,
    dry_run: bool,
) -> io::Result<(Option<i64>, DecisionState, WorldObservation, DecisionPlan)> {
    let mut store = FleetStore::open(&home_dir.join("events.sqlite"))?;
    let state = store.load()?;
    let now = now_secs();

    let mut live_keys = BTreeSet::new();
    for (agent_id, a) in &live_state.agents {
        if a.state != "Exited" {
            live_keys.insert(agent_id.clone());
        }
    }
    for (agent_key, a) in &state.agents {
        if a.finished_at.is_none()
            && a.pid
                .is_some_and(|pid| pid > 0 && Path::new(&format!("/proc/{pid}")).exists())
        {
            live_keys.insert(agent_key.clone());
        }
    }

    let mut decision_state = decision_state_of(&state);
    let mut agent_samples = BTreeMap::new();
    for key in &live_keys {
        if let Some(live_agent) = live_state.agents.get(key) {
            let is_stalled = live_agent.state == "Stalled" || !live_agent.flags.is_empty();
            if is_stalled && !decision_state.nudged.contains(key) {
                let entry = decision_state.samples.entry(key.clone()).or_default();
                if entry.is_empty() {
                    entry.push(ProgressSample {
                        ts: now - 2_800,
                        log_bytes: 0,
                        head_sha: String::new(),
                        dirty_files: 0,
                        cpu_ticks: 0,
                    });
                    entry.push(ProgressSample {
                        ts: now - 900,
                        log_bytes: 0,
                        head_sha: String::new(),
                        dirty_files: 0,
                        cpu_ticks: 1,
                    });
                }
                agent_samples.insert(
                    key.clone(),
                    ProgressSample {
                        ts: now,
                        log_bytes: 0,
                        head_sha: String::new(),
                        dirty_files: 0,
                        cpu_ticks: 2,
                    },
                );
            } else {
                agent_samples.insert(
                    key.clone(),
                    ProgressSample {
                        ts: now,
                        log_bytes: (live_agent.timeline.len() as u64) * 64,
                        head_sha: String::new(),
                        dirty_files: u32::from(live_agent.last_edit_ts_ns.is_some()),
                        cpu_ticks: (live_agent.tokens_in + live_agent.tokens_out).max(1),
                    },
                );
            }
        }
    }

    let mut tidy_snapshot = tacho_core::decide::watch::TidySnapshot {
        now,
        ..Default::default()
    };
    for a in state.agents.values() {
        if live_keys.contains(&a.key) && !a.worktree.is_empty() {
            tidy_snapshot
                .live_agent_worktrees
                .insert(a.worktree.clone());
        }
    }
    for a in live_state.agents.values() {
        if a.state != "Exited" {
            if let Some(ref cwd) = a.cwd {
                tidy_snapshot.live_agent_worktrees.insert(cwd.clone());
            }
        }
    }
    for h_key in state.holds.keys() {
        tidy_snapshot.held_paths.insert(h_key.clone());
        if let Some(wt) = state.worktrees.get(h_key) {
            tidy_snapshot.held_paths.insert(wt.path.clone());
        }
    }
    for wt in state.worktrees.values() {
        let age_secs = (now - wt.created_at).max(0);
        let pr_merged_or_closed = wt
            .base_pr
            .is_some_and(|pr| state.merged_prs.contains(&pr) || state.closed_prs.contains(&pr));
        tidy_snapshot
            .worktrees
            .push(tacho_core::decide::watch::WorktreeCandidate {
                path: wt.path.clone(),
                branch: wt.branch.clone(),
                age_secs,
                dirty: false,
                unpushed_commits: 0,
                pr_merged_or_closed,
            });
    }

    let main_sha = explicit_main_sha
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| resolve_autonomous_main_sha(home_dir, &state, live_state));

    let world_obs = WorldObservation {
        now,
        main_sha,
        open_prs: state.open_prs_cache.clone(),
        merged_prs: state.merged_prs.clone(),
        closed_prs: state.closed_prs.clone(),
        live_keys,
        agent_samples,
        batch_outcomes: new_batch_outcomes(&state),
        tidy_snapshot,
        burners: observe_proc_burners(),
        ..Default::default()
    };

    let plan = tacho_core::decide::decide(&decision_state, &world_obs);
    let mut tick_id = None;
    if !dry_run {
        let id = store.record_decision_tick(now, &decision_state, &world_obs, &plan)?;
        tick_id = Some(id);

        for (agent_key, msg) in &plan.tell_actions {
            deliver_tell_message(home_dir, &store, &state, live_state, agent_key, msg)?;
        }

        let mut removed_paths = BTreeSet::new();
        for td in &plan.tidy_decisions {
            match td {
                tacho_core::decide::watch::TidyDecision::RemoveTree { path, because, .. }
                | tacho_core::decide::watch::TidyDecision::RemoveCheckout {
                    path, because, ..
                } => {
                    if let Err(e) = remove_worktree_dir(Path::new(path)) {
                        eprintln!("[tacho-daemon] failed to remove worktree '{}': {}", path, e);
                    }
                    removed_paths.insert(path.clone());
                    store.record_decision("watch", "remove_worktree", path, because)?;
                }
                _ => {}
            }
        }

        let page_size = crate::sampler::get_page_size();
        let mut stat_buf = [0u8; 1024];
        for burner in &plan.burner_kills {
            let still_matches = match burner.starttime {
                Some(expected_st) => crate::sampler::read_process_stat_and_comm_slice(
                    burner.pid,
                    page_size,
                    &mut stat_buf,
                )
                .is_some_and(|st| st.start_ticks == expected_st),
                None => true,
            };
            if still_matches && burner.pid > 1 {
                unsafe {
                    libc::kill(burner.pid as i32, libc::SIGKILL);
                }
                store.record_decision(
                    "watch",
                    "kill_burner",
                    &format!("pid:{}", burner.pid),
                    &format!(
                        "killed runaway burner {} ({}s)",
                        burner.comm, burner.elapsed_secs
                    ),
                )?;
            }
        }

        store.mutate(None, |st| {
            let mut todo_with_dispatched = plan.dispatched_tasks.clone();
            for item in &plan.next_state.todo {
                if !todo_with_dispatched.iter().any(|t| t.key == item.key) {
                    todo_with_dispatched.push(item.clone());
                }
            }
            st.todo = todo_with_dispatched;
            st.agents = plan.next_state.agents.clone();
            for r in &plan.next_state.retired {
                st.retired.insert(r.clone());
            }
            for (hk, hv) in &plan.next_state.holds {
                st.holds.entry(hk.clone()).or_insert_with(|| hv.clone());
            }
            st.passed = plan.next_state.passed.clone();
            st.failed = plan.next_state.failed.clone();
            st.merged_prs = plan.next_state.merged_prs.clone();
            st.closed_prs = plan.next_state.closed_prs.clone();
            st.batches = plan.next_state.batches.clone();
            st.flakes = plan.next_state.flakes.clone();
            st.samples = plan.next_state.samples.clone();
            st.nudged = plan.next_state.nudged.clone();
            st.rebase_attempts = plan.next_state.rebase_attempts.clone();
            st.fix_notified = plan.next_state.fix_notified.clone();
            st.notified_failures = plan.next_state.notified_keys.clone();
            st.inbox = plan.next_state.inbox.clone();
            st.last_tick_ts = plan.next_state.last_tick_ts;
            st.last_watchdog_alert_ts = plan.next_state.last_watchdog_alert_ts;
            if !removed_paths.is_empty() {
                st.worktrees.retain(|_, w| !removed_paths.contains(&w.path));
                for a in st.agents.values_mut() {
                    if removed_paths.contains(&a.worktree) {
                        a.worktree.clear();
                    }
                }
            }
        })?;
    }

    Ok((tick_id, decision_state, world_obs, plan))
}

/// Runs an autonomous world-observation and `decide` pass using the current [`tacho_core::LiveState`].
pub fn run_autonomous_decide_tick_with_live(
    home_dir: &Path,
    live_state: &tacho_core::LiveState,
) -> io::Result<DecisionPlan> {
    let (_, _, _, plan) = execute_decide_tick(home_dir, live_state, None, false)?;
    Ok(plan)
}

/// Runs an autonomous world-observation and `decide` pass reading live agent states from `recorder`.
pub fn run_autonomous_decide_tick(
    home_dir: &Path,
    recorder: &std::sync::Arc<std::sync::Mutex<tacho_core::Recorder>>,
) -> io::Result<DecisionPlan> {
    let live_state = recorder
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .live_state()
        .clone();
    run_autonomous_decide_tick_with_live(home_dir, &live_state)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fleet_store_roundtrip_and_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.sqlite");
        let mut store = FleetStore::open(&db).unwrap();

        store
            .mutate(
                Some(("dispatch", "queue", "feat-1", "queued by user")),
                |st| {
                    st.todo.push(TodoItem {
                        key: "feat-1".to_string(),
                        brief: "Add feature 1".to_string(),
                        area: "core".to_string(),
                        ..Default::default()
                    });
                },
            )
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.todo.len(), 1);
        assert_eq!(loaded.todo[0].key, "feat-1");

        let decisions = store.recent_decisions(10, None, None).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, "queue");
        assert_eq!(decisions[0].target, "feat-1");
        assert_eq!(decisions[0].because, "queued by user");
    }

    #[test]
    fn test_autonomous_decide_tick_records_tick_delivers_tell_and_removes_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let db = home.join("events.sqlite");
        let mut store = FleetStore::open(&db).unwrap();

        let stale_wt_dir = home.join("wt-stale");
        std::fs::create_dir_all(&stale_wt_dir).unwrap();
        let live_wt_dir = home.join("wt-live");
        std::fs::create_dir_all(&live_wt_dir).unwrap();

        store
            .mutate(None, |st| {
                st.inbox
                    .entry("agent-alpha".to_string())
                    .or_default()
                    .push("Focus on fixing the parser edge case".to_string());
                st.worktrees.insert(
                    "wt-stale".to_string(),
                    WorktreeEntry {
                        slug: "wt-stale".to_string(),
                        path: stale_wt_dir.to_string_lossy().to_string(),
                        branch: "feat/stale".to_string(),
                        base_sha: "a".repeat(40),
                        base_pr: None,
                        port: 0,
                        port_last: 0,
                        created_at: 100,
                    },
                );
            })
            .unwrap();

        let mut recorder = tacho_core::Recorder::new();
        let mut events = Vec::new();
        recorder.apply(
            tacho_core::Input::RunStart {
                source_ts_ns: Some(1_000_000_000),
                provider: "claude".to_string(),
                session_id: "sess-1".to_string(),
                agent: Some("agent-alpha".to_string()),
                model: Some("claude-sonnet".to_string()),
                task: Some("Fix parser".to_string()),
                cwd: Some(live_wt_dir.to_string_lossy().to_string()),
                pid: Some(std::process::id()),
                role: None,
                parent_session: None,
                git_tree: None,
            },
            1_000_000_000,
            &mut events,
        );
        let shared_recorder = std::sync::Arc::new(std::sync::Mutex::new(recorder));

        let plan = run_autonomous_decide_tick(home, &shared_recorder).unwrap();
        assert!(
            plan.tell_actions.contains(&(
                "agent-alpha".to_string(),
                "Focus on fixing the parser edge case".to_string()
            )),
            "expected tell_actions in plan, got: {:?}",
            plan.tell_actions
        );
        assert!(
            !stale_wt_dir.exists(),
            "stale worktree should have been removed by decide tidy action"
        );

        let inbox_file = home.join("inbox/agent-alpha.jsonl");
        assert!(inbox_file.exists(), "expected delivered inbox jsonl file");
        let inbox_contents = std::fs::read_to_string(&inbox_file).unwrap();
        assert!(inbox_contents.contains("Focus on fixing the parser edge case"));

        let (_, _, recorded_obs, recorded_plan) =
            store.load_latest_decision_tick().unwrap().unwrap();
        assert_eq!(recorded_obs.main_sha, "a".repeat(40));
        assert!(!recorded_obs.main_sha.is_empty());
        assert_eq!(recorded_plan.tell_actions, plan.tell_actions);
    }
}
