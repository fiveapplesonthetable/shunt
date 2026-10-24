//! Parity tests for `decide::ops::tell` and poll loop (RS1–RS8, EV1–EV8, PD1–PD3, UT1–UT6, AR1–AR6).

use crate::decide::ops::schedule::*;
use crate::decide::ops::tell::*;
use crate::decide::ops::OpsConfig;

fn cfg() -> OpsConfig {
    OpsConfig {
        owner_issue: 10,
        ..Default::default()
    }
}

#[test]
fn rs1_and_rs2_ready_set_tells_once_and_retells_only_when_new_pr_or_head_joins() {
    let c = cfg();
    let mut ledger = Ledger::default();
    let facts1 = vec![Fact::ReadySet {
        train: "t1".into(),
        prs: vec![ReadyPr {
            number: 1,
            head: "sha1".into(),
            title: "First".into(),
            body: "Body 1".into(),
        }],
        land_command: "tacho land t1".into(),
    }];
    let p1 = pending(&ledger, &facts1, 1000, &c);
    assert_eq!(p1.len(), 1);
    let (ledger2, _) = settle(ledger, &p1[0], Delivery::Delivered, 1000, 500);
    ledger = ledger2;
    assert!(pending(&ledger, &facts1, 1010, &c).is_empty());

    // New PR joins the ready set -> retells
    let facts2 = vec![Fact::ReadySet {
        train: "t1".into(),
        prs: vec![
            ReadyPr {
                number: 1,
                head: "sha1".into(),
                title: "First".into(),
                body: "Body 1".into(),
            },
            ReadyPr {
                number: 2,
                head: "sha2".into(),
                title: "Second".into(),
                body: "Body 2".into(),
            },
        ],
        land_command: "tacho land t1".into(),
    }];
    assert_eq!(pending(&ledger, &facts2, 1100, &c).len(), 1);
}

#[test]
fn rs3_to_rs7_decision_and_main_job_tell_once_and_prune_when_resolved() {
    let c = cfg();
    let mut ledger = Ledger::default();
    let main_fact = vec![Fact::MainJobFailed {
        run: 500,
        job: "ci".into(),
        sha: "abcdef1234".into(),
        first_line: "test failed".into(),
        url: "https://example.com/500".into(),
    }];
    let p = pending(&ledger, &main_fact, 1000, &c);
    assert_eq!(p.len(), 1);
    ledger = settle(ledger, &p[0], Delivery::Delivered, 1000, 500).0;
    assert!(pending(&ledger, &main_fact, 1010, &c).is_empty());

    // Decision with sentence A told once; sentence B retells; pruning when gone forgets Decision
    let dec_a = vec![Fact::Decision {
        key: "pr-42".into(),
        sentence: "conflict".into(),
    }];
    let pa = pending(&ledger, &dec_a, 1000, &c);
    assert_eq!(pa.len(), 1);
    ledger = settle(ledger, &pa[0], Delivery::Delivered, 1000, 500).0;
    assert!(pending(&ledger, &dec_a, 1010, &c).is_empty());

    let dec_b = vec![Fact::Decision {
        key: "pr-42".into(),
        sentence: "oversized".into(),
    }];
    assert_eq!(pending(&ledger, &dec_b, 1020, &c).len(), 1);

    // Prune when no standing facts -> Decision is forgotten and will tell again if it returns
    ledger = prune(ledger, &[]);
    assert_eq!(pending(&ledger, &dec_a, 1030, &c).len(), 1);
}

#[test]
fn rs8_ledger_is_bounded_to_cap_per_kind() {
    let mut ledger = Ledger::default();
    for i in 0..10 {
        ledger.told.insert(
            format!("decision:k{i}"),
            Told {
                fingerprint: "fp".into(),
                at: i,
            },
        );
    }
    let bounded = bound(ledger, FactKind::Decision, 5);
    assert_eq!(bounded.told.len(), 5);
    assert!(bounded.told.contains_key("decision:k9"));
    assert!(!bounded.told.contains_key("decision:k0"));
}

#[test]
fn ev1_to_ev8_event_facts_and_daily_digest() {
    let c = cfg();
    let ledger = Ledger::default();
    let ev = vec![
        Fact::BatchRed {
            id: "b1".into(),
            finished_at: 950,
            lines: vec!["rust failed".into()],
        },
        Fact::DeployStarted {
            run: 900,
            sha: "11112222".into(),
            url: "https://example.com/900".into(),
        },
        Fact::DeployDone {
            run: 901,
            sha: "11112222".into(),
            ok: false,
            what: "deploy failed".into(),
        },
    ];
    let notices = pending(&ledger, &ev, 1000, &c);
    assert_eq!(notices.len(), 3);

    // Daily digest state machine (EV7, EV8)
    let d0 = Digest::default();
    let step0 = digest_step(&d0, &[], &[], 1000, &c);
    assert_eq!(step0, DigestStep::Stamp { now: 1000 });
    let mut d = settle_digest(d0, &step0, Delivery::Delivered, 1000);
    d.lines.push((1100, "merged #12".into()));
    assert!(matches!(
        digest_step(&d, &[], &[], 2000, &c),
        DigestStep::Wait { .. }
    ));
    let step_send = digest_step(&d, &["#55 held".into()], &[], 1000 + 86400, &c);
    let DigestStep::Send { mail, .. } = &step_send else {
        panic!("expected Send");
    };
    assert!(mail.body.contains("merged #12"));
    assert!(mail.body.contains("#55 held"));
    let d_after = settle_digest(d, &step_send, Delivery::Delivered, 1000 + 86400);
    assert!(d_after.lines.is_empty());
}

#[test]
fn pd1_to_pd3_failed_delivery_is_retried_and_not_configured_records_attention() {
    let c = cfg();
    let ledger = Ledger::default();
    let facts = vec![Fact::Decision {
        key: "issue-42".into(),
        sentence: "needs human".into(),
    }];
    let notices = pending(&ledger, &facts, 1000, &c);
    // Failed delivery -> ledger unchanged, retried next tick
    let (ledger_fail, att_fail) = settle(ledger, &notices[0], Delivery::Failed, 1000, 500);
    assert!(att_fail.is_none());
    assert_eq!(pending(&ledger_fail, &facts, 1060, &c).len(), 1);

    // NotConfigured -> recorded in ledger and returns attention line
    let (ledger_nc, att_nc) = settle(ledger_fail, &notices[0], Delivery::NotConfigured, 1060, 500);
    assert!(att_nc.is_some());
    assert!(pending(&ledger_nc, &facts, 1120, &c).is_empty());
}

#[test]
fn ut1_to_ut6_untied_failure_counter_halts_at_threshold_and_resets_on_new_sha() {
    let policy = TellPolicy::default();
    let u1 = record_untied(None, "sha-a", "infra timeout");
    assert_eq!(u1.times, 1);
    assert!(!untied_stopped(&u1, &policy));

    let u2 = record_untied(Some(&u1), "sha-a", "infra timeout");
    assert_eq!(u2.times, 2);
    assert!(untied_stopped(&u2, &policy));

    // New commit SHA resets counter to 1
    let u3 = record_untied(Some(&u2), "sha-b", "infra timeout");
    assert_eq!(u3.times, 1);
    assert!(!untied_stopped(&u3, &policy));
}

#[test]
fn ar1_to_ar6_poll_loop_isolation_and_main_moved() {
    let policy = SchedulePolicy::default();
    assert!(!main_moved("", "abc"));
    assert!(!main_moved("abc", ""));
    assert!(!main_moved("abc", "abc\n"));
    assert!(main_moved("abc", "def"));

    let d1 = poll_decision(
        1000,
        "abc",
        1065,
        Ok(false),
        Err("ssh timeout".into()),
        &policy,
    );
    assert!(d1.run_tick);
    assert_eq!(d1.new_last_main, "abc");
    assert_eq!(d1.errors.len(), 1);

    let d2 = poll_decision(
        1000,
        "abc",
        1010,
        Err("gh api 502".into()),
        Ok("def".into()),
        &policy,
    );
    assert!(d2.run_tick);
    assert_eq!(d2.new_last_main, "def");
    assert_eq!(d2.errors.len(), 1);

    let d3 = poll_decision(1000, "abc", 1020, Ok(false), Ok("abc".into()), &policy);
    assert!(!d3.run_tick);
}
