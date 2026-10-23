//! Benchmark restoring recorder state from 1,000,000 recorded events.

use std::time::Instant;
use tacho_core::{Event, Kind, Recorder};
use tacho_proto::tacho;

fn main() {
    let num_events = 1_000_000;
    println!("Generating {} simulated events...", num_events);
    let mut events = Vec::with_capacity(num_events);

    events.push(Event {
        seq: 1,
        ts_ns: 1_000_000,
        kind: Kind::Run,
        track_key: "agent:worker-1/run:1".to_string(),
        cause_seq: None,
        agent: Some("worker-1".to_string()),
        custom_name: None,
        flow_ids: Vec::new(),
        terminating_flow_ids: Vec::new(),
        counter_value: None,
        body: tacho::Event {
            body: Some(tacho::event::Body::Run(tacho::Run {
                provider: Some("claude".to_string()),
                session_id: Some("sess-1".to_string()),
                task: Some("bench".to_string()),
                pid: Some(1234),
                cwd: Some("/workspace".to_string()),
                model: Some("claude-3-5".to_string()),
                parent_run_seq: None,
                resume: None,
                git_tree: None,
            })),
        },
    });

    events.push(Event {
        seq: 2,
        ts_ns: 2_000_000,
        kind: Kind::Turn,
        track_key: "agent:worker-1/run:1/turns".to_string(),
        cause_seq: None,
        agent: Some("worker-1".to_string()),
        custom_name: None,
        flow_ids: Vec::new(),
        terminating_flow_ids: Vec::new(),
        counter_value: None,
        body: tacho::Event {
            body: Some(tacho::event::Body::Turn(tacho::Turn {
                turn_number: Some(1),
                model: Some("claude-3-5".to_string()),
            })),
        },
    });

    for i in 3..=num_events as u64 {
        let ts_ns = 2_000_000 + i * 1_000;
        let (kind, track_key, cause_seq, counter_val) = match i % 5 {
            0 => (
                Kind::Command,
                "agent:worker-1/run:1/lane:1".to_string(),
                None,
                None,
            ),
            1 => (
                Kind::CommandEnded,
                "agent:worker-1/run:1/lane:1".to_string(),
                Some(i - 1),
                None,
            ),
            2 => (
                Kind::AgentState,
                "agent:worker-1/state".to_string(),
                None,
                None,
            ),
            3 => (
                Kind::Custom,
                "machine/cpu_busy".to_string(),
                None,
                Some((i % 100) as i64),
            ),
            _ => (
                Kind::FileEdited,
                "agent:worker-1/run:1/lane:1".to_string(),
                Some(i - 1),
                None,
            ),
        };
        events.push(Event {
            seq: i,
            ts_ns,
            kind,
            track_key,
            cause_seq,
            agent: Some("worker-1".to_string()),
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: counter_val,
            body: tacho::Event { body: None },
        });
    }

    println!("Starting restoration benchmark...");
    let start = Instant::now();
    let rec = Recorder::restore(events.iter());
    let elapsed = start.elapsed();

    assert!(rec.live_state().agents.contains_key("worker-1"));
    println!(
        "Restored {} events in {:.2?} ({:.0} events/s)",
        num_events,
        elapsed,
        (num_events as f64) / elapsed.as_secs_f64()
    );
}
