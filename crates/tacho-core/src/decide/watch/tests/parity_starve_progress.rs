//! Parity tests for CPU starvation planning and running, ported from the reference starve tests
//! not already covered by `starve_jobs.rs`:
//! - burners are four times the size of the slice (default and explicit CPU sets);
//! - a plan with a problem starts nothing: no burner is spawned and no command is called.

use std::collections::BTreeMap;

use crate::decide::watch::config::{StarveConfig, WatchConfig};
use crate::decide::watch::starve::{plan_starve, run_starve};

#[test]
fn burners_are_four_times_the_size_of_the_slice() {
    let c = StarveConfig::default();
    assert_eq!(plan_starve(72, 4, None, true, true, None, &c).burners, 16);
    let explicit = plan_starve(72, 4, Some("0-3,8"), true, true, None, &c);
    assert_eq!((explicit.cpus.as_str(), explicit.burners), ("0-3,8", 20));
}

#[test]
fn a_plan_with_a_problem_starts_nothing() {
    let plan = plan_starve(72, 16, None, false, false, None, &StarveConfig::default());
    assert!(plan.wrap.is_none());
    let (mut spawned, mut called, mut said) = (0, 0, Vec::new());
    let rc = run_starve(
        &["cargo".to_string()],
        &plan,
        false,
        &BTreeMap::new(),
        &WatchConfig::default(),
        |_, _| {
            spawned += 1;
            1
        },
        |_, _, _| {
            called += 1;
            Ok(0)
        },
        |_| {},
        |m| said.push(m.to_string()),
    );
    assert_eq!(rc, 1);
    assert_eq!((spawned, called), (0, 0));
    assert_eq!(said, vec![plan.problem.clone()]);
}
