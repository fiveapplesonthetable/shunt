//! Snapshot generation from SQLite WAL event log.
//!
//! Produces Perfetto-compatible `.perfetto-trace` byte streams adhering to design section 9.
//! The SQLite event log is the single source of truth; snapshots stream directly from
//! SQLite rows to Perfetto TracePackets without buffering the dataset in memory.

use prost::Message;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;
use tacho_core::{encode_snapshot_header, Encoder, Event, Kind, Recorder, TrackRegistry};
use tacho_proto::tacho;

/// Statistics about an emitted snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Total bytes written to the output stream.
    pub bytes: u64,
    /// Total number of events encoded and emitted.
    pub events: u64,
}

/// Rebuilds the `TrackRegistry` by folding persisted `TrackCreated` rows up to `max_seq`.
pub fn build_registry_from_conn_up_to(
    conn: &Connection,
    max_seq: Option<u64>,
) -> io::Result<TrackRegistry> {
    let mut reg_stmt = conn
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body
             FROM events
             WHERE kind = ?1 AND (?2 IS NULL OR seq <= ?2)
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = reg_stmt
        .query(rusqlite::params![Kind::TrackCreated as u32, max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut events = Vec::new();
    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;
        let body_bytes: Vec<u8> = row.get(6).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);

        let body = tacho::Event::decode(&body_bytes[..]).unwrap_or(tacho::Event { body: None });
        let ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);
        events.push(ev);
    }

    let rec = Recorder::restore(events.iter());
    let mut reg = rec.registry().clone();

    // Ensure any track with counter samples is registered as TrackKind::Counter
    let mut counter_stmt = conn
        .prepare(
            "SELECT DISTINCT track_key, custom_name
             FROM events
             WHERE counter_value IS NOT NULL AND (?1 IS NULL OR seq <= ?1)",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut counter_rows = counter_stmt
        .query(rusqlite::params![max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    while let Some(row) = counter_rows
        .next()
        .map_err(|e| io::Error::other(e.to_string()))?
    {
        let track_key: String = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let unit: Option<String> = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(entry) = reg.get_by_key_mut(&track_key) {
            entry.kind = tacho_core::TrackKind::Counter;
            if entry.counter_unit.is_none() {
                entry.counter_unit = unit;
            }
        } else {
            reg.register(
                &track_key,
                None,
                &track_key,
                None,
                tacho_core::TrackKind::Counter,
                unit.as_deref(),
            );
        }
    }

    // Ensure any track with state events (TaskState, AgentState) is registered as TrackKind::State
    let mut state_stmt = conn
        .prepare(
            "SELECT DISTINCT track_key
             FROM events
             WHERE kind IN (?1, ?2) AND (?3 IS NULL OR seq <= ?3)",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut state_rows = state_stmt
        .query(rusqlite::params![
            Kind::TaskState as u32,
            Kind::AgentState as u32,
            max_seq
        ])
        .map_err(|e| io::Error::other(e.to_string()))?;

    while let Some(row) = state_rows
        .next()
        .map_err(|e| io::Error::other(e.to_string()))?
    {
        let track_key: String = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(entry) = reg.get_by_key_mut(&track_key) {
            entry.kind = tacho_core::TrackKind::State;
        } else {
            reg.register(
                &track_key,
                None,
                &track_key,
                None,
                tacho_core::TrackKind::State,
                None,
            );
        }
    }

    // Ensure ALL tracks appearing in events up to max_seq are registered in registry
    let mut all_tracks_stmt = conn
        .prepare(
            "SELECT DISTINCT track_key
             FROM events
             WHERE (?1 IS NULL OR seq <= ?1)",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut all_tracks_rows = all_tracks_stmt
        .query(rusqlite::params![max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    while let Some(row) = all_tracks_rows
        .next()
        .map_err(|e| io::Error::other(e.to_string()))?
    {
        let track_key: String = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        if reg.get_by_key(&track_key).is_none() {
            reg.register(
                &track_key,
                None,
                &track_key,
                None,
                tacho_core::TrackKind::Slices,
                None,
            );
        }
    }

    Ok(reg)
}

/// Rebuilds the `TrackRegistry` by folding all persisted `TrackCreated` rows from the event log.
pub fn build_registry_from_conn(conn: &Connection) -> io::Result<TrackRegistry> {
    build_registry_from_conn_up_to(conn, None)
}

/// Writes a complete flight recorder snapshot to `w`, streaming rows directly from SQLite.
pub fn write_snapshot_conn(
    conn: &Connection,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let max_seq: Option<u64> = tx
        .query_row(
            "SELECT MAX(seq) FROM events WHERE (?1 IS NULL OR ts_ns <= ?1)",
            rusqlite::params![until],
            |r| r.get(0),
        )
        .unwrap_or(None);

    let max_seq = match max_seq {
        Some(s) => s,
        None => {
            let registry = TrackRegistry::new();
            let (now_ns, boottime_ns) = match until {
                Some(u) => (u, None),
                None => (
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64,
                    get_boottime_ns(),
                ),
            };
            let mut writer = io::BufWriter::with_capacity(8192, w);
            let mut header_bytes = Vec::with_capacity(8192);
            encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
            writer.write_all(&header_bytes)?;
            writer.flush()?;
            return Ok(SnapshotStats {
                bytes: header_bytes.len() as u64,
                events: 0,
            });
        }
    };

    let registry = build_registry_from_conn_up_to(&tx, Some(max_seq))?;

    let (now_ns, boottime_ns) = match until {
        Some(u) => (u, None),
        None => (
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            get_boottime_ns(),
        ),
    };

    let mut writer = io::BufWriter::with_capacity(65536, w);

    // 1-4. Snapshot Header
    let mut header_bytes = Vec::with_capacity(8192);
    encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
    writer.write_all(&header_bytes)?;
    let mut total_bytes = header_bytes.len() as u64;

    // Pre-collect flow origins (cause_seq * 256 + ordinal) up to max_seq
    let mut origin_flows: HashMap<u64, Vec<u64>> = HashMap::new();
    {
        let mut flow_stmt = tx
            .prepare(
                "SELECT kind, cause_seq, body
                 FROM events
                 WHERE (cause_seq IS NOT NULL OR kind = ?1) AND seq <= ?2
                 ORDER BY seq ASC",
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut flow_rows = flow_stmt
            .query(rusqlite::params![Kind::Run as u32, max_seq])
            .map_err(|e| io::Error::other(e.to_string()))?;
        while let Some(row) = flow_rows
            .next()
            .map_err(|e| io::Error::other(e.to_string()))?
        {
            let kind_int: u32 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
            let cause_seq: Option<u64> = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
            let kind = Kind::ALL
                .get(kind_int as usize)
                .copied()
                .unwrap_or(Kind::Daemon);
            let parent_run_seq = if kind == Kind::Run {
                let slice: &[u8] = row
                    .get_ref(2)
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .as_bytes()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                tacho::Event::decode(slice).ok().and_then(|b| match b.body {
                    Some(tacho::event::Body::Run(r)) => r.parent_run_seq,
                    _ => None,
                })
            } else {
                None
            };
            Recorder::record_flow_origin(kind, cause_seq, parent_run_seq, &mut origin_flows);
        }
    }

    // 5. Query events ordered by seq ASC (streaming, O(1) memory)
    let mut stmt = tx
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name
             FROM events
             WHERE (?1 IS NULL OR ts_ns >= ?1) AND (?2 IS NULL OR ts_ns <= ?2) AND seq <= ?3
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = stmt
        .query(rusqlite::params![since, until, max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::with_capacity(2048);
    let mut events_count = 0u64;

    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;

        let slice: &[u8] = row
            .get_ref(6)
            .map_err(|e| io::Error::other(e.to_string()))?
            .as_bytes()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let counter_val: Option<i64> = row.get(7).map_err(|e| io::Error::other(e.to_string()))?;
        let custom_name: Option<String> =
            row.get(8).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);

        let body = tacho::Event::decode(slice).unwrap_or(tacho::Event { body: None });

        let mut ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);

        if let Some(c) = counter_val {
            ev = ev.with_counter(c);
        }
        if let Some(cn) = custom_name {
            ev = ev.with_custom_name(cn);
        }
        Recorder::populate_event_flows(&mut ev, &origin_flows);

        packet_buf.clear();
        encoder.encode_event(&ev, &registry, &mut packet_buf);
        writer.write_all(&packet_buf)?;
        total_bytes += packet_buf.len() as u64;
        events_count += 1;
    }

    writer.flush()?;

    Ok(SnapshotStats {
        bytes: total_bytes,
        events: events_count,
    })
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
        let registry = TrackRegistry::new();
        let (now_ns, boottime_ns) = match until {
            Some(u) => (u, None),
            None => (
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
                get_boottime_ns(),
            ),
        };
        let mut header_bytes = Vec::with_capacity(8192);
        encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
        w.write_all(&header_bytes)?;
        w.flush()?;
        return Ok(SnapshotStats {
            bytes: header_bytes.len() as u64,
            events: 0,
        });
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| io::Error::other(format!("failed to open sqlite db {:?}: {}", db_path, e)))?;

    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    let _ = conn.pragma_update(None, "query_only", true);

    write_snapshot_conn(&conn, since, until, w)
}

/// Rebuilds a Perfetto trace from the durable SQLite event log by restoring a fresh
/// `Recorder` state machine from the persisted events and encoding through `Encoder`.
pub fn rebuild_from_log(
    db_path: &Path,
    since: Option<u64>,
    until: Option<u64>,
    w: &mut impl Write,
) -> io::Result<SnapshotStats> {
    if !db_path.exists() {
        return write_snapshot(db_path, since, until, w);
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| io::Error::other(format!("failed to open sqlite db {:?}: {}", db_path, e)))?;
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    let _ = conn.pragma_update(None, "query_only", true);

    let max_seq: Option<u64> = conn
        .query_row(
            "SELECT MAX(seq) FROM events WHERE (?1 IS NULL OR ts_ns <= ?1)",
            rusqlite::params![until],
            |r| r.get(0),
        )
        .unwrap_or(None);

    let Some(max_seq) = max_seq else {
        return write_snapshot_conn(&conn, since, until, w);
    };

    let mut stmt = conn
        .prepare(
            "SELECT seq, ts_ns, kind, track_key, cause_seq, agent, body, counter_value, custom_name
             FROM events
             WHERE seq <= ?1
             ORDER BY seq ASC",
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut rows = stmt
        .query(rusqlite::params![max_seq])
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut all_events = Vec::new();
    while let Some(row) = rows.next().map_err(|e| io::Error::other(e.to_string()))? {
        let seq: u64 = row.get(0).map_err(|e| io::Error::other(e.to_string()))?;
        let ts_ns: u64 = row.get(1).map_err(|e| io::Error::other(e.to_string()))?;
        let kind_int: u32 = row.get(2).map_err(|e| io::Error::other(e.to_string()))?;
        let track_key: String = row.get(3).map_err(|e| io::Error::other(e.to_string()))?;
        let cause_seq: Option<u64> = row.get(4).map_err(|e| io::Error::other(e.to_string()))?;
        let agent: Option<String> = row.get(5).map_err(|e| io::Error::other(e.to_string()))?;
        let body_bytes: Vec<u8> = row.get(6).map_err(|e| io::Error::other(e.to_string()))?;
        let counter_val: Option<i64> = row.get(7).map_err(|e| io::Error::other(e.to_string()))?;
        let custom_name: Option<String> =
            row.get(8).map_err(|e| io::Error::other(e.to_string()))?;

        let kind = Kind::ALL
            .get(kind_int as usize)
            .copied()
            .unwrap_or(Kind::Daemon);
        let body = tacho::Event::decode(&body_bytes[..]).unwrap_or(tacho::Event { body: None });
        let mut ev = Event::new(seq, ts_ns, kind, track_key, cause_seq, agent, body);
        if let Some(c) = counter_val {
            ev = ev.with_counter(c);
        }
        if let Some(cn) = custom_name {
            ev = ev.with_custom_name(cn);
        }
        all_events.push(ev);
    }

    Recorder::populate_flows(&mut all_events);
    let rec = Recorder::restore(all_events.iter());
    let mut registry = rec.registry().clone();
    for ev in &all_events {
        if let Some(c_unit) = ev.custom_name.as_deref() {
            if ev.counter_value.is_some() {
                if let Some(entry) = registry.get_by_key_mut(&ev.track_key) {
                    entry.kind = tacho_core::TrackKind::Counter;
                    if entry.counter_unit.is_none() {
                        entry.counter_unit = Some(c_unit.to_string());
                    }
                }
            }
        }
        if matches!(ev.kind, Kind::TaskState | Kind::AgentState) {
            if let Some(entry) = registry.get_by_key_mut(&ev.track_key) {
                entry.kind = tacho_core::TrackKind::State;
            } else {
                registry.register(
                    &ev.track_key,
                    None,
                    &ev.track_key,
                    None,
                    tacho_core::TrackKind::State,
                    None,
                );
            }
        }
        if registry.get_by_key(&ev.track_key).is_none() {
            registry.register(
                &ev.track_key,
                None,
                &ev.track_key,
                None,
                tacho_core::TrackKind::Slices,
                None,
            );
        }
    }

    let (now_ns, boottime_ns) = match until {
        Some(u) => (u, None),
        None => (
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            get_boottime_ns(),
        ),
    };

    let mut writer = io::BufWriter::with_capacity(65536, w);
    let mut header_bytes = Vec::with_capacity(8192);
    encode_snapshot_header(&registry, now_ns, boottime_ns, &mut header_bytes);
    writer.write_all(&header_bytes)?;
    let mut total_bytes = header_bytes.len() as u64;

    let mut encoder = Encoder::new();
    let mut packet_buf = Vec::with_capacity(2048);
    let mut events_count = 0u64;

    for ev in &all_events {
        if since.is_some_and(|s| ev.ts_ns < s) {
            continue;
        }
        if until.is_some_and(|u| ev.ts_ns > u) {
            continue;
        }
        packet_buf.clear();
        encoder.encode_event(ev, &registry, &mut packet_buf);
        writer.write_all(&packet_buf)?;
        total_bytes += packet_buf.len() as u64;
        events_count += 1;
    }

    writer.flush()?;
    Ok(SnapshotStats {
        bytes: total_bytes,
        events: events_count,
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
mod tests {
    use super::*;
    use crate::log::EventLog;
    use tacho_core::{Event, Kind};
    use tacho_proto::tacho;

    #[test]
    fn test_extract_packet_timestamp() {
        let registry = TrackRegistry::new();
        let mut encoder = Encoder::new();
        let mut out = Vec::new();

        let ev = Event::new(
            1,
            123456789,
            Kind::Run,
            "agent:1/run:1",
            None,
            None,
            tacho::Event { body: None },
        );
        encoder.encode_event(&ev, &registry, &mut out);

        let (ts, total_len) = extract_framed_packet_timestamp(&out).unwrap();
        assert_eq!(ts, 123456789);
        assert_eq!(total_len, out.len());
    }

    #[test]
    fn test_write_snapshot_streaming() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("events.sqlite");
        let mut log = EventLog::open(&db_path).unwrap();

        let ev = Event::new(
            1,
            1_000_000,
            Kind::Daemon,
            "daemon",
            None,
            None,
            tacho::Event { body: None },
        );
        log.insert_batch(&[ev]).unwrap();

        let mut output = Vec::new();
        let stats = write_snapshot(&db_path, None, None, &mut output).unwrap();

        assert_eq!(stats.events, 1);
        assert!(stats.bytes > 0);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_write_snapshot_and_rebuild_from_log_byte_identical() {
        use tacho_core::{Input, MessageDirection, SessionKey, TaskStatus, ToolInput};

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("events.sqlite");
        let mut log = EventLog::open(&db_path).unwrap();

        let mut rec = Recorder::new();
        let mut events = Vec::new();
        let t0 = 1_000_000_000u64;
        let sess = SessionKey::new("claude", "sess-rebuild");

        rec.apply(
            Input::Task {
                source_ts_ns: Some(t0),
                slug: "rebuild-task".into(),
                brief_digest: None,
                brief_bytes: Some(128),
                paths: vec!["src/main.rs".into()],
                agent: Some("worker-1".into()),
                base_commit: None,
                token_budget: None,
                cost_budget_micros: None,
            },
            t0,
            &mut events,
        );
        rec.apply(
            Input::RunStart {
                source_ts_ns: Some(t0 + 100),
                provider: "claude".into(),
                session_id: "sess-rebuild".into(),
                agent: Some("worker-1".into()),
                task: Some("rebuild-task".into()),
                pid: Some(1234),
                cwd: Some("/workspace".into()),
                model: Some("claude-sonnet-4-6".into()),
                parent_session: None,
                git_tree: None,
                role: None,
            },
            t0 + 100,
            &mut events,
        );
        rec.apply(
            Input::Prompt {
                source_ts_ns: Some(t0 + 200),
                session: sess.clone(),
                text_len: 50,
                digest: None,
            },
            t0 + 200,
            &mut events,
        );
        rec.apply(
            Input::ToolBegin {
                source_ts_ns: Some(t0 + 300),
                session: sess.clone(),
                tool_use_id: "t1".into(),
                tool_name: "Bash".into(),
                input: ToolInput::Shell {
                    argv: "cargo test".into(),
                    cwd: None,
                    git_tree: None,
                },
            },
            t0 + 300,
            &mut events,
        );
        rec.apply(
            Input::ToolEnd {
                source_ts_ns: Some(t0 + 400),
                session: sess.clone(),
                tool_use_id: "t1".into(),
                error: false,
                result_bytes: Some(42),
                exit_code: Some(0),
            },
            t0 + 400,
            &mut events,
        );
        rec.apply(
            Input::Message {
                source_ts_ns: Some(t0 + 500),
                from: "orch".into(),
                to: "worker-1".into(),
                id: Some("m-1".into()),
                bytes: Some(16),
                dir: Some(MessageDirection::Sent),
            },
            t0 + 500,
            &mut events,
        );
        rec.apply(
            Input::Message {
                source_ts_ns: Some(t0 + 600),
                from: "orch".into(),
                to: "worker-1".into(),
                id: Some("m-1".into()),
                bytes: Some(16),
                dir: Some(MessageDirection::Delivered),
            },
            t0 + 600,
            &mut events,
        );
        rec.apply(
            Input::TaskState {
                source_ts_ns: Some(t0 + 700),
                slug: "rebuild-task".into(),
                state: TaskStatus::Done,
                because: Some("all green".into()),
            },
            t0 + 700,
            &mut events,
        );

        log.insert_batch(&events).unwrap();

        let until = Some(t0 + 1_000);
        let mut snap_bytes = Vec::new();
        let mut rebuild_bytes = Vec::new();
        let s1 = write_snapshot(&db_path, None, until, &mut snap_bytes).unwrap();
        let s2 = rebuild_from_log(&db_path, None, until, &mut rebuild_bytes).unwrap();

        assert_eq!(s1, s2);
        assert_eq!(
            snap_bytes, rebuild_bytes,
            "write_snapshot and rebuild_from_log must be byte-for-byte identical at the same until"
        );
    }
}
