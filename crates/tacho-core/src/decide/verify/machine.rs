//! Pure event-driven batch verification state machine (R17–R23, R26–R28, §5, §7).
//!
//! `step(state, event, ctx) -> (BatchState, Vec<VerifyAction>)` takes no I/O and no clocks.

use super::bisect::{step as bisect_step, BisectDecision, BisectProbe};
use super::result::{
    failed_areas_of, failed_tests_with, has_counterexample, is_not_run, merge_proofs, proved_areas,
    retry_areas, CheckResult, Member, RC_NOTHING_MERGED,
};
use super::status::{pending_status, CommitStatusPost};
use super::verdict::{
    complete_green_run, finish_batch, make_blame, record_skips, reusable_batch_id,
    run_check_action, stale_shots_step, wall_clock_expired, BatchState, BatchVerdict, Outcome,
    Phase, StaleShotsAction, UntiedRecord, VerifyAction, VerifyCtx, VerifyEvent,
};
use std::collections::BTreeMap;

/// Advance the batch verification state machine by one event.
pub fn step(
    state: &BatchState,
    event: &VerifyEvent,
    ctx: &VerifyCtx,
) -> (BatchState, Vec<VerifyAction>) {
    let mut st = state.clone();
    match (&st.phase.clone(), event) {
        (Phase::Done(_), _) => (st, Vec::new()),

        (Phase::Idle, VerifyEvent::Start { now }) => {
            if let Some(prev_id) =
                reusable_batch_id(&ctx.reusable, &st.main_sha, &st.heads, &st.prs, &st.areas)
            {
                let proved = st.areas.iter().map(|a| (a.clone(), prev_id.clone())).collect();
                let prs = st.prs.clone();
                return finish_batch(
                    st,
                    Outcome::Green,
                    &prs,
                    proved,
                    None,
                    Vec::new(),
                    *now,
                    ctx,
                    format!("reused green verdict from {prev_id} with identical main and heads"),
                );
            }
            if wall_clock_expired(st.started, *now, ctx.cfg.wall_clock_secs) {
                return finish_batch(
                    st,
                    Outcome::WallClock,
                    &[],
                    BTreeMap::new(),
                    None,
                    Vec::new(),
                    *now,
                    ctx,
                    "batch wall-clock budget expired before round 0 started".to_string(),
                );
            }
            let mut actions = Vec::new();
            let pending: Vec<CommitStatusPost> = st
                .prs
                .iter()
                .filter_map(|&pr| {
                    let sha = st.heads.get(&pr).map(String::as_str).unwrap_or("");
                    pending_status(pr, sha, &st.batch_id, &st.prs, &ctx.cfg)
                })
                .collect();
            if !pending.is_empty() {
                actions.push(VerifyAction::PostCommitStatuses {
                    statuses: pending,
                    reason: format!("mark batch {} members pending at start", st.batch_id),
                });
            }
            let label = st.batch_id.clone();
            let cand = st.prs.clone();
            st.phase = Phase::FullRun {
                round: 0,
                cand: cand.clone(),
                run_label: label.clone(),
            };
            actions.push(run_check_action(
                label,
                &cand,
                &st.heads,
                st.areas.clone(),
                format!("round 0 full batch merge-check for {cand:?}"),
            ));
            (st, actions)
        }

        (Phase::FullRun { round, run_label, .. }, VerifyEvent::RunCompleted { result, now, .. }) => {
            record_skips(&mut st, result);
            if is_not_run(result, &ctx.cfg.log_patterns) {
                return finish_batch(
                    st,
                    Outcome::Infrastructure,
                    &[],
                    BTreeMap::new(),
                    result.tree.clone(),
                    Vec::new(),
                    *now,
                    ctx,
                    format!("run {run_label} did not complete a code check (rc={})", result.rc),
                );
            }
            if result.rc == RC_NOTHING_MERGED || result.merged.is_empty() {
                return finish_batch(
                    st,
                    Outcome::NothingMerged,
                    &[],
                    BTreeMap::new(),
                    result.tree.clone(),
                    Vec::new(),
                    *now,
                    ctx,
                    format!("nothing merged in {run_label}"),
                );
            }
            if result.rc == 0 {
                let proved = proved_areas(result, run_label);
                return complete_green_run(
                    st,
                    Outcome::Green,
                    result.merged.clone(),
                    proved,
                    result.tree.clone(),
                    *now,
                    ctx,
                    Vec::new(),
                    &format!("round {round} full run {run_label} green"),
                );
            }

            let merged = result.merged.clone();
            let failed_areas = failed_areas_of(result, &st.areas);
            if let StaleShotsAction::RefreshAndInvalidate { pr, sha, redraw_areas, reason } =
                stale_shots_step(
                    &merged,
                    &st.heads,
                    &failed_areas,
                    ctx.pr_files.get(&merged[0]).map(Vec::as_slice).unwrap_or(&[]),
                    &ctx.cfg,
                )
            {
                return (st, vec![VerifyAction::RefreshStaleShots { pr, sha, areas: redraw_areas, reason }]);
            }

            let only_no_rerun =
                !failed_areas.is_empty() && failed_areas.iter().all(|a| ctx.cfg.no_rerun_areas.contains(a));
            let allow_rerun = st.reruns_used < ctx.cfg.reruns
                && !has_counterexample(&result.log, &ctx.cfg.log_patterns)
                && !(merged.len() == 1 && (ctx.cfg.no_rerun_single_member || only_no_rerun));

            if allow_rerun {
                st.reruns_used += 1;
                let r_areas = retry_areas(result, &st.areas, ctx.cfg.areas.gate.as_deref());
                let next_label = format!("{run_label}-retry");
                st.phase = Phase::RetryOnRed {
                    round: *round,
                    cand: merged.clone(),
                    first: result.clone(),
                    first_label: run_label.clone(),
                    retry_areas: r_areas.clone(),
                    run_label: next_label.clone(),
                };
                return (
                    st.clone(),
                    vec![run_check_action(
                        next_label,
                        &merged,
                        &st.heads,
                        r_areas.clone(),
                        format!("rerun failed/stopped areas {r_areas:?} on {merged:?}"),
                    )],
                );
            }
            if has_counterexample(&result.log, &ctx.cfg.log_patterns) && merged.len() > 1 {
                return finish_batch(
                    st,
                    Outcome::Counterexample,
                    &[],
                    BTreeMap::new(),
                    result.tree.clone(),
                    Vec::new(),
                    *now,
                    ctx,
                    format!("property counterexample in multi-PR batch {run_label}; refusing to bisect"),
                );
            }
            if merged.len() == 1 {
                let b = make_blame(&st, merged[0], result, &st.areas, run_label, *now, &ctx.cfg);
                st.blamed.push(b);
                return finish_batch(
                    st,
                    Outcome::Red,
                    &[],
                    BTreeMap::new(),
                    result.tree.clone(),
                    Vec::new(),
                    *now,
                    ctx,
                    format!("single-member PR #{} failed in {run_label}", merged[0]),
                );
            }
            start_or_advance_bisect(st, *round, merged, result.clone(), run_label.clone(), failed_areas, Vec::new(), *now, ctx)
        }

        (
            Phase::RetryOnRed { round, cand, first, first_label, retry_areas: r_areas, run_label },
            VerifyEvent::RunCompleted { result, now, .. },
        ) => {
            record_skips(&mut st, result);
            if is_not_run(result, &ctx.cfg.log_patterns) {
                return finish_batch(st, Outcome::Infrastructure, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("rerun {run_label} failed on infrastructure"));
            }
            if result.rc == 0 {
                let mut flake_actions = Vec::new();
                let tests = failed_tests_with(&first.log, &ctx.cfg.log_patterns);
                if !tests.is_empty() {
                    flake_actions.push(VerifyAction::RecordFlakes {
                        tests,
                        area: r_areas.first().cloned().unwrap_or_default(),
                        batch_id: st.batch_id.clone(),
                        log: first.log.clone(),
                        reason: format!("first run {first_label} failed and retry {run_label} passed"),
                    });
                }
                let proved = merge_proofs(first, first_label, result, run_label);
                if ctx.cfg.confirm_flaky_green && !ctx.cfg.pass_on_flaky_rerun {
                    let confirm_label = format!("{first_label}-confirm");
                    st.phase = Phase::ConfirmFlaky {
                        round: *round,
                        cand: cand.clone(),
                        merged: result.merged.clone(),
                        skipped: st.all_skipped.clone(),
                        skips: st.all_skips.clone(),
                        first_tree: result.tree.clone(),
                        proved,
                        retry_areas: r_areas.clone(),
                        run_label: confirm_label.clone(),
                    };
                    flake_actions.push(run_check_action(
                        confirm_label,
                        cand,
                        &st.heads,
                        r_areas.clone(),
                        format!("confirm flaky retry on {r_areas:?}"),
                    ));
                    return (st, flake_actions);
                }
                let base_outcome = if ctx.cfg.pass_on_flaky_rerun { Outcome::Green } else { Outcome::FlakyGreen };
                return complete_green_run(st, base_outcome, result.merged.clone(), proved, result.tree.clone(), *now, ctx, flake_actions, &format!("retry {run_label} passed"));
            }
            if has_counterexample(&result.log, &ctx.cfg.log_patterns) && cand.len() > 1 {
                return finish_batch(st, Outcome::Counterexample, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("counterexample on retry {run_label}"));
            }
            if cand.len() == 1 {
                let b = make_blame(&st, cand[0], result, r_areas, run_label, *now, &ctx.cfg);
                st.blamed.push(b);
                return finish_batch(st, Outcome::Red, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("single-member PR #{} failed on retry", cand[0]));
            }
            let narrow = failed_areas_of(first, &st.areas);
            start_or_advance_bisect(st, *round, cand.clone(), first.clone(), first_label.clone(), narrow, Vec::new(), *now, ctx)
        }

        (
            Phase::ConfirmFlaky { cand, merged, proved, retry_areas: r_areas, run_label, .. },
            VerifyEvent::RunCompleted { result, now, .. },
        ) => {
            if is_not_run(result, &ctx.cfg.log_patterns) {
                return finish_batch(st, Outcome::Infrastructure, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("confirm {run_label} failed on infra"));
            }
            if result.rc == 0 {
                let mut p = proved.clone();
                for (a, r) in proved_areas(result, run_label) {
                    p.insert(a, r);
                }
                return complete_green_run(st, Outcome::FlakyGreen, merged.clone(), p, result.tree.clone(), *now, ctx, Vec::new(), &format!("confirm {run_label} passed"));
            }
            if cand.len() == 1 {
                let b = make_blame(&st, cand[0], result, r_areas, run_label, *now, &ctx.cfg);
                st.blamed.push(b);
                return finish_batch(st, Outcome::Red, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("confirm {run_label} failed for PR #{}", cand[0]));
            }
            finish_batch(st, Outcome::RedOnce, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("confirm {run_label} failed"))
        }

        (
            Phase::Bisect { round, cand, full_failed, full_label, narrow_areas, probes, probe_len, run_label, .. },
            VerifyEvent::RunCompleted { result, now, .. },
        ) => {
            if is_not_run(result, &ctx.cfg.log_patterns) {
                let mut untied = Vec::new();
                if *probe_len == 2 && cand.len() >= 2 && probes.iter().any(|p| p.prefix_len == 1 && p.passed) {
                    let pair = vec![cand[0], cand[1]];
                    let shas = pair.iter().filter_map(|p| st.heads.get(p).map(|s| (*p, s.clone()))).collect();
                    untied.push(UntiedRecord {
                        prs: pair,
                        shas,
                        batch_id: st.batch_id.clone(),
                        summary: if !full_failed.line.is_empty() { full_failed.line.clone() } else { format!("failed: {}", narrow_areas.join(" ")) },
                        at: *now,
                    });
                }
                return finish_batch(st, Outcome::Infrastructure, &[], BTreeMap::new(), result.tree.clone(), untied, *now, ctx, format!("bisect probe {run_label} failed on infra"));
            }
            if result.rc != 0 {
                if has_counterexample(&result.log, &ctx.cfg.log_patterns) {
                    return finish_batch(st, Outcome::Counterexample, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("counterexample during bisect {run_label}"));
                }
                let probe_cand = &cand[..*probe_len];
                let only_no_rerun = !narrow_areas.is_empty() && narrow_areas.iter().all(|a| ctx.cfg.no_rerun_areas.contains(a));
                if st.reruns_used < ctx.cfg.reruns && !only_no_rerun {
                    st.reruns_used += 1;
                    let retry_lbl = format!("{run_label}-retry");
                    st.phase = Phase::BisectRetryOnRed {
                        round: *round,
                        cand: cand.clone(),
                        full_failed: full_failed.clone(),
                        full_label: full_label.clone(),
                        skipped: st.all_skipped.clone(),
                        skips: st.all_skips.clone(),
                        narrow_areas: narrow_areas.clone(),
                        probes: probes.clone(),
                        probe_len: *probe_len,
                        first_probe: result.clone(),
                        run_label: retry_lbl.clone(),
                    };
                    return (
                        st.clone(),
                        vec![run_check_action(retry_lbl, probe_cand, &st.heads, narrow_areas.clone(), format!("rerun red bisect probe of {probe_cand:?}"))],
                    );
                }
            }
            let mut next_probes = probes.clone();
            next_probes.push(BisectProbe { prefix_len: *probe_len, passed: result.rc == 0 });
            let (next_failed, next_lbl) = if result.rc != 0 { (result.clone(), run_label.clone()) } else { (full_failed.clone(), full_label.clone()) };
            start_or_advance_bisect(st, *round, cand.clone(), next_failed, next_lbl, narrow_areas.clone(), next_probes, *now, ctx)
        }

        (
            Phase::BisectRetryOnRed { round, cand, full_failed, full_label, narrow_areas, probes, probe_len, first_probe, run_label, .. },
            VerifyEvent::RunCompleted { result, now, .. },
        ) => {
            if is_not_run(result, &ctx.cfg.log_patterns) {
                return finish_batch(st, Outcome::Infrastructure, &[], BTreeMap::new(), result.tree.clone(), Vec::new(), *now, ctx, format!("bisect retry {run_label} failed on infra"));
            }
            let mut actions = Vec::new();
            if result.rc == 0 {
                let tests = failed_tests_with(&first_probe.log, &ctx.cfg.log_patterns);
                if !tests.is_empty() {
                    actions.push(VerifyAction::RecordFlakes {
                        tests,
                        area: narrow_areas.first().cloned().unwrap_or_default(),
                        batch_id: st.batch_id.clone(),
                        log: first_probe.log.clone(),
                        reason: format!("bisect probe failed then passed on retry {run_label}"),
                    });
                }
            }
            let mut next_probes = probes.clone();
            next_probes.push(BisectProbe { prefix_len: *probe_len, passed: result.rc == 0 });
            let (next_failed, next_lbl) = if result.rc != 0 { (result.clone(), run_label.clone()) } else { (full_failed.clone(), full_label.clone()) };
            let (next_st, mut more) = start_or_advance_bisect(st, *round, cand.clone(), next_failed, next_lbl, narrow_areas.clone(), next_probes, *now, ctx);
            actions.append(&mut more);
            (next_st, actions)
        }

        (_, VerifyEvent::ShotsRefreshed { pr, new_sha, now }) => finish_batch(
            st,
            Outcome::ShotsUpdated,
            &[],
            BTreeMap::new(),
            None,
            Vec::new(),
            *now,
            ctx,
            format!("refreshed visual goldens for PR #{pr} (new head {new_sha}); proof does not carry over to unchecked tree"),
        ),

        _ => (st, Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn start_or_advance_bisect(
    mut st: BatchState,
    round: u32,
    cand: Vec<u64>,
    last_red: CheckResult,
    last_red_label: String,
    narrow_areas: Vec<String>,
    probes: Vec<BisectProbe>,
    now: i64,
    ctx: &VerifyCtx,
) -> (BatchState, Vec<VerifyAction>) {
    match bisect_step(&cand, &probes) {
        BisectDecision::Probe {
            prefix_len,
            prefix,
            reason,
        } => {
            if wall_clock_expired(st.started, now, ctx.cfg.wall_clock_secs) {
                return finish_batch(
                    st,
                    Outcome::WallClock,
                    &[],
                    BTreeMap::new(),
                    last_red.tree.clone(),
                    Vec::new(),
                    now,
                    ctx,
                    "wall-clock budget expired before bisect probe".to_string(),
                );
            }
            let lbl = format!("{}-bisect-r{round}-{prefix_len}", st.batch_id);
            st.phase = Phase::Bisect {
                round,
                cand,
                full_failed: last_red,
                full_label: last_red_label,
                skipped: st.all_skipped.clone(),
                skips: st.all_skips.clone(),
                narrow_areas: narrow_areas.clone(),
                probes,
                probe_len: prefix_len,
                run_label: lbl.clone(),
            };
            let act = run_check_action(lbl, &prefix, &st.heads, narrow_areas, reason);
            (st, vec![act])
        }
        BisectDecision::Resolved { culprit, .. } => {
            if culprit == cand[0] {
                if wall_clock_expired(st.started, now, ctx.cfg.wall_clock_secs) {
                    return finish_batch(
                        st,
                        Outcome::WallClock,
                        &[],
                        BTreeMap::new(),
                        last_red.tree.clone(),
                        Vec::new(),
                        now,
                        ctx,
                        "wall-clock budget expired before main probe".to_string(),
                    );
                }
                if !probes.iter().any(|p| p.prefix_len == 0) {
                    let lbl = format!("{}-main-r{round}", st.batch_id);
                    st.phase = Phase::Bisect {
                        round,
                        cand,
                        full_failed: last_red,
                        full_label: last_red_label,
                        skipped: st.all_skipped.clone(),
                        skips: st.all_skips.clone(),
                        narrow_areas: narrow_areas.clone(),
                        probes,
                        probe_len: 0,
                        run_label: lbl.clone(),
                    };
                    let act = run_check_action(
                        lbl,
                        &[],
                        &st.heads,
                        narrow_areas,
                        "prefix [cand[0]] failed; probing main alone before blaming cand[0] (R21)"
                            .to_string(),
                    );
                    return (st, vec![act]);
                }
                if probes.iter().any(|p| p.prefix_len == 0 && !p.passed) {
                    return finish_batch(
                        st,
                        Outcome::MainBroken,
                        &[],
                        BTreeMap::new(),
                        last_red.tree.clone(),
                        Vec::new(),
                        now,
                        ctx,
                        "main alone failed; blaming nothing (R21)".to_string(),
                    );
                }
            }
            let b = make_blame(
                &st,
                culprit,
                &last_red,
                &narrow_areas,
                &last_red_label,
                now,
                &ctx.cfg,
            );
            st.blamed.push(b);
            let rest: Vec<u64> = cand.into_iter().filter(|&p| p != culprit).collect();
            if rest.is_empty() {
                return finish_batch(
                    st,
                    Outcome::Red,
                    &[],
                    BTreeMap::new(),
                    last_red.tree.clone(),
                    Vec::new(),
                    now,
                    ctx,
                    format!("isolated culprit PR #{culprit}; no remaining PRs"),
                );
            }
            let next_round = round + 1;
            if next_round >= ctx.cfg.rounds {
                return finish_batch(
                    st,
                    Outcome::GaveUp,
                    &[],
                    BTreeMap::new(),
                    last_red.tree.clone(),
                    Vec::new(),
                    now,
                    ctx,
                    format!("exhausted {} culprit isolation rounds", ctx.cfg.rounds),
                );
            }
            if wall_clock_expired(st.started, now, ctx.cfg.wall_clock_secs) {
                return finish_batch(
                    st,
                    Outcome::WallClock,
                    &[],
                    BTreeMap::new(),
                    last_red.tree.clone(),
                    Vec::new(),
                    now,
                    ctx,
                    "wall-clock budget expired before next round".to_string(),
                );
            }
            let next_lbl = format!("{}-r{next_round}", st.batch_id);
            st.phase = Phase::FullRun {
                round: next_round,
                cand: rest.clone(),
                run_label: next_lbl.clone(),
            };
            let act = run_check_action(next_lbl, &rest, &st.heads, st.areas.clone(), format!("round {next_round} full check on remaining {rest:?} after blaming PR #{culprit}"));
            (st, vec![act])
        }
        BisectDecision::Inconclusive { reason } => finish_batch(
            st,
            Outcome::RedOnce,
            &[],
            BTreeMap::new(),
            last_red.tree.clone(),
            Vec::new(),
            now,
            ctx,
            reason,
        ),
    }
}

/// Synchronous driver for `step` used in table tests / `FakeChecks` harnesses.
pub fn run_batch_to_verdict(
    initial: BatchState,
    ctx: &VerifyCtx,
    mut now: i64,
    mut runner: impl FnMut(&[Member], &[String], &str, &mut i64) -> CheckResult,
) -> (BatchState, BatchVerdict, Vec<VerifyAction>) {
    let mut all_actions = Vec::new();
    let (mut st, mut actions) = step(&initial, &VerifyEvent::Start { now }, ctx);
    all_actions.extend(actions.iter().cloned());
    loop {
        if let Phase::Done(v) = &st.phase {
            return (st.clone(), v.clone(), all_actions);
        }
        let next_run = actions.into_iter().find_map(|a| match a {
            VerifyAction::RunCheck {
                run_label,
                members,
                areas,
                ..
            } => Some((run_label, members, areas)),
            _ => None,
        });
        let Some((run_label, members, areas)) = next_run else {
            break;
        };
        let res = runner(&members, &areas, &run_label, &mut now);
        let (next_st, next_acts) = step(
            &st,
            &VerifyEvent::RunCompleted {
                run_label,
                result: res,
                now,
            },
            ctx,
        );
        all_actions.extend(next_acts.iter().cloned());
        st = next_st;
        actions = next_acts;
    }
    let fallback = match &st.phase {
        Phase::Done(v) => v.clone(),
        _ => BatchVerdict {
            outcome: Outcome::GaveUp,
            main_sha: None,
            passed: BTreeMap::new(),
            blamed: st.blamed.clone(),
            skipped: st.all_skipped.clone(),
            skips: st.all_skips.clone(),
            rebases: BTreeMap::new(),
            untied: Vec::new(),
            reason: "driver stopped without reaching Phase::Done".to_string(),
        },
    };
    (st, fallback, all_actions)
}
