//! JSON and human-text rendering of `StatusSnapshot` with `Scrubber` redaction.

use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use tacho_core::Scrubber;
use tacho_daemon::{render_status, EventLog, Recorder};

use crate::cli::StatusArgs;
use crate::client::DaemonClient;
use crate::fleet::store::now_secs;

use super::collect::{load_status_snapshot, validate_sections};
use super::model::{ActionableItem, StatusSnapshot, ATTENTION_TEXT_CAP};
use super::report::resolve_events_db;

fn scrub_json_value(val: &mut serde_json::Value, scrubber: &Scrubber) {
    match val {
        serde_json::Value::String(s) => {
            *s = scrubber.scrub(s);
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                scrub_json_value(item, scrubber);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                scrub_json_value(v, scrubber);
            }
        }
        _ => {}
    }
}

/// Renders the requested sections (or the full snapshot when `sections` is empty) as JSON.
pub fn render_snapshot_json(snap: &StatusSnapshot, sections: &[String]) -> String {
    let scrubber = Scrubber::new();
    let full = serde_json::to_value(snap).unwrap_or_else(|_| serde_json::json!({}));
    let mut out_val = if sections.is_empty() {
        full
    } else {
        let mut map = serde_json::Map::new();
        map.insert("version".to_string(), serde_json::json!(snap.version));
        map.insert("at".to_string(), serde_json::json!(snap.at));
        if let Some(obj) = full.as_object() {
            for sec in sections {
                if sec == "needs" {
                    if let Some(v) = obj.get("needs") {
                        map.insert("needs".to_string(), v.clone());
                    }
                    if let Some(v) = obj.get("needs_you") {
                        map.insert("needs_you".to_string(), v.clone());
                    }
                    if let Some(v) = obj.get("attention") {
                        map.insert("attention".to_string(), v.clone());
                    }
                } else if let Some(v) = obj.get(sec) {
                    map.insert(sec.clone(), v.clone());
                }
            }
        }
        serde_json::Value::Object(map)
    };
    scrub_json_value(&mut out_val, &scrubber);
    serde_json::to_string_pretty(&out_val).unwrap_or_else(|_| "{}".to_string())
}

fn render_actionable_block(title: &str, items: &[ActionableItem], out: &mut Vec<String>) {
    if items.is_empty() {
        out.push(format!("{title}: none"));
        return;
    }
    out.push(format!("{title} ({}):", items.len()));
    for item in items.iter().take(ATTENTION_TEXT_CAP) {
        let label = if item.target.is_empty() {
            format!("[{}] {}", item.kind, item.summary)
        } else {
            format!("[{}] {}: {}", item.kind, item.target, item.summary)
        };
        out.push(format!("  * {label}"));
        let fix = if !item.fix.is_empty() {
            &item.fix
        } else {
            &item.command_hint
        };
        if !fix.is_empty() {
            out.push(format!("    -> {fix}"));
        }
    }
    if items.len() > ATTENTION_TEXT_CAP {
        out.push(format!(
            "  ... and {} more",
            items.len() - ATTENTION_TEXT_CAP
        ));
    }
}

/// Renders the requested sections (or the full snapshot when `sections` is empty) as human-readable text.
pub fn render_snapshot_text(snap: &StatusSnapshot, sections: &[String]) -> String {
    let scrubber = Scrubber::new();
    let all = sections.is_empty();
    let has = |name: &str| all || sections.iter().any(|s| s == name);
    let mut lines = Vec::new();

    if has("headline") {
        let h = &snap.headline;
        lines.push(format!(
            "tacho: {} | {}/{} agents, {} todo | {} PRs, {} batch, {} train, {} ready | today ${:.2}",
            h.health,
            h.agents,
            h.max_agents,
            h.todo,
            h.open_prs,
            h.batches,
            h.trains,
            h.ready,
            h.cost_today
        ));
    }

    if has("needs") || has("needs_you") {
        if !all || !snap.needs_you.is_empty() {
            render_actionable_block("needs you", &snap.needs_you, &mut lines);
        }
        if sections.iter().any(|s| s == "needs") && !has("attention") && !snap.attention.is_empty()
        {
            render_actionable_block("attention", &snap.attention, &mut lines);
        }
    }

    if has("attention") && (!all || !snap.attention.is_empty()) {
        render_actionable_block("attention", &snap.attention, &mut lines);
    }

    if has("pipeline") {
        let p = &snap.pipeline;
        lines.push(format!(
            "pipeline: {} open PR(s), {} batch(es), {} train(s), {} ready, {} queued",
            p.open_prs,
            p.batches.len(),
            p.trains.len(),
            p.ready.len(),
            p.queue.len()
        ));
        for q in &p.queue {
            let state_str = if q.ready { "ready" } else { "waiting" };
            lines.push(format!(
                "  todo {:<20} area={:<10} {} ({}) -- {}",
                q.key,
                if q.area.is_empty() { "-" } else { &q.area },
                state_str,
                q.reason,
                q.brief
            ));
        }
    }

    if has("agents") {
        if snap.agents.is_empty() {
            lines.push("agents: none".to_string());
        } else {
            lines.push(format!("agents ({}):", snap.agents.len()));
            for a in &snap.agents {
                let pr_str =
                    a.pr.map(|n| format!("#{n}"))
                        .unwrap_or_else(|| "-".to_string());
                lines.push(format!(
                    "  {:<20} {:<8} {:<8} phase={:<6} area={:<10} pr={:<5} {}",
                    a.key,
                    a.status,
                    a.verdict,
                    a.phase,
                    if a.area.is_empty() { "-" } else { &a.area },
                    pr_str,
                    a.note
                ));
            }
        }
    }

    if has("progress") && !all {
        if snap.progress.agents.is_empty() {
            lines.push("progress: no active or recent agents".to_string());
        } else {
            lines.push("progress:".to_string());
            for r in &snap.progress.agents {
                let phase_str = if r.phase_long {
                    format!("{}!", r.phase)
                } else {
                    r.phase.clone()
                };
                lines.push(format!(
                    "  {:<20} {:<10} {:<8} {:<8} {:>6}s  {}",
                    r.key,
                    if r.area.is_empty() { "-" } else { &r.area },
                    r.verdict,
                    phase_str,
                    r.idle_secs,
                    r.note
                ));
            }
        }
        for c in &snap.progress.chains {
            lines.push(format!(
                "  chain {:<12} 1h={} 24h={} med_run={}s med_batch={}s owner_w={}s mach_w={}s",
                c.area,
                c.steps_last_hour,
                c.steps_last_day,
                c.median_run_secs,
                c.median_batch_secs,
                c.owner_wait_secs,
                c.machine_wait_secs
            ));
        }
    }

    if has("machine") {
        let m = &snap.machine;
        lines.push(format!(
            "machine: {:.2} / {:.1} cores in use ({:.1}% of quota, limit {:.0}%) | load {:.2}/{:.2} on {} cores | mem {:.1}/{:.1} GB | disk {:.1} GB free",
            m.cores_in_use,
            m.quota_cores,
            m.cpu_percent,
            m.max_cpu_percent,
            m.load1,
            m.load5,
            m.usable_cpus,
            m.mem_used_gb,
            m.mem_total_gb,
            m.disk_free_gb
        ));
        for owner in &m.by_owner {
            lines.push(format!(
                "  cpu {:<20} [{:<5}] {:.2} cores",
                owner.owner, owner.kind, owner.cores
            ));
        }
        for z in &m.zombies {
            lines.push(format!("  zombie: {z}"));
        }
    }

    if has("cost") {
        let c = &snap.cost;
        lines.push(format!(
            "cost: ${:.2} hour | ${:.2} today | ${:.2} 7d (total ${:.2} across {} runs)",
            c.hour, c.today, c.week, c.total_usd, c.runs
        ));
        if !c.by_model.is_empty() {
            let models: Vec<String> = c
                .by_model
                .iter()
                .map(|(k, v)| format!("{k}=${v:.2}"))
                .collect();
            lines.push(format!("  by_model: {}", models.join(", ")));
        }
        if !c.by_place.is_empty() {
            let places: Vec<String> = c
                .by_place
                .iter()
                .map(|(k, v)| format!("{k}=${v:.2}"))
                .collect();
            lines.push(format!("  by_place: {}", places.join(", ")));
        }
        for top in &c.top_today {
            lines.push(format!(
                "  top_today: {} ({}) ${:.2}",
                top.key, top.model, top.usd
            ));
        }
        if c.per_pr.samples > 0 {
            lines.push(format!(
                "  per_pr: avg ${:.2}, max ${:.2} ({} PRs)",
                c.per_pr.avg_usd, c.per_pr.max_usd, c.per_pr.samples
            ));
        }
    }

    let explicit_held_or_stale = sections.iter().any(|s| s == "held" || s == "stale");
    if explicit_held_or_stale {
        let target_sec =
            if sections.iter().any(|s| s == "stale") && !sections.iter().any(|s| s == "held") {
                &snap.stale
            } else {
                &snap.held
            };
        if target_sec.holds.is_empty() && target_sec.refused.is_empty() {
            lines.push("held: none".to_string());
        } else {
            lines.push(format!("held ({}):", target_sec.holds.len()));
            for h in &target_sec.holds {
                let stale_tag = if h.stale { " [STALE >24h]" } else { "" };
                lines.push(format!(
                    "  * {:<20} {:>6}s  {}{}",
                    h.target, h.age_secs, h.why, stale_tag
                ));
                lines.push(format!("    -> {}", h.fix));
            }
            for r in &target_sec.refused {
                lines.push(format!("  * refused: {r}"));
            }
        }
    } else if all && (!snap.held.holds.is_empty() || !snap.held.refused.is_empty()) {
        lines.push(format!(
            "held: {} hold(s) ({} stale >24h) -- run `tacho status --section held` to expand",
            snap.held.holds.len(),
            snap.held.stale_holds.len()
        ));
        for h in &snap.held.stale_holds {
            lines.push(format!(
                "  * [stale-hold] {}: {} ({}h) -> {}",
                h.target,
                h.why,
                h.age_secs / 3600,
                h.fix
            ));
        }
    }

    if has("flakes") && (!all || !snap.flakes.is_empty()) {
        if snap.flakes.is_empty() {
            lines.push("flakes: none".to_string());
        } else {
            lines.push(format!("flakes ({}):", snap.flakes.len()));
            for f in &snap.flakes {
                lines.push(format!(
                    "  * {} (area={}, count={}, last_seen={}, task={})",
                    f.test,
                    if f.area.is_empty() { "-" } else { &f.area },
                    f.count,
                    f.last_seen,
                    f.task_key.as_deref().unwrap_or("-")
                ));
            }
        }
    }

    scrubber.scrub(&lines.join("\n"))
}

/// Executes `tacho status [--json] [--section <S>]...`.
pub fn execute_status(args: StatusArgs) -> io::Result<()> {
    let sections = validate_sections(&args.section)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let home = crate::paths::resolve_home();
    let now = now_secs();
    let snap = load_status_snapshot(&home, now).map_err(io::Error::other)?;

    if args.json {
        let has_fleet_data = !sections.is_empty()
            || home.join("snapshot.json").is_file()
            || !snap.agents.is_empty()
            || !snap.needs.is_empty()
            || !snap.held.holds.is_empty()
            || !snap.flakes.is_empty()
            || !snap.pipeline.queue.is_empty()
            || !snap.pipeline.batches.is_empty()
            || !snap.pipeline.trains.is_empty()
            || !snap.pipeline.ready.is_empty()
            || snap.headline.open_prs > 0;

        if !has_fleet_data {
            let client = DaemonClient::new();
            let recorder_json = match client.send_request(&serde_json::json!({
                "cmd": "status",
                "json": true,
            })) {
                Ok(resp) => resp
                    .get("output")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                Err(_) => {
                    let db_path = resolve_events_db();
                    if db_path.exists() {
                        let events = EventLog::open(&db_path)
                            .and_then(|log| log.load_all())
                            .unwrap_or_default();
                        if !events.is_empty() {
                            let recorder = Recorder::restore(events.iter());
                            let now_ns = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64;
                            Some(render_status(recorder.live_state(), true, now_ns))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            };
            if let Some(rec_json) = recorder_json {
                let has_rec_agents = serde_json::from_str::<serde_json::Value>(&rec_json)
                    .ok()
                    .and_then(|v| v.as_array().map(|a| !a.is_empty()))
                    .unwrap_or(false);
                if has_rec_agents {
                    println!("{}", Scrubber::new().scrub(&rec_json));
                    return Ok(());
                }
            }
        }

        let out = render_snapshot_json(&snap, &sections);
        println!("{out}");
        return Ok(());
    }

    let mut text = render_snapshot_text(&snap, &sections);

    if sections.is_empty() {
        let client = DaemonClient::new();
        let recorder_text = match client.send_request(&serde_json::json!({
            "cmd": "status",
            "json": false,
        })) {
            Ok(resp) => resp
                .get("output")
                .and_then(|v| v.as_str())
                .map(String::from),
            Err(_) => {
                let db_path = resolve_events_db();
                if db_path.exists() {
                    let events = EventLog::open(&db_path)
                        .and_then(|log| log.load_all())
                        .unwrap_or_default();
                    if !events.is_empty() {
                        let recorder = Recorder::restore(events.iter());
                        let now_ns = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        let db_size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
                        Some(format!(
                            "{}\nevents.sqlite: {} bytes",
                            render_status(recorder.live_state(), false, now_ns),
                            db_size
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
        if let Some(rec) = recorder_text {
            let scrubbed_rec = Scrubber::new().scrub(&rec);
            if !scrubbed_rec.trim().is_empty() {
                text.push('\n');
                text.push_str(scrubbed_rec.trim_end());
            }
        }
    }

    println!("{}", Scrubber::new().scrub(&text));
    Ok(())
}
