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

impl FleetStore {
    /// Opens or initializes the fleet tables inside `$TACHO_HOME/events.sqlite`.
    pub fn open(db_path: &Path) -> io::Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
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

        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_decisions_ts ON decisions(ts);",
            [],
        );

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

        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_decision_ticks_ts ON decision_ticks(ts);",
            [],
        );

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
        let _ = conn.execute("ALTER TABLE events ADD COLUMN counter_value INTEGER;", []);
        let _ = conn.execute("ALTER TABLE events ADD COLUMN custom_name TEXT;", []);
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts_ns);",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_events_kind_seq ON events(kind, seq);",
            [],
        );

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
        let _ = ev_body.encode(&mut buf);
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
        let _ = begin_body.encode(&mut begin_buf);
        let mut end_buf = Vec::new();
        let _ = end_body.encode(&mut end_buf);

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
}
