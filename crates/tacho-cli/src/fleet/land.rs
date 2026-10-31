//! Landing verified merge trains and ready PRs (`tacho land`) with base SHA and post-merge
//! tree hash verification.

use std::collections::HashMap;
use std::io;

use tacho_core::decide::land::{finished, mismatch, open_for, queue, TrainRecord};
use tacho_core::decide::plugin::{ForgePlugin, PullDisposition};
use tacho_core::TaskStatus;

use crate::cli::LandArgs;

use super::store::{emit_decision_event, now_ns, open_fleet_store, resolve_verification_main_sha};

fn complete_train_proof(state: &tacho_daemon::FleetState, train: &TrainRecord) -> io::Result<()> {
    let batch = state
        .batches
        .iter()
        .find(|batch| batch.id == train.batch_id)
        .ok_or_else(|| io::Error::other("train has no verification batch"))?;
    if batch.status != "passed"
        || !batch.flakes.is_empty()
        || !batch.skipped.is_empty()
        || batch.mode != tacho_core::decide::verify::BatchMode::Presubmit
        || batch.prs != train.prs
        || batch.shas != train.shas
        || batch.main_sha != train.base
        || batch.tree.as_deref() != Some(train.tree.as_str())
        || train.prs.is_empty()
    {
        return Err(io::Error::other(
            "train does not match a complete passed batch",
        ));
    }
    for number in &train.prs {
        let proof = state
            .passed
            .get(number)
            .ok_or_else(|| io::Error::other("train member has no passed proof"))?;
        if proof.batch_id != batch.id
            || train.shas.get(number) != Some(&proof.sha)
            || proof.main_sha != train.base
            || proof.tree != train.tree
        {
            return Err(io::Error::other("train member proof identity changed"));
        }
        if let Some(pull) = state
            .open_prs_cache
            .iter()
            .find(|pull| pull.number == *number)
        {
            let required =
                tacho_core::decide::verify::areas_for_files(&pull.files, &state.area_rules);
            if pull.files.is_empty()
                || required.is_empty()
                || required
                    .iter()
                    .any(|area| !proof.areas.contains(area) || !batch.areas.contains(area))
            {
                return Err(io::Error::other(
                    "train member proof does not cover its required areas",
                ));
            }
        }
    }
    Ok(())
}

/// Options for `tacho land`.
#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    pub train: Option<String>,
    pub dry_run: bool,
}

impl From<LandArgs> for LandOptions {
    fn from(args: LandArgs) -> Self {
        Self {
            train: args.train.or(args.train_flag),
            dry_run: args.dry_run,
        }
    }
}

/// Verifies pre-merge base SHA and post-merge `main^{tree}` integrity for a merge train.
///
/// 1. Checks that `current_main` matches `train.base` (refusing if `main` moved since verification).
/// 2. Checks that `post_merge_main_tree` equals `train.tree` (flagging any tree mismatch).
pub fn verify_train_land_integrity(
    train: &TrainRecord,
    current_main: &str,
    post_merge_main_tree: &str,
) -> Result<(), String> {
    if !train.base.is_empty() && !current_main.is_empty() && train.base != current_main {
        return Err(format!(
            "refusing to land train {}: main moved from {} to {}",
            train.id,
            &train.base[..train.base.len().min(12)],
            &current_main[..current_main.len().min(12)]
        ));
    }
    let landed = finished(train);
    if let Some(err) = mismatch(&landed, post_merge_main_tree) {
        return Err(err);
    }
    Ok(())
}

/// Executes `tacho land`.
pub fn execute_land(args: LandArgs) -> io::Result<()> {
    execute_land_with(LandOptions::from(args))
}

/// Executes `tacho land` with [`LandOptions`].
pub fn execute_land_with(args: LandOptions) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let current_main = resolve_verification_main_sha(&cwd, &state)?;

    let target_open = if let Some(ref tid) = args.train {
        let found = state
            .trains
            .iter()
            .find(|t| (t.id == *tid || format!("train/{}", t.id) == *tid) && t.status == "open")
            .cloned();
        if found.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("open train '{tid}' not found"),
            ));
        }
        found
    } else {
        open_for(&state.trains).cloned()
    };

    if let Some(open) = target_open {
        let merge_enabled = match std::env::var("TACHO_LAND_MERGE") {
            Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
            Err(_) => state.merge_prs,
        };
        if !merge_enabled {
            println!("Merging is disabled; train {} remains open.", open.id);
            return Ok(());
        }
        complete_train_proof(&state, &open)?;
        let gh_forge = tacho_daemon::project::forge_repository(&state).map(|r| {
            tacho_core::decide::plugin::GitHubForge::new(r).with_allow_merge(merge_enabled)
        });
        let forge = gh_forge
            .as_ref()
            .ok_or_else(|| io::Error::other("merging requires a configured forge repository"))?;
        let mut runner = |argv: &[String]| {
            tacho_daemon::exec::observe::run_argv(
                Some(&cwd),
                argv,
                std::time::Duration::from_secs(30),
            )
        };
        let mut expected_main = open.base.clone();
        let mut progress = open.clone();
        progress.landed_prs.clear();
        let mut remaining = Vec::new();
        for &number in &open.prs {
            if state.holds.contains_key(&format!("pr:{number}")) {
                return Err(io::Error::other(format!("PR #{number} is held")));
            }
            let pull = forge
                .merge_pull_facts(number, &mut runner)
                .map_err(io::Error::other)?;
            if open.shas.get(&number) != Some(&pull.head) || pull.base != "main" {
                return Err(io::Error::other(format!(
                    "PR #{number} no longer has its verified head and base"
                )));
            }
            match pull.disposition {
                PullDisposition::Merged if remaining.is_empty() => {
                    expected_main = pull
                        .merge_commit
                        .ok_or_else(|| io::Error::other("merged member lacks its merge commit"))?;
                    progress.landed_prs.push(number);
                }
                PullDisposition::Open => remaining.push((number, pull)),
                _ => {
                    return Err(io::Error::other(
                        "train members are closed or merged outside their verified order",
                    ))
                }
            }
        }
        if current_main != expected_main {
            return Err(io::Error::other(
                "canonical main moved outside the verified train prefix",
            ));
        }
        if args.dry_run {
            println!(
                "Would land train {}: PRs {:?}",
                open.id,
                remaining
                    .iter()
                    .map(|(number, _)| number)
                    .collect::<Vec<_>>()
            );
            return Ok(());
        }
        for (number, pull) in remaining {
            if resolve_verification_main_sha(&cwd, &state)? != expected_main {
                return Err(io::Error::other(
                    "canonical main moved before the next train member",
                ));
            }
            if pull.draft {
                forge.mark_pr_ready(number).map_err(io::Error::other)?;
            }
            forge
                .merge_pr_at_head(number, &pull.head)
                .map_err(io::Error::other)?;
            let merged = forge
                .merge_pull_facts(number, &mut runner)
                .map_err(io::Error::other)?;
            if merged.disposition != PullDisposition::Merged
                || merged.head != pull.head
                || merged.base != "main"
            {
                return Err(io::Error::other(
                    "the forge has not confirmed the expected member merge",
                ));
            }
            expected_main = merged
                .merge_commit
                .ok_or_else(|| io::Error::other("confirmed merge lacks a commit identity"))?;
            progress.landed_prs.push(number);
            store.mutate(None, |state| {
                state.merged_prs.insert(number);
                if let Some(train) = state.trains.iter_mut().find(|train| train.id == open.id) {
                    *train = progress.clone();
                }
            })?;
        }
        if resolve_verification_main_sha(&cwd, &state)? != expected_main {
            return Err(io::Error::other(
                "canonical main moved after the train merge",
            ));
        }
        let post_merge_tree = forge
            .commit_tree(&expected_main, &mut runner)
            .map_err(io::Error::other)?;
        let mut landed = finished(&open);
        if let Some(err) = mismatch(&landed, &post_merge_tree) {
            landed.status = "mismatch".to_string();
            store.mutate(Some(("land", "train_mismatch", &open.id, &err)), |st| {
                for t in &mut st.trains {
                    if t.id == open.id {
                        *t = landed.clone();
                    }
                }
            })?;
            store.emit_state_band(
                &format!("train:{}", open.id),
                "mismatch",
                TaskStatus::Failed,
                &err,
                now_ns(),
            )?;
            emit_decision_event(&format!("train:{}", open.id), "land:mismatch", &err);
            let mut hands = super::watch::CliHands {
                home: crate::paths::resolve_home(),
            };
            let _ = tacho_daemon::exec::Hands::notify_owner(
                &mut hands,
                &format!("[tacho] train {} mismatch", open.id),
                &err,
            );
            if let Some(ref forge) = gh_forge {
                use tacho_core::decide::plugin::ForgePlugin;
                let _ = forge.set_issue(&format!("train {} mismatch", open.id), &err);
            }
            return Err(io::Error::other(err));
        }
        let because = format!("landed train {} (PRs {:?})", open.id, open.prs);
        store.mutate(Some(("land", "land_train", &open.id, &because)), |st| {
            for &p in &open.prs {
                st.merged_prs.insert(p);
            }
            for t in &mut st.trains {
                if t.id == open.id {
                    *t = landed.clone();
                }
            }
        })?;
        store.emit_state_band(
            &format!("train:{}", open.id),
            "landed",
            TaskStatus::Done,
            &because,
            now_ns(),
        )?;
        emit_decision_event(&format!("train:{}", open.id), "land:done", &because);
        println!("{because}");
        return Ok(());
    }

    let ready = queue(
        &state.open_prs_cache,
        &state.passed,
        &state.batches,
        &state.holds,
        &current_main,
        &HashMap::new(),
        &state.area_rules,
    );
    if ready.is_empty() {
        println!("Nothing ready to land.");
        return Ok(());
    }
    for item in &ready {
        println!(
            "Ready to land PR #{} ({}) from batch {}",
            item.pr, item.title, item.batch_id
        );
    }
    Ok(())
}
