//! Unit tests for `decide::ops::watchdog` (R68–R71).

use std::collections::BTreeMap;

use crate::decide::ops::watchdog::*;
use crate::decide::ops::OpsConfig;

#[test]
fn watchdog_down_reason_checks_active_and_heartbeat_age() {
    let policy = WatchdogPolicy::default();
    assert_eq!(
        down_reason(false, Some(10), &policy),
        Some("the daemon is not active".into())
    );
    assert_eq!(
        down_reason(true, None, &policy),
        Some("the daemon heartbeat does not exist".into())
    );
    assert_eq!(
        down_reason(true, Some(20 * 60), &policy),
        Some("the daemon heartbeat has not advanced for 20 minutes".into())
    );
    assert_eq!(down_reason(true, Some(60), &policy), None);
}

#[test]
fn watchdog_alert_lifecycle_open_update_close_and_none() {
    let cfg = OpsConfig::default();
    let tail = vec!["tick started".to_string()];

    // Down with no open issue -> Open
    let act1 = down_alert(Some("the daemon is not active"), &tail, None, &cfg);
    assert_eq!(act1.kind, AlarmKind::Open);

    // Down with existing open issue and changed body -> Update
    let open = OpenIssue {
        number: 55,
        body: "old body".into(),
    };
    let act2 = down_alert(Some("the daemon is not active"), &tail, Some(&open), &cfg);
    assert_eq!(act2.kind, AlarmKind::Update);
    assert_eq!(act2.number, Some(55));

    // Down with existing open issue and identical body -> None
    let same = OpenIssue {
        number: 55,
        body: act1.body.clone(),
    };
    let act_same = down_alert(Some("the daemon is not active"), &tail, Some(&same), &cfg);
    assert_eq!(act_same.kind, AlarmKind::None);

    // Healthy with open issue -> Close
    let act3 = down_alert(None, &tail, Some(&open), &cfg);
    assert_eq!(act3.kind, AlarmKind::Close);
    assert_eq!(act3.number, Some(55));

    // Healthy with no open issue -> None
    let act4 = down_alert(None, &tail, None, &cfg);
    assert_eq!(act4.kind, AlarmKind::None);
}

#[test]
fn watchdog_drifted_units_detects_missing_or_mismatched_units_in_shipped_order() {
    let shipped = vec![
        ("tacho.service".to_string(), "unit-a".to_string()),
        ("tacho-watchdog.timer".to_string(), "unit-b".to_string()),
    ];
    let mut installed = BTreeMap::new();
    installed.insert("tacho.service".into(), "unit-a".into());
    installed.insert("tacho-watchdog.timer".into(), "old".into());

    assert_eq!(
        drifted_units(&shipped, &installed),
        vec!["tacho-watchdog.timer"]
    );
}
