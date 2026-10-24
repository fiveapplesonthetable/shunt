//! Snapshot generation from SQLite WAL event log.
//!
//! Produces Perfetto-compatible `.perfetto-trace` byte streams adhering to design section 9.
//! The SQLite event log is the single source of truth; snapshots stream directly from
//! SQLite rows to Perfetto TracePackets without buffering the dataset in memory.

mod projector;

use prost::Message;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;
use tacho_core::{encode_snapshot_header, Event, Kind, Recorder, TrackKind, TrackRegistry};
use tacho_proto::tacho;

use projector::{run_prefix_of, task_state_track, Sink, SnapshotProjector, MESSAGES_TRACK};

/// Statistics about an emitted snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Total bytes written to the output stream.
    pub bytes: u64,
    /// Total number of events encoded and emitted.
    pub events: u64,
}

fn sql_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e.to_string())
}

fn kind_of(kind_int: u32) -> Kind {
    Kind::ALL
        .get(kind_int as usize)
        .copied()
        .unwrap_or(Kind::Daemon)
}

/// Columns selected by every event query, in this order.
const EVENT_COLUMNS: &str =
    "seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name";

/// Decodes a row selected with [`EVENT_COLUMNS`]. A corrupt body decodes as empty rather
/// than failing the whole snapshot.
fn event_from_row(row: &rusqlite::Row<'_>) -> io::Result<Event> {
    let body_bytes: &[u8] = row
        .get_ref(6)
        .map_err(sql_err)?
        .as_bytes()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let body = tacho::Event::decode(body_bytes).unwrap_or(tacho::Event { body: None });
    let mut ev = Event::new(
        row.get(0).map_err(sql_err)?,
        row.get(1).map_err(sql_err)?,
        kind_of(row.get(2).map_err(sql_err)?),
        row.get::<_, String>(3).map_err(sql_err)?,
        row.get(4).map_err(sql_err)?,
        row.get(5).map_err(sql_err)?,
        body,
    );
    if let Some(c) = row.get::<_, Option<i64>>(7).map_err(sql_err)? {
        ev = ev.with_counter(c);
    }
    if let Some(name) = row.get::<_, Option<String>>(8).map_err(sql_err)? {
        ev = ev.with_custom_name(name);
    }
    Ok(ev)
}

/// Clock snapshot values for the trace header.
///
/// A live snapshot maps REALTIME to the current BOOTTIME. A snapshot bounded by `until`
/// must be reproducible byte for byte, so it pins REALTIME = BOOTTIME = `until`; without
/// some BOOTTIME mapping trace processor reports `clock_sync_unrelatable_clock_domains`.
fn header_clocks(until: Option<u64>) -> (u64, Option<u64>) {
    match until {
        Some(u) => (u, Some(u)),
        None => (
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            get_boottime_ns(),
        ),
    }
}

fn encode_header(registry: &TrackRegistry, until: Option<u64>) -> Vec<u8> {
    let (now_ns, boottime_ns) = header_clocks(until);
    let mut header = Vec::with_capacity(8192);
    encode_snapshot_header(registry, now_ns, boottime_ns, &mut header);
    header
}

/// Writes a header-only trace (no events), e.g. for an empty or missing log.
fn write_empty_snapshot(until: Option<u64>, w: &mut impl Write) -> io::Result<SnapshotStats> {
    let header = encode_header(&TrackRegistry::new(), until);
    w.write_all(&header)?;
    w.flush()?;
    Ok(SnapshotStats {
        bytes: header.len() as u64,
        events: 0,
    })
}

fn register_hierarchy(
    reg: &mut TrackRegistry,
    rec_helper: &Recorder,
    track_key: &str,
    leaf_kind: TrackKind,
    leaf_unit: Option<&str>,
) {
    let chain = rec_helper.hierarchy_chain(track_key, leaf_kind, leaf_unit);
    for (k, parent_k, name, desc, kd, unit) in chain {
        let is_leaf = k == track_key;
        let final_kind = if is_leaf { leaf_kind } else { kd };
        let final_unit = if is_leaf {
            leaf_unit.or(unit.as_deref())
        } else {
            unit.as_deref()
        };
        if let Some(entry) = reg.get_by_key_mut(&k) {
            if is_leaf {
                entry.kind = final_kind;
                if entry.counter_unit.is_none() {
                    entry.counter_unit = final_unit.map(ToString::to_string);
                }
            }
        } else {
            reg.register(
                &k,
                parent_k.as_deref(),
                &name,
                desc.as_deref(),
                final_kind,
                final_unit,
            );
        }
    }
}

type EventTrackSummary = (Kind, String, Option<i64>, Option<String>);

fn finalize_snapshot_registry(reg: &mut TrackRegistry, summaries: &[EventTrackSummary]) {
    let rec_helper = Recorder::new();
    let mut active_keys = HashSet::new();

    for (kind, track_key, counter_val, custom_name) in summaries {
        match kind {
            Kind::TrackCreated | Kind::Prompt => continue,
            Kind::FileEdited => {
                if let Some(run_prefix) = run_prefix_of(track_key) {
                    let ft = format!("{}/files_edited", run_prefix);
                    register_hierarchy(reg, &rec_helper, &ft, TrackKind::Counter, Some("files"));
                    active_keys.insert(ft);
                }
            }
            Kind::Milestone if track_key.ends_with("/turns") => {
                let cname = custom_name.as_deref().unwrap_or("");
                if cname.starts_with("WorktreeCommit") {
                    if let Some(run_prefix) = run_prefix_of(track_key) {
                        let wt = format!("{}/worktree", run_prefix);
                        register_hierarchy(reg, &rec_helper, &wt, TrackKind::Slices, None);
                        active_keys.insert(wt);
                        let ct = format!("{}/commits", run_prefix);
                        register_hierarchy(
                            reg,
                            &rec_helper,
                            &ct,
                            TrackKind::Counter,
                            Some("commits"),
                        );
                        active_keys.insert(ct);
                    }
                } else if cname.starts_with("HeadMoved") {
                    if let Some(run_prefix) = run_prefix_of(track_key) {
                        let wt = format!("{}/worktree", run_prefix);
                        register_hierarchy(reg, &rec_helper, &wt, TrackKind::Slices, None);
                        active_keys.insert(wt);
                    }
                }
            }
            Kind::TaskQueued => {
                let st = task_state_track(track_key);
                register_hierarchy(reg, &rec_helper, &st, TrackKind::State, None);
                active_keys.insert(st);
            }
            Kind::Stall => {
                let st = if track_key.ends_with("/stalls") {
                    track_key.clone()
                } else {
                    format!("{}/stalls", track_key)
                };
                register_hierarchy(reg, &rec_helper, &st, TrackKind::Slices, None);
                active_keys.insert(st);
            }
            Kind::Message if track_key == "tacho" => {
                let mt = MESSAGES_TRACK.to_string();
                register_hierarchy(reg, &rec_helper, &mt, TrackKind::Slices, None);
                active_keys.insert(mt);
            }
            _ => {
                if counter_val.is_some() {
                    register_hierarchy(
                        reg,
                        &rec_helper,
                        track_key,
                        TrackKind::Counter,
                        custom_name.as_deref(),
                    );
                } else if matches!(kind, Kind::TaskState | Kind::AgentState) {
                    register_hierarchy(reg, &rec_helper, track_key, TrackKind::State, None);
                } else {
                    let existing_kind = reg
                        .get_by_key(track_key)
                        .map(|e| e.kind)
                        .filter(|&k| k != TrackKind::Group)
                        .unwrap_or(TrackKind::Slices);
                    register_hierarchy(reg, &rec_helper, track_key, existing_kind, None);
                }
                active_keys.insert(track_key.clone());
            }
        }
    }

    reg.retain_used(&active_keys);
    apply_display_names(reg);
}

/// Shortens default track names for the UI: under the `agents`, `tasks` and `providers`
/// groups the `agent:`/`task:`/`provider:` prefix is noise, and `run:2` reads better as
/// `run 2`. Only names still equal to the key's last segment are touched, so explicit
/// names from the log are kept.
fn apply_display_names(reg: &mut TrackRegistry) {
    let keys: Vec<String> = reg.iter().map(|e| e.key.clone()).collect();
    for key in keys {
        let Some(entry) = reg.get_by_key_mut(&key) else {
            continue;
        };
        let leaf = key.rsplit('/').next().unwrap_or(&key);
        if entry.name != leaf {
            continue;
        }
        let pretty = if let Some(rest) = ["agent:", "task:", "provider:"]
            .iter()
            .find_map(|p| leaf.strip_prefix(p))
        {
            rest.to_string()
        } else if let Some(n) = leaf.strip_prefix("run:") {
            format!("run {}", n)
        } else if let Some(n) = leaf.strip_prefix("lane:") {
            format!("lane {}", n)
        } else {
            continue;
        };
        if !pretty.is_empty() {
            entry.name = pretty;
        }
    }
}

/// Rebuilds the `TrackRegistry` by folding persisted `TrackCreated` rows up to `max_seq`.
pub fn build_registry_from_conn_up_to(
    conn: &Connection,
    max_seq: Option<u64>,
) -> io::Result<TrackRegistry> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE kind = ?1 AND (?2 IS NULL OR seq <= ?2)
             ORDER BY seq ASC"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt
        .query(rusqlite::params![Kind::TrackCreated as u32, max_seq])
        .map_err(sql_err)?;
    let mut events = Vec::new();
    while let Some(row) = rows.next().map_err(sql_err)? {
        events.push(event_from_row(row)?);
    }
    let mut reg = Recorder::restore(events.iter()).registry().clone();

    let mut summary_stmt = conn
        .prepare(
            "SELECT DISTINCT kind, track_key, counter_value, custom_name
             FROM events
             WHERE (?1 IS NULL OR seq <= ?1)",
        )
        .map_err(sql_err)?;
    let mut summary_rows = summary_stmt
        .query(rusqlite::params![max_seq])
        .map_err(sql_err)?;
    let mut summaries = Vec::new();
    while let Some(row) = summary_rows.next().map_err(sql_err)? {
        summaries.push((
            kind_of(row.get(0).map_err(sql_err)?),
            row.get(1).map_err(sql_err)?,
            row.get(2).map_err(sql_err)?,
            row.get(3).map_err(sql_err)?,
        ));
    }
    finalize_snapshot_registry(&mut reg, &summaries);
    Ok(reg)
}

/// Rebuilds the `TrackRegistry` by folding all persisted `TrackCreated` rows from the event log.
pub fn build_registry_from_conn(conn: &Connection) -> io::Result<TrackRegistry> {
    build_registry_from_conn_up_to(conn, None)
}

fn max_seq_until(conn: &Connection, until: Option<u64>) -> io::Result<Option<u64>> {
    conn.query_row(
        "SELECT MAX(seq) FROM events WHERE (?1 IS NULL OR ts_ns <= ?1)",
        rusqlite::params![until],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

/// Flow origins (`cause_seq * 256 + ordinal`) for every causal row up to `max_seq`, so a
/// windowed snapshot still links effects to causes that precede the window.
fn collect_flow_origins(conn: &Connection, max_seq: u64) -> io::Result<HashMap<u64, Vec<u64>>> {
    let mut origin_flows: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT kind, cause_seq, body
             FROM events
             WHERE (cause_seq IS NOT NULL OR kind = ?1) AND seq <= ?2
             ORDER BY seq ASC",
        )
        .map_err(sql_err)?;
    let mut rows = stmt
        .query(rusqlite::params![Kind::Run as u32, max_seq])
        .map_err(sql_err)?;
    while let Some(row) = rows.next().map_err(sql_err)? {
        let kind = kind_of(row.get(0).map_err(sql_err)?);
        let cause_seq: Option<u64> = row.get(1).map_err(sql_err)?;
        let parent_run_seq = if kind == Kind::Run {
            let body: &[u8] = row
                .get_ref(2)
                .map_err(sql_err)?
                .as_bytes()
                .map_err(|e| io::Error::other(e.to_string()))?;
            tacho::Event::decode(body).ok().and_then(|b| match b.body {
                Some(tacho::event::Body::Run(r)) => r.parent_run_seq,
                _ => None,
            })
        } else {
            None
        };
        Recorder::record_flow_origin(kind, cause_seq, parent_run_seq, &mut origin_flows);
    }
    Ok(origin_flows)
}

/// Writes a complete flight recorder snapshot to `w`, streaming rows directly from SQLite.
pub fn write_snapshot_conn(
    conn: &Connection,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    // One read transaction: every query below sees the same WAL snapshot.
    let tx = conn.unchecked_transaction().map_err(sql_err)?;
    let Some(max_seq) = max_seq_until(&tx, until)? else {
        let mut writer = io::BufWriter::with_capacity(8192, w);
        return write_empty_snapshot(until, &mut writer);
    };

    let registry = build_registry_from_conn_up_to(&tx, Some(max_seq))?;
    let origin_flows = collect_flow_origins(&tx, max_seq)?;

    let mut writer = io::BufWriter::with_capacity(65536, w);
    let header = encode_header(&registry, until);
    writer.write_all(&header)?;
    let mut sink = Sink::new(&registry, &mut writer, header.len() as u64);
    let mut projector = SnapshotProjector::default();

    let mut stmt = tx
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE (?1 IS NULL OR ts_ns >= ?1) AND (?2 IS NULL OR ts_ns <= ?2) AND seq <= ?3
             ORDER BY seq ASC"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt
        .query(rusqlite::params![since, until, max_seq])
        .map_err(sql_err)?;
    while let Some(row) = rows.next().map_err(sql_err)? {
        let mut ev = event_from_row(row)?;
        Recorder::populate_event_flows(&mut ev, &origin_flows);
        projector.process_event(ev, true, &mut sink)?;
    }
    projector.finish(&mut sink)?;
    sink.flush()?;
    Ok(SnapshotStats {
        bytes: sink.bytes,
        events: projector.events,
    })
}

fn open_read_only(db_path: &Path) -> io::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| io::Error::other(format!("failed to open sqlite db {:?}: {}", db_path, e)))?;
    // Best effort: a busy writer checkpointing should delay us briefly, not fail the snapshot.
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    let _ = conn.pragma_update(None, "query_only", true);
    Ok(conn)
}

/// Writes a complete flight recorder snapshot directly from `$TACHO_HOME/events.sqlite`.
///
/// Uses an independent read-only WAL connection; never blocks or depends on the appender.
pub fn write_snapshot(
    db_path: &Path,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    if !db_path.exists() {
        return write_empty_snapshot(until, w);
    }
    write_snapshot_conn(&open_read_only(db_path)?, since, until, w)
}

/// Writes `bytes` to `dest` atomically: a crash leaves either the old file or the new one,
/// never a truncated mix. The data is fsynced before the rename makes it visible.
pub fn write_file_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(dest, |f| f.write_all(bytes))
}

/// Writes a full snapshot of `db_path` to `dest` atomically (see [`write_file_atomic`]).
pub fn write_snapshot_file_atomic(db_path: &Path, dest: &Path) -> io::Result<SnapshotStats> {
    let mut stats = SnapshotStats::default();
    write_atomic_with(dest, |f| {
        stats = write_snapshot(db_path, None, None, f)?;
        Ok(())
    })?;
    Ok(stats)
}

fn write_atomic_with(
    dest: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let file_name = dest.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
    })?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        fill(&mut f)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dest)?;
        // Persist the rename itself.
        std::fs::File::open(dir).and_then(|d| d.sync_all())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Rebuilds a Perfetto trace from the durable SQLite event log by restoring a fresh
/// `Recorder` state machine from the persisted events and encoding through `Encoder`.
///
/// Unlike [`write_snapshot`] this loads every event into memory; it exists to verify that
/// the streaming path and a from-scratch replay agree.
pub fn rebuild_from_log(
    db_path: &Path,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    if !db_path.exists() {
        return write_snapshot(db_path, since, until, w);
    }
    let conn = open_read_only(db_path)?;
    let tx = conn.unchecked_transaction().map_err(sql_err)?;
    let Some(max_seq) = max_seq_until(&tx, until)? else {
        return write_empty_snapshot(until, w);
    };

    let mut stmt = tx
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE seq <= ?1 ORDER BY seq ASC"
        ))
        .map_err(sql_err)?;
    let mut rows = stmt.query(rusqlite::params![max_seq]).map_err(sql_err)?;
    let mut all_events = Vec::new();
    while let Some(row) = rows.next().map_err(sql_err)? {
        all_events.push(event_from_row(row)?);
    }

    Recorder::populate_flows(&mut all_events);
    let mut registry = Recorder::restore(all_events.iter()).registry().clone();
    let summaries: Vec<EventTrackSummary> = all_events
        .iter()
        .map(|ev| {
            (
                ev.kind,
                ev.track_key.clone(),
                ev.counter_value,
                ev.custom_name.clone(),
            )
        })
        .collect();
    finalize_snapshot_registry(&mut registry, &summaries);

    let mut writer = io::BufWriter::with_capacity(65536, w);
    let header = encode_header(&registry, until);
    writer.write_all(&header)?;
    let mut sink = Sink::new(&registry, &mut writer, header.len() as u64);
    let mut projector = SnapshotProjector::default();
    for ev in all_events {
        let in_window = since.is_none_or(|s| ev.ts_ns >= s) && until.is_none_or(|u| ev.ts_ns <= u);
        projector.process_event(ev, in_window, &mut sink)?;
    }
    projector.finish(&mut sink)?;
    sink.flush()?;
    Ok(SnapshotStats {
        bytes: sink.bytes,
        events: projector.events,
    })
}

/// Reads CLOCK_BOOTTIME in nanoseconds on Linux systems.
pub fn get_boottime_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let res = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if res == 0 {
        Some((ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64))
    } else {
        None
    }
}

/// Parses varint from slice, returning (value, bytes_consumed).
#[inline]
fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0;
    for (i, &b) in buf.iter().enumerate() {
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((val, i + 1));
        }
        shift += 7;
        if shift > 64 {
            return None;
        }
    }
    None
}

/// Inspects framed TracePacket bytes (Tag 0x0a + varint length + payload).
/// If the first field in payload is field 8 (varint timestamp), extracts it.
#[inline]
pub fn extract_framed_packet_timestamp(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() || buf[0] != 0x0a {
        return None;
    }

    let (packet_len, len_bytes) = decode_varint(&buf[1..])?;
    let packet_start = 1 + len_bytes;
    let total_len = packet_start + packet_len as usize;

    if total_len > buf.len() {
        return None;
    }

    let payload = &buf[packet_start..total_len];
    if !payload.is_empty() && payload[0] == 0x40 {
        // Tag 0x40 = field 8, wire type 0 (varint timestamp)
        let (ts, _) = decode_varint(&payload[1..])?;
        return Some((ts, total_len));
    }

    // Packet has no leading timestamp field
    Some((0, total_len))
}

#[cfg(test)]
mod tests;
