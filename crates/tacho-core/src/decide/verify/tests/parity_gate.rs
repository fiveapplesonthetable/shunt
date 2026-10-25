//! Parity tests for running a batch's areas with a gate area first.
//!
//! Reference behaviour covered (on top of `warm_ci_tests`, which covers the wave split and
//! that a trip leaves an already-finished area alone):
//!
//! - a client that does not compile (the gate fails) stops the long areas: none of the second
//!   wave runs, and only the gate's result is recorded;
//! - a failed Rust area fails the run, and its own return code is recorded;
//! - areas still running when a sibling fails are killed and reported as stopped, not failed.
//!
//! The runner is modelled with the pure [`execution_waves`], [`StopTracker`] and [`summary`].

use super::*;

const GATE: &str = "compile";

fn areas() -> Vec<String> {
    ["rust", "console", GATE, "android"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn rc(code: i32) -> AreaResult {
    if code == 0 {
        ok_area(1)
    } else {
        fail_area(code)
    }
}

/// Runs `areas` wave by wave the way the area runner does, with `raw_rc` giving each area's
/// exit code. Returns the areas started, in order, and the recorded results.
fn run(
    areas: &[String],
    raw_rc: impl Fn(&str) -> i32,
) -> (Vec<String>, BTreeMap<String, AreaResult>) {
    let mut stop = StopTracker::new();
    let mut started = Vec::new();
    let mut recorded = BTreeMap::new();
    for wave in execution_waves(areas, Some(GATE)) {
        let mut running = Vec::new();
        for area in &wave {
            if stop.start(area) {
                continue;
            }
            started.push(area.clone());
            running.push(area.clone());
        }
        for area in running {
            let code = raw_rc(&area);
            if code == 0 {
                stop.finish(&area);
            } else if !stop.killed.contains(&area) {
                stop.fail(&area);
            }
            recorded.insert(area.clone(), rc(stop.effective_rc(&area, code)));
        }
    }
    (started, recorded)
}

#[test]
fn the_gate_starts_before_any_other_area() {
    let (started, recorded) = run(&areas(), |_| 0);
    assert_eq!(started[0], GATE);
    let mut sorted = started.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["android", "compile", "console", "rust"]);
    assert!(summary(&areas(), &recorded).starts_with("passed:"));
}

#[test]
fn a_failed_rust_area_fails_the_run() {
    let (_, recorded) = run(&areas(), |a| i32::from(a == "rust"));
    assert_eq!(recorded["rust"].rc, 1);
    assert_eq!(summary(&areas(), &recorded), "failed: rust");
}

#[test]
fn a_client_that_does_not_compile_stops_the_long_areas() {
    let (started, recorded) = run(&areas(), |a| i32::from(a == GATE));
    assert_eq!(started, vec![GATE.to_string()]);
    assert_eq!(recorded.keys().collect::<Vec<_>>(), vec![GATE]);
    assert_eq!(recorded[GATE].rc, 1);
    assert_eq!(summary(&[GATE.to_string()], &recorded), "failed: compile");
}

#[test]
fn the_areas_still_running_are_killed_and_not_called_failed() {
    let names: Vec<String> = ["rust", "console", "e2e"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut stop = StopTracker::new();
    for a in &names {
        assert!(!stop.start(a));
    }
    let mut killed = stop.fail("rust");
    killed.sort();
    assert_eq!(killed, vec!["console", "e2e"]);
    let recorded: BTreeMap<String, AreaResult> = [("rust", 1), ("console", 143), ("e2e", 143)]
        .into_iter()
        .map(|(a, code)| (a.to_string(), rc(stop.effective_rc(a, code))))
        .collect();
    let rcs: BTreeMap<&str, i32> = recorded.iter().map(|(a, r)| (a.as_str(), r.rc)).collect();
    assert_eq!(
        rcs,
        BTreeMap::from([("console", RC_STOPPED), ("e2e", RC_STOPPED), ("rust", 1)])
    );
    assert_eq!(summary(&names, &recorded), "failed: rust");
}
