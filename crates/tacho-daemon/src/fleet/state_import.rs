//! Atomic, lossless imports into an empty controller store.

use std::collections::BTreeSet;
use std::io;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{now_secs, sql_err, FleetState, FleetStore};

/// Neutral import envelope produced by a repository's migration adapter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Import contract version.
    pub version: u32,
    /// Source controller name.
    pub source: String,
    /// Source capture time in epoch seconds.
    pub captured_at: i64,
    /// Target state, checked for fields that deserialization would discard.
    pub state: Value,
    /// Live source features the adapter could not represent.
    pub unmapped_active: Vec<String>,
    /// Original source records retained without interpretation.
    pub archive: Value,
}

/// Receipt for an imported immutable source snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReceipt {
    /// Content identifier of the complete import envelope.
    pub digest: String,
    /// Whether this exact snapshot was already imported.
    pub already_imported: bool,
    /// Number of queued tasks preserved.
    pub tasks: usize,
    /// Number of holds preserved.
    pub holds: usize,
    /// Number of historical run records preserved.
    pub agents: usize,
    /// Number of batch records preserved.
    pub batches: usize,
    /// Number of complete proof records preserved.
    pub complete_passes: usize,
    /// Number of unresolved project decisions preserved.
    pub attention: usize,
}

fn preserved(original: &Value, canonical: &Value, path: &str) -> io::Result<()> {
    match (original, canonical) {
        (Value::Object(source), Value::Object(target)) => {
            for (key, value) in source {
                let nested = format!("{path}.{key}");
                let current = target.get(key).ok_or_else(|| {
                    io::Error::other(format!("state import would discard {nested}"))
                })?;
                preserved(value, current, &nested)?;
            }
        }
        (Value::Array(source), Value::Array(target)) if source.len() == target.len() => {
            for (index, (value, current)) in source.iter().zip(target).enumerate() {
                preserved(value, current, &format!("{path}[{index}]"))?;
            }
        }
        _ if original == canonical => {}
        _ => {
            return Err(io::Error::other(format!(
                "state import would discard or change {path}"
            )))
        }
    }
    Ok(())
}

/// Validates the envelope and every claimed pass before opening or changing a controller.
pub fn validate(raw: &[u8]) -> io::Result<(Snapshot, FleetState, ImportReceipt)> {
    let snapshot: Snapshot = serde_json::from_slice(raw).map_err(io::Error::other)?;
    if snapshot.version != 1 || snapshot.source.trim().is_empty() || snapshot.captured_at < 0 {
        return Err(io::Error::other(
            "unsupported or incomplete state import contract",
        ));
    }
    if !snapshot.unmapped_active.is_empty() {
        return Err(io::Error::other("unmapped live work prevents state import"));
    }
    let state: FleetState =
        serde_json::from_value(snapshot.state.clone()).map_err(io::Error::other)?;
    preserved(
        &snapshot.state,
        &serde_json::to_value(&state).map_err(io::Error::other)?,
        "state",
    )?;
    if let Some(project) = &state.project {
        project.validate().map_err(io::Error::other)?;
    }
    if state.batches.iter().any(|batch| batch.status == "running") {
        return Err(io::Error::other(
            "drain active batches before importing controller state",
        ));
    }
    for run in state.agents.values() {
        if run.finished_at.is_none()
            && run
                .pid
                .is_some_and(|pid| crate::exec::identity::pid_alive_matching(pid, run.starttime))
        {
            return Err(io::Error::other(
                "drain active workers before importing controller state",
            ));
        }
        if run.outcome == tacho_core::decide::dispatch::RunOutcome::Completed
            && run.pr_source.as_deref() == Some("guessed")
        {
            return Err(io::Error::other(
                "unconfirmed PR attribution cannot import as completed work",
            ));
        }
    }
    let ids: BTreeSet<_> = state.batches.iter().map(|batch| &batch.id).collect();
    if ids.len() != state.batches.len() {
        return Err(io::Error::other("duplicate imported batch identities"));
    }
    for (number, proof) in &state.passed {
        let batch = state
            .batches
            .iter()
            .find(|batch| batch.id == proof.batch_id)
            .ok_or_else(|| io::Error::other("imported proof has no matching batch"))?;
        let areas: BTreeSet<_> = batch.areas.iter().collect();
        if proof.pr != *number
            || proof.sha.is_empty()
            || proof.tree.is_empty()
            || proof.main_sha.is_empty()
            || batch.status != "passed"
            || batch.main_sha != proof.main_sha
            || batch.tree.as_deref() != Some(&proof.tree)
            || batch.shas.get(number) != Some(&proof.sha)
            || !batch.prs.contains(number)
            || areas.is_empty()
            || areas != proof.areas.iter().collect()
        {
            return Err(io::Error::other(
                "imported proof lacks complete source, membership or area identity",
            ));
        }
    }
    let receipt = ImportReceipt {
        digest: tacho_core::project::content_id(raw),
        already_imported: false,
        tasks: state.todo.len(),
        holds: state.holds.len(),
        agents: state.agents.len(),
        batches: state.batches.len(),
        complete_passes: state.passed.len(),
        attention: state.external_attention.len(),
    };
    Ok((snapshot, state, receipt))
}

impl FleetStore {
    /// Imports once into an empty store; replaying the same snapshot never rewinds current state.
    pub fn import_snapshot(&mut self, raw: &[u8]) -> io::Result<ImportReceipt> {
        let (snapshot, mut state, mut receipt) = validate(raw)?;
        // Imported history is visible without replaying its old owner notifications.
        for batch in &state.batches {
            state
                .notified_failures
                .insert(format!("batch-red:{}", batch.id));
        }
        for item in tacho_core::decide::watch::needs_you(
            &state.agents,
            &state.todo,
            &state.retired,
            &state.holds,
            &BTreeSet::new(),
            &state.open_prs_cache,
            &state.passed,
            &state.failed,
            snapshot.captured_at,
        ) {
            state.notified_failures.insert(format!(
                "attention:{}:{}:{}",
                item.kind, item.target, item.summary
            ));
        }
        for (key, summary) in &state.external_attention {
            state
                .notified_failures
                .insert(format!("attention:project:{key}:{summary}"));
        }
        let transaction = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS fleet_imports (digest TEXT PRIMARY KEY, source TEXT NOT NULL, captured_at INTEGER NOT NULL, imported_at INTEGER NOT NULL, snapshot BLOB NOT NULL, receipt TEXT NOT NULL)").map_err(sql_err)?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT receipt FROM fleet_imports WHERE digest = ?1",
                [&receipt.digest],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(existing) = existing {
            receipt = serde_json::from_str(&existing).map_err(io::Error::other)?;
            receipt.already_imported = true;
            transaction.commit().map_err(sql_err)?;
            return Ok(receipt);
        }
        let initialized: bool = transaction
            .query_row("SELECT EXISTS(SELECT 1 FROM fleet_state)", [], |row| {
                row.get(0)
            })
            .map_err(sql_err)?;
        if initialized {
            return Err(io::Error::other(
                "state import cannot overwrite an initialized controller",
            ));
        }
        let now = now_secs();
        transaction
            .execute(
                "INSERT INTO fleet_state (id, updated_at, state_json) VALUES (1, ?1, ?2)",
                params![
                    now,
                    serde_json::to_string(&state).map_err(io::Error::other)?
                ],
            )
            .map_err(sql_err)?;
        transaction.execute("INSERT INTO fleet_imports (digest, source, captured_at, imported_at, snapshot, receipt) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![receipt.digest, snapshot.source, snapshot.captured_at, now, raw, serde_json::to_string(&receipt).map_err(io::Error::other)?]).map_err(sql_err)?;
        transaction.execute("INSERT INTO decisions (ts, category, action, target, because) VALUES (?1, 'migration', 'import', ?2, 'validated snapshot imported without resetting live state')", params![now, receipt.digest]).map_err(sql_err)?;
        transaction.commit().map_err(sql_err)?;
        Ok(receipt)
    }
}
