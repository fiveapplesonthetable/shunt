//! Merge train assembly (`tacho train`), candidate ordering, conflict policy,
//! compile-breaker bisection, `tacho release-train` 4-state notice machine, and `tacho queue`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::dispatch::{PullFacts, TodoItem};
use tacho_core::decide::land::{
    drop_breakers, eligible, make_train, open_for, order_train_prs, queue, settle, waiting_for,
    with_train, TrainPrState, TrainRecord,
};
use tacho_core::decide::verify::{batch_refs_moved, parents, BatchMode, BatchRun};
use tacho_core::TaskStatus;
use tacho_daemon::{FleetState, FleetStore};

use crate::cli::{QueueArgs, ReleaseTrainArgs, TrainArgs};

use super::batch::{execute_batch_with, BatchOptions, BatchOrigin};
use super::store::{
    emit_decision_event, load_fleet_meta, mutate_fleet_meta, now_ns, now_secs, open_fleet_store,
    resolve_live_main_sha, resolve_live_pr_heads, NoticeState,
};
use crate::kit::use_private_tmp;

/// Options for `tacho train`.
#[derive(Debug, Clone, Default)]
pub struct TrainOptions {
    pub prs: Vec<u64>,
    pub max: Option<usize>,
    pub from_batch: Option<String>,
    pub json: bool,
    pub run_batch: bool,
    pub compile_cmd: Option<String>,
}

impl From<TrainArgs> for TrainOptions {
    fn from(args: TrainArgs) -> Self {
        Self {
            prs: args.prs,
            max: args.max,
            from_batch: args.from_batch,
            json: args.json,
            run_batch: false,
            compile_cmd: None,
        }
    }
}

/// Returns `true` if `path` touches a frontend or UI asset.
pub fn is_frontend_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.starts_with("web/")
        || p.starts_with("frontend/")
        || p.starts_with("ui/")
        || p.starts_with("app/src/main/res/")
        || p.ends_with(".tsx")
        || p.ends_with(".jsx")
        || p.ends_with(".css")
        || p.ends_with(".scss")
        || p.ends_with(".vue")
        || p.ends_with(".svelte")
        || p.ends_with(".png")
}

/// Extracts screen names touched by a PR's file list (for grouping adjacent screens together).
pub fn candidate_screens(files: &[String]) -> BTreeSet<String> {
    let mut screens = BTreeSet::new();
    for f in files {
        let lower = f.to_ascii_lowercase();
        if lower.ends_with(".png")
            || lower.contains("/screens/")
            || lower.contains("/pages/")
            || lower.contains("/views/")
        {
            if let Some(stem) = Path::new(f).file_stem().and_then(|s| s.to_str()) {
                screens.insert(stem.to_ascii_lowercase());
            }
        }
    }
    screens
}

/// Orders merge train candidates:
/// 1. Non-frontend PRs first (smallest file count to largest, tie-broken by PR number).
/// 2. Frontend PRs next, ordered small-to-large while grouping PRs that share adjacent screens.
pub fn order_train_candidates(prs: &[PullFacts], max: Option<usize>) -> Vec<PullFacts> {
    let mut non_fe: Vec<PullFacts> = Vec::new();
    let mut fe: Vec<PullFacts> = Vec::new();
    for pr in prs {
        if pr.files.iter().any(|f| is_frontend_path(f)) {
            fe.push(pr.clone());
        } else {
            non_fe.push(pr.clone());
        }
    }
    non_fe.sort_by_key(|p| (p.files.len(), p.number));
    fe.sort_by_key(|p| (p.files.len(), p.number));

    let mut ordered_fe: Vec<PullFacts> = Vec::with_capacity(fe.len());
    let mut used = vec![false; fe.len()];
    for i in 0..fe.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let screens_i = candidate_screens(&fe[i].files);
        ordered_fe.push(fe[i].clone());
        if !screens_i.is_empty() {
            for j in (i + 1)..fe.len() {
                if !used[j] {
                    let screens_j = candidate_screens(&fe[j].files);
                    if !screens_i.is_disjoint(&screens_j) {
                        used[j] = true;
                        ordered_fe.push(fe[j].clone());
                    }
                }
            }
        }
    }

    let mut out = non_fe;
    out.extend(ordered_fe);
    if let Some(m) = max {
        out.truncate(m);
    }
    out
}

/// Resolution policy for files that conflict when merging a candidate PR onto the train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainConflictResolution {
    /// All conflicting files are `.png` screenshots -> take later (`git checkout --theirs`).
    TakeLaterPng { files: Vec<String> },
    /// Conflicting file is `generators.json` -> take later and run generator regeneration.
    RegenerateGeneratorsJson { files: Vec<String> },
    /// Combined `.png` and `generators.json` auto-resolvable conflicts.
    AutoResolvePngAndGenerators {
        png_files: Vec<String>,
        generator_files: Vec<String>,
    },
    /// Any other file conflicted -> drop PR from the train and queue `rebase-<n>`.
    DropAndQueueRebase { conflicting_files: Vec<String> },
}

/// Classifies a set of conflicting paths according to the merge train conflict policy:
/// - `.png` files take the later PR's version (`--theirs`).
/// - `generators.json` regenerates after taking the later PR's version.
/// - Any other conflicting file drops the PR and queues `rebase-<n>`.
pub fn classify_train_conflict(conflicting_files: &[String]) -> TrainConflictResolution {
    if conflicting_files.is_empty() {
        return TrainConflictResolution::DropAndQueueRebase {
            conflicting_files: Vec::new(),
        };
    }
    let mut png_files = Vec::new();
    let mut generator_files = Vec::new();
    let mut other_files = Vec::new();

    for f in conflicting_files {
        let trimmed = f.trim();
        let file_name = Path::new(trimmed)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(trimmed);
        if trimmed.to_ascii_lowercase().ends_with(".png") {
            png_files.push(trimmed.to_string());
        } else if file_name == "generators.json" {
            generator_files.push(trimmed.to_string());
        } else {
            other_files.push(trimmed.to_string());
        }
    }

    if !other_files.is_empty() {
        TrainConflictResolution::DropAndQueueRebase {
            conflicting_files: other_files,
        }
    } else if !png_files.is_empty() && !generator_files.is_empty() {
        TrainConflictResolution::AutoResolvePngAndGenerators {
            png_files,
            generator_files,
        }
    } else if !png_files.is_empty() {
        TrainConflictResolution::TakeLaterPng { files: png_files }
    } else {
        TrainConflictResolution::RegenerateGeneratorsJson {
            files: generator_files,
        }
    }
}

/// Selects default unheld done/passed PRs when `tacho train` is invoked without explicit PR numbers.
pub fn select_default_train_prs(st: &FleetState) -> Vec<PullFacts> {
    let mut out = Vec::new();
    for pr in &st.open_prs_cache {
        if pr.draft || !pr.state.eq_ignore_ascii_case("OPEN") {
            continue;
        }
        if st.merged_prs.contains(&pr.number) || st.closed_prs.contains(&pr.number) {
            continue;
        }
        if st.holds.contains_key(&format!("pr:{}", pr.number)) {
            continue;
        }
        if st.failed.contains_key(&pr.number) {
            continue;
        }
        if st.passed.contains_key(&pr.number) {
            out.push(pr.clone());
        }
    }
    out
}

/// Assembles a merge train from candidate PRs, applying ordering, conflict resolution,
/// `drop_breakers` compile bisection (queueing `fix-compile-<n>` for compile breakers and
/// `rebase-<n>` for non-auto-resolvable conflicts), and optionally launching a train batch.
pub fn assemble_train(
    store: &mut FleetStore,
    cwd: &Path,
    opts: &TrainOptions,
) -> io::Result<TrainRecord> {
    let state = store.load()?;
    let now = now_secs();
    let base_sha = resolve_live_main_sha(cwd);

    let candidates: Vec<PullFacts> = if !opts.prs.is_empty() {
        let mut list = Vec::new();
        for &n in &opts.prs {
            if let Some(p) = state.open_prs_cache.iter().find(|p| p.number == n) {
                list.push(p.clone());
            } else if let Some(passed) = state.passed.get(&n) {
                list.push(PullFacts {
                    number: n,
                    branch: format!("pr-{n}"),
                    head: passed.sha.clone(),
                    base: "main".to_string(),
                    state: "OPEN".to_string(),
                    draft: false,
                    mergeable: "MERGEABLE".to_string(),
                    files: Vec::new(),
                    title: format!("PR #{n}"),
                    ..Default::default()
                });
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("PR #{n} not found in open or passed PRs"),
                ));
            }
        }
        order_train_candidates(&list, opts.max)
    } else {
        let defaults = select_default_train_prs(&state);
        order_train_candidates(&defaults, opts.max)
    };

    if candidates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no eligible unheld done PRs to assemble into a merge train",
        ));
    }

    let mut accepted_prs: Vec<u64> = Vec::new();
    let mut rebase_tasks: Vec<TodoItem> = Vec::new();
    for pr in &candidates {
        if pr.mergeable.eq_ignore_ascii_case("CONFLICTING") {
            match classify_train_conflict(&pr.files) {
                TrainConflictResolution::TakeLaterPng { .. }
                | TrainConflictResolution::RegenerateGeneratorsJson { .. }
                | TrainConflictResolution::AutoResolvePngAndGenerators { .. } => {
                    accepted_prs.push(pr.number);
                }
                TrainConflictResolution::DropAndQueueRebase { conflicting_files } => {
                    rebase_tasks.push(TodoItem {
                        key: format!("rebase-{}", pr.number),
                        brief: format!(
                            "Rebase PR #{} ({}) onto main to resolve conflicts in {:?}",
                            pr.number, pr.branch, conflicting_files
                        ),
                        area: String::new(),
                        base: Some(pr.branch.clone()),
                        after: Vec::new(),
                        priority: 1,
                        created_at: now,
                        ..Default::default()
                    });
                }
            }
        } else {
            accepted_prs.push(pr.number);
        }
    }

    let mut compile_tasks: Vec<TodoItem> = Vec::new();
    if let Some(ref compile_cmd) = opts.compile_cmd {
        let parent_map = parents(&state.open_prs_cache);
        let (surviving, dropped) = drop_breakers(&accepted_prs, &parent_map, |slice| {
            let prs_csv = slice
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut compile = Command::new("bash");
            compile
                .args(["-c", compile_cmd])
                .current_dir(cwd)
                .env("TACHO_TRAIN_SLICE_PRS", &prs_csv);
            use_private_tmp(&mut compile, "train").map_err(|e| e.to_string())?;
            let out = compile.output().map_err(|e| e.to_string())?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
                .trim()
                .to_string())
            }
        });
        for (bad_pr, reason) in dropped {
            compile_tasks.push(TodoItem {
                key: format!("fix-compile-{bad_pr}"),
                brief: format!("Fix train compile breakage in PR #{bad_pr}: {reason}"),
                area: String::new(),
                base: Some(bad_pr.to_string()),
                after: Vec::new(),
                priority: 1,
                created_at: now,
                ..Default::default()
            });
        }
        accepted_prs = surviving;
    }

    if accepted_prs.is_empty() {
        store.mutate(
            Some((
                "land",
                "train_empty",
                "train",
                "all candidates dropped due to conflict or compile breakage",
            )),
            |st| {
                for task in rebase_tasks.into_iter().chain(compile_tasks) {
                    if !st.todo.iter().any(|t| t.key == task.key) {
                        st.todo.push(task);
                    }
                }
            },
        )?;
        return Err(io::Error::other(
            "all candidate PRs were dropped due to conflicts or compile failures",
        ));
    }

    let mut shas = BTreeMap::new();
    for &pr_num in &accepted_prs {
        let sha = state
            .open_prs_cache
            .iter()
            .find(|p| p.number == pr_num)
            .map(|p| p.head.clone())
            .or_else(|| state.passed.get(&pr_num).map(|r| r.sha.clone()))
            .unwrap_or_else(|| base_sha.clone());
        shas.insert(pr_num, sha);
    }
    let tree_sha = crate::kit::tree_of_worktree(cwd).unwrap_or_else(|_| base_sha.clone());
    let synth_batch = BatchRun {
        id: format!("b{:03}", state.batches.len() + 1),
        group: "train".to_string(),
        prs: accepted_prs.clone(),
        shas,
        main_sha: base_sha.clone(),
        areas: Vec::new(),
        checks: Vec::new(),
        mode: BatchMode::Presubmit,
        status: "passed".to_string(),
        started_at: now,
        finished_at: Some(now),
        tree: Some(tree_sha.clone()),
        pid: None,
        log: String::new(),
        culprit: None,
        skipped: Vec::new(),
        flakes: Vec::new(),
    };
    let train = make_train(&state.trains, &synth_batch, &base_sha, &tree_sha, now)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tid = train.id.clone();
    let because = format!("opened train {tid} for PRs {:?}", train.prs);

    store.mutate(Some(("land", "train_open", &tid, &because)), |st| {
        for task in rebase_tasks.into_iter().chain(compile_tasks) {
            if !st.todo.iter().any(|t| t.key == task.key) {
                st.todo.push(task);
            }
        }
        st.trains = with_train(&st.trains, train.clone());
    })?;

    if opts.run_batch {
        execute_batch_with(BatchOptions {
            prs: train.prs.clone(),
            origin: BatchOrigin::Train,
            foreground: true,
            ..Default::default()
        })?;
    }

    Ok(train)
}

/// Executes `tacho queue`.
pub fn execute_queue(args: QueueArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let current_main = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let ready = queue(
        &state.open_prs_cache,
        &state.passed,
        &state.batches,
        &state.holds,
        &current_main,
        &HashMap::new(),
    );
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ready).unwrap_or_default()
        );
        return Ok(());
    }
    if ready.is_empty() {
        println!("Ready queue is empty.");
        return Ok(());
    }
    println!("{:<8} {:<14} {:<10} TITLE", "PR", "SHA", "BATCH");
    for item in &ready {
        println!(
            "#{:<7} {:<14} {:<10} {}",
            item.pr,
            &item.sha[..item.sha.len().min(12)],
            item.batch_id,
            item.title
        );
    }
    Ok(())
}

/// Executes `tacho train`.
pub fn execute_train(args: TrainArgs) -> io::Result<()> {
    execute_train_with(TrainOptions::from(args))
}

/// Executes `tacho train` with [`TrainOptions`].
pub fn execute_train_with(args: TrainOptions) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    if let Some(ref batch_id) = args.from_batch {
        let Some(mut b) = state.batches.iter().find(|b| &b.id == batch_id).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("batch '{batch_id}' not found"),
            ));
        };
        if let Some(max_n) = args.max {
            b.prs.truncate(max_n);
        }
        let tree = b.tree.clone().unwrap_or_else(|| b.main_sha.clone());
        let train = make_train(&state.trains, &b, &b.main_sha, &tree, now_secs())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let tid = train.id.clone();
        let because = format!(
            "opened train {tid} from batch {batch_id} (PRs {:?})",
            train.prs
        );
        store.mutate(Some(("land", "train_open", &tid, &because)), |st| {
            st.trains = with_train(&st.trains, train);
        })?;
        store.emit_state_band(
            &format!("train:{tid}"),
            "open",
            TaskStatus::InProgress,
            &because,
            now_ns(),
        )?;
        emit_decision_event(&format!("train:{tid}"), "land:train_open", &because);
        println!("{because}");
        return Ok(());
    }

    if !args.prs.is_empty() {
        let cwd = std::env::current_dir()?;
        let train = assemble_train(&mut store, &cwd, &args)?;
        let because = format!("opened train {} for PRs {:?}", train.id, train.prs);
        println!("{because}");
        return Ok(());
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&state.trains).unwrap_or_default()
        );
        return Ok(());
    }

    if let Some(open) = open_for(&state.trains) {
        println!(
            "Open train {}: PRs {:?} on base {} (tree {})",
            open.id,
            open.prs,
            &open.base[..open.base.len().min(12)],
            &open.tree[..open.tree.len().min(12)]
        );
    } else {
        println!("No open merge train.");
    }
    Ok(())
}

/// Options for `tacho release-train`.
#[derive(Debug, Clone, Default)]
pub struct ReleaseTrainOptions {
    pub pr: Option<u64>,
    pub approve: bool,
}

impl From<ReleaseTrainArgs> for ReleaseTrainOptions {
    fn from(args: ReleaseTrainArgs) -> Self {
        Self {
            pr: args.pr,
            approve: args.approve,
        }
    }
}

/// Advances the 4-state notice state machine (`Pending -> Sending -> Sent | Uncertain`) for a
/// release-train head SHA.
///
/// Invariant: Once a notice reaches `Uncertain` (or `Sent`) for a given `head`, it is NEVER
/// retried automatically (`should_send == false`).
pub fn transition_release_notice<F>(
    current: &NoticeState,
    head: &str,
    now: i64,
    send_fn: F,
) -> (NoticeState, bool)
where
    F: FnOnce(&str) -> Result<(), String>,
{
    match current {
        NoticeState::Uncertain {
            head: prev_head, ..
        } if prev_head == head => {
            // Never retry Uncertain for the same head!
            return (current.clone(), false);
        }
        NoticeState::Sent {
            head: prev_head, ..
        } if prev_head == head => {
            return (current.clone(), false);
        }
        NoticeState::Sending {
            head: prev_head, ..
        } if prev_head == head => {
            return (current.clone(), false);
        }
        _ => {}
    }

    let _sending = NoticeState::Sending {
        head: head.to_string(),
        at: now,
    };
    match send_fn(head) {
        Ok(()) => (
            NoticeState::Sent {
                head: head.to_string(),
                at: now,
            },
            true,
        ),
        Err(err) => (
            NoticeState::Uncertain {
                head: head.to_string(),
                at: now,
                error: err,
            },
            true,
        ),
    }
}

/// Executes `tacho release-train`.
pub fn execute_release_train(args: ReleaseTrainArgs) -> io::Result<()> {
    execute_release_train_with(ReleaseTrainOptions::from(args))
}

/// Removes a scratch worktree created during release-train verification, returning an error if cleanup fails.
pub fn cleanup_scratch_worktree(repo_root: &Path, scratch_wt: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(scratch_wt)
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            format!(
                "git worktree remove failed for {}: {e}",
                scratch_wt.display()
            )
        })?;
    if !out.status.success() && scratch_wt.exists() {
        std::fs::remove_dir_all(scratch_wt).map_err(|e| {
            format!(
                "git worktree remove failed ({}) and remove_dir_all failed for {}: {e}",
                String::from_utf8_lossy(&out.stderr).trim(),
                scratch_wt.display()
            )
        })?;
    }
    Ok(())
}

fn verify_train_compilation_in_scratch(
    cwd: &Path,
    initial_main: &str,
    slice: &[u64],
    open_prs_cache: &[PullFacts],
    compile_cmd: &str,
    cleanup_err_slot: &mut Option<String>,
) -> Result<(), String> {
    let prs_str = slice
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
    let scratch_path = scratch.path().join("wt");
    let add_out = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&scratch_path)
        .arg(initial_main)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git worktree add failed: {e}"))?;
    if !add_out.status.success() {
        return Err(format!(
            "Failed to create scratch worktree at {initial_main}: {}",
            String::from_utf8_lossy(&add_out.stderr)
        ));
    }

    for &pr_num in slice {
        let Some(pr) = open_prs_cache.iter().find(|p| p.number == pr_num) else {
            if let Err(ce) = cleanup_scratch_worktree(cwd, &scratch_path) {
                *cleanup_err_slot = Some(ce.clone());
                return Err(ce);
            }
            return Err(format!("PR #{pr_num} not found in open PR cache"));
        };
        let merge_out = match Command::new("git")
            .args(["merge", "--no-ff", "--no-edit", &pr.head])
            .current_dir(&scratch_path)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                if let Err(ce) = cleanup_scratch_worktree(cwd, &scratch_path) {
                    *cleanup_err_slot = Some(ce.clone());
                    return Err(ce);
                }
                return Err(e.to_string());
            }
        };
        if !merge_out.status.success() {
            if let Err(ce) = cleanup_scratch_worktree(cwd, &scratch_path) {
                *cleanup_err_slot = Some(ce.clone());
                return Err(ce);
            }
            return Err(format!(
                "Merge conflict merging PR #{pr_num} ({}) onto train: {}",
                pr.head,
                String::from_utf8_lossy(&merge_out.stderr)
            ));
        }
    }

    let mut compile = Command::new("bash");
    compile
        .args(["-c", compile_cmd])
        .current_dir(&scratch_path)
        .env("TACHO_TRAIN_SLICE_PRS", &prs_str);
    let out = match use_private_tmp(&mut compile, "train").and_then(|()| compile.output()) {
        Ok(o) => o,
        Err(e) => {
            if let Err(ce) = cleanup_scratch_worktree(cwd, &scratch_path) {
                *cleanup_err_slot = Some(ce.clone());
                return Err(ce);
            }
            return Err(e.to_string());
        }
    };

    if let Err(ce) = cleanup_scratch_worktree(cwd, &scratch_path) {
        *cleanup_err_slot = Some(ce.clone());
        return Err(ce);
    }

    if !out.status.success() {
        let err_txt = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(err_txt.trim().to_string());
    }
    Ok(())
}

/// Executes `tacho release-train` with [`ReleaseTrainOptions`].
pub fn execute_release_train_with(args: ReleaseTrainOptions) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    let elig = eligible(
        &state.open_prs_cache,
        &state.holds,
        &state.failed,
        &state.area_rules,
    );
    let ordered_nums = order_train_prs(&elig, &state.open_prs_cache);
    let parent_map = parents(&state.open_prs_cache);
    let cwd = std::env::current_dir()?;
    let initial_main = resolve_live_main_sha(&cwd);
    let compile_cmd = std::env::var("TACHO_COMPILE_CMD")
        .ok()
        .or_else(|| state.train_compile_cmd.clone())
        .unwrap_or_else(|| "cargo check --workspace".to_string());
    if compile_cmd.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "merged-tree compile command cannot be empty before release-train",
        ));
    }
    let mut cleanup_error: Option<String> = None;
    let (surviving, dropped) = drop_breakers(&ordered_nums, &parent_map, |slice| {
        verify_train_compilation_in_scratch(
            &cwd,
            &initial_main,
            slice,
            &state.open_prs_cache,
            &compile_cmd,
            &mut cleanup_error,
        )
    });
    if let Some(ce) = cleanup_error {
        return Err(io::Error::other(ce));
    }

    let mut changed_union = BTreeSet::new();
    let mut member_shas = BTreeMap::new();
    for &n in &surviving {
        if let Some(pr) = state.open_prs_cache.iter().find(|p| p.number == n) {
            member_shas.insert(n, pr.head.clone());
            for f in &pr.files {
                changed_union.insert(f.clone());
            }
        }
    }

    // Verify that neither main nor any surviving member head moved during compilation
    let live_main_after = resolve_live_main_sha(&cwd);
    let live_pr_shas_after = resolve_live_pr_heads(&cwd, &surviving, &member_shas, &state);
    if let Some(moved_reason) = batch_refs_moved(
        &initial_main,
        &live_main_after,
        &member_shas,
        &live_pr_shas_after,
    ) {
        return Err(io::Error::other(format!(
            "Release train refused because refs moved during merged-tree compilation: {moved_reason}"
        )));
    }
    let changed_vec: Vec<String> = changed_union.into_iter().collect();
    let settled_cmds = settle(&changed_vec, &state.generators);
    for cmd in &settled_cmds {
        let mut generator = Command::new("bash");
        generator.args(["-c", cmd]).current_dir(&cwd);
        use_private_tmp(&mut generator, "generators")?;
        let status = generator.status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "generator command '{cmd}' failed with {status}"
            )));
        }
    }

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let tree = crate::kit::tree_of_worktree(&cwd).unwrap_or_else(|_| head.clone());

    let mut tpr = state.train_pr.clone().unwrap_or_else(|| TrainPrState {
        number: args.pr,
        branch: "release/train".to_string(),
        base: "main".to_string(),
        head: head.clone(),
        tree: tree.clone(),
        members: surviving.clone(),
        member_shas: member_shas.clone(),
        dropped: dropped.clone(),
        reviewed: None,
        proved_batch: None,
        proved_tree: None,
    });
    if let Some(num) = args.pr {
        tpr.number = Some(num);
    }
    tpr.head = head.clone();
    tpr.tree = tree;
    tpr.members = surviving;
    tpr.member_shas = member_shas;
    tpr.dropped = dropped;
    if args.approve {
        tpr.reviewed = Some(tpr.head.clone());
    }
    let waiting = waiting_for(
        &tpr,
        &state.batches,
        &state.open_prs_cache,
        &state.area_rules,
        true,
    );

    store.mutate(
        Some((
            "land",
            "release_train",
            &tpr.branch,
            "updated combined train PR state",
        )),
        |st| {
            st.train_pr = Some(tpr.clone());
        },
    )?;

    // Update 4-state notice machine for the release train head SHA
    let now = now_secs();
    let meta = load_fleet_meta().unwrap_or_default();
    let prev_notice = meta
        .train_notices
        .get(&tpr.branch)
        .cloned()
        .unwrap_or(NoticeState::Pending);
    let (next_notice, _) = transition_release_notice(&prev_notice, &head, now, |_| Ok(()));
    mutate_fleet_meta(|m| {
        m.train_notices.insert(tpr.branch.clone(), next_notice);
    })?;

    if !settled_cmds.is_empty() {
        println!("Settled generators: {:?}", settled_cmds);
    }
    println!(
        "Release Train branch '{}' (members {:?}, dropped {:?})",
        tpr.branch, tpr.members, tpr.dropped
    );
    if waiting.is_empty() {
        println!("Status: READY TO LAND");
    } else {
        println!("Waiting for:");
        for w in &waiting {
            println!("  - {w}");
        }
    }
    Ok(())
}
