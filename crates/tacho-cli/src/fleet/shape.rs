//! PR branch shape/replay (`tacho replay <pr> [--dry-run]` cherry-picking onto base +
//! `--force-with-lease` or marking `needs-hands` with conflicting files, preserving state on
//! GitHub `UNKNOWN`) and decision/batch replay (`tick:<id>`, `batch-<id>`, `decide-replay`).

use std::io;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::decide;
use tacho_core::decide::dispatch::{PullFacts, TodoItem};
use tacho_daemon::FleetStore;

use crate::cli::{BatchArgs, ReplayArgs};

use super::batch::execute_batch;
use super::store::{git_stdout, now_secs, open_fleet_store};

/// Parsed target of `tacho replay <target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayTarget {
    /// PR branch shape/replay (`<pr>` or `pr:<pr>`).
    Pr(u64),
    /// Decision tick replay (`tick:<id>`, `tick`, `latest`, `decide-replay`).
    Decision(Option<i64>),
    /// Batch verification replay (`b001`, `batch-b001`).
    Batch(String),
}

impl ReplayTarget {
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim();
        if s.eq_ignore_ascii_case("latest")
            || s.eq_ignore_ascii_case("tick")
            || s.eq_ignore_ascii_case("decide")
            || s.eq_ignore_ascii_case("decide-replay")
        {
            return Self::Decision(None);
        }
        if let Some(rest) = s.strip_prefix("tick:") {
            return Self::Decision(rest.trim().parse::<i64>().ok());
        }
        if let Some(rest) = s.strip_prefix("pr:").or_else(|| s.strip_prefix('#')) {
            if let Ok(pr) = rest.trim().parse::<u64>() {
                return Self::Pr(pr);
            }
        }
        if let Some(rest) = s
            .strip_prefix("batch-")
            .or_else(|| s.strip_prefix("batch:"))
        {
            let norm = if rest.starts_with('b') {
                rest.to_string()
            } else {
                format!("b{rest}")
            };
            return Self::Batch(norm);
        }
        if let Ok(pr) = s.parse::<u64>() {
            return Self::Pr(pr);
        }
        Self::Batch(s.to_string())
    }
}

/// Outcome of shaping/replaying a PR branch onto its base (`replay_pr_branch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrReplayOutcome {
    /// GitHub `mergeable` is `"UNKNOWN"`: state is preserved untouched without mutating refs or holds.
    PreservedUnknown { pr: u64 },
    /// Cherry-picked cleanly onto `base` and pushed with `--force-with-lease`.
    Replayed {
        pr: u64,
        new_head: String,
        dry_run: bool,
    },
    /// Conflicted during cherry-pick: marked `needs-hands` with the list of conflicting files.
    NeedsHands {
        pr: u64,
        conflicting_files: Vec<String>,
    },
}

/// Replays a PR's commits onto its target base branch (`main` or stacked base) in a temporary
/// detached worktree, pushing with `--force-with-lease` on success, or recording `needs-hands`
/// with conflicting files on conflict.
///
/// If `pr.mergeable == "UNKNOWN"`, preserves state without modifying anything.
pub fn replay_pr_branch(
    store: &mut FleetStore,
    repo_dir: &Path,
    pr: &PullFacts,
    dry_run: bool,
) -> io::Result<PrReplayOutcome> {
    if pr.mergeable.eq_ignore_ascii_case("UNKNOWN") {
        println!(
            "PR #{} mergeable state is UNKNOWN; preserving state without replay.",
            pr.number
        );
        return Ok(PrReplayOutcome::PreservedUnknown { pr: pr.number });
    }

    let base_ref = if pr.base.trim().is_empty() {
        "main"
    } else {
        pr.base.trim()
    };
    let base_sha = git_stdout(repo_dir, &["rev-parse", base_ref]).unwrap_or_else(|_| {
        git_stdout(repo_dir, &["rev-parse", "HEAD"]).unwrap_or_else(|_| "HEAD".to_string())
    });
    let head_sha = if !pr.head.is_empty() && git_stdout(repo_dir, &["rev-parse", &pr.head]).is_ok()
    {
        pr.head.clone()
    } else if !pr.branch.is_empty() {
        git_stdout(repo_dir, &["rev-parse", &pr.branch]).unwrap_or_else(|_| base_sha.clone())
    } else {
        base_sha.clone()
    };

    let scratch = tempfile::tempdir()?;
    let scratch_path = scratch.path();
    let add_out = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(scratch_path)
        .arg(&base_sha)
        .current_dir(repo_dir)
        .output()?;
    if !add_out.status.success() {
        return Err(io::Error::other(format!(
            "failed to create replay worktree at {base_sha}: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        )));
    }

    let commits_raw = git_stdout(
        scratch_path,
        &["rev-list", "--reverse", &format!("{base_sha}..{head_sha}")],
    )
    .unwrap_or_default();
    let commits: Vec<&str> = commits_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let mut conflicting_files: Vec<String> = Vec::new();
    for sha in &commits {
        let cp = Command::new("git")
            .args(["cherry-pick", sha])
            .current_dir(scratch_path)
            .output()?;
        if !cp.status.success() {
            let diff_out = Command::new("git")
                .args(["diff", "--name-only", "--diff-filter=U"])
                .current_dir(scratch_path)
                .output()?;
            for line in String::from_utf8_lossy(&diff_out.stdout).lines() {
                let f = line.trim();
                if !f.is_empty() && !conflicting_files.contains(&f.to_string()) {
                    conflicting_files.push(f.to_string());
                }
            }
            if conflicting_files.is_empty() {
                conflicting_files = pr.files.clone();
            }
            let _ = Command::new("git")
                .args(["cherry-pick", "--abort"])
                .current_dir(scratch_path)
                .output();
            break;
        }
    }

    let new_head =
        git_stdout(scratch_path, &["rev-parse", "HEAD"]).unwrap_or_else(|_| head_sha.clone());
    if let Err(e) = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(scratch_path)
        .current_dir(repo_dir)
        .output()
    {
        eprintln!(
            "[tacho-shape] warning: failed to remove scratch worktree {}: {e}",
            scratch_path.display()
        );
    }

    if !conflicting_files.is_empty() {
        let now = now_secs();
        let reb_key = format!("rebase-{}", pr.number);
        let because = format!(
            "PR #{} needs-hands rebase onto {}: conflicting files {:?}",
            pr.number, base_ref, conflicting_files
        );
        if !dry_run {
            store.mutate(
                Some(("land", "replay_needs_hands", &reb_key, &because)),
                |st| {
                    st.rebase_attempts.insert(pr.number, head_sha.clone());
                    if !st.todo.iter().any(|t| t.key == reb_key) {
                        st.todo.push(TodoItem {
                            key: reb_key.clone(),
                            brief: format!(
                                "Rebase PR #{} ({}) onto {} (conflicting files: {:?})",
                                pr.number, pr.branch, base_ref, conflicting_files
                            ),
                            area: String::new(),
                            base: Some(pr.branch.clone()),
                            after: Vec::new(),
                            priority: 1,
                            created_at: now,
                            ..Default::default()
                        });
                    }
                },
            )?;
        }
        println!("{because}");
        return Ok(PrReplayOutcome::NeedsHands {
            pr: pr.number,
            conflicting_files,
        });
    }

    if !dry_run && !pr.branch.is_empty() {
        let has_origin = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(repo_dir)
            .output()
            .is_ok_and(|o| o.status.success());
        if has_origin {
            let expected_head = pr.head.trim();
            if expected_head.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "refusing force-with-lease push for PR #{} ({}): expected remote head SHA is empty",
                        pr.number, pr.branch
                    ),
                ));
            }
            let lease = format!("--force-with-lease={}:{expected_head}", pr.branch);
            let push_out = Command::new("git")
                .args([
                    "push",
                    "-q",
                    &lease,
                    "origin",
                    &format!("{new_head}:refs/heads/{}", pr.branch),
                ])
                .current_dir(repo_dir)
                .output()?;
            if !push_out.status.success() {
                return Err(io::Error::other(format!(
                    "git push {lease} origin {new_head}:refs/heads/{} failed: {}",
                    pr.branch,
                    String::from_utf8_lossy(&push_out.stderr).trim()
                )));
            }
        }
        let br_out = Command::new("git")
            .args(["branch", "-f", &pr.branch, &new_head])
            .current_dir(repo_dir)
            .output()?;
        if !br_out.status.success() {
            return Err(io::Error::other(format!(
                "git branch -f {} {} failed: {}",
                pr.branch,
                new_head,
                String::from_utf8_lossy(&br_out.stderr).trim()
            )));
        }
        let because = format!(
            "replayed PR #{} ({}) onto {} -> {}",
            pr.number,
            pr.branch,
            base_ref,
            &new_head[..new_head.len().min(12)]
        );
        store.mutate(
            Some(("land", "replay_pr", &pr.number.to_string(), &because)),
            |st| {
                st.rebase_attempts.insert(pr.number, new_head.clone());
                for p in &mut st.open_prs_cache {
                    if p.number == pr.number {
                        p.head = new_head.clone();
                        p.mergeable = "MERGEABLE".to_string();
                    }
                }
            },
        )?;
    }

    println!(
        "{}Replayed PR #{} onto {} -> {}",
        if dry_run { "[dry-run] " } else { "" },
        pr.number,
        base_ref,
        &new_head[..new_head.len().min(12)]
    );
    Ok(PrReplayOutcome::Replayed {
        pr: pr.number,
        new_head,
        dry_run,
    })
}

/// Replays a recorded decision tick from `FleetStore` and verifies 1:1 determinism.
pub fn replay_decision(store: &FleetStore, tick_id: Option<i64>) -> io::Result<()> {
    let tick_data = match tick_id {
        Some(id) => store.load_decision_tick(id)?,
        None => {
            let Some((_, s, o, p)) = store.load_latest_decision_tick()? else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no recorded decision ticks found",
                ));
            };
            (s, o, p)
        }
    };
    let (logged_state, logged_obs, logged_plan) = tick_data;
    println!("Replaying pure decision evaluation from recorded tick inputs...");
    let replayed_plan = decide(&logged_state, &logged_obs);
    if replayed_plan != logged_plan {
        return Err(io::Error::other(format!(
            "Replayed decisions diverged from recorded decision plan!\nExpected: {:?}\nReplayed: {:?}",
            logged_plan, replayed_plan
        )));
    }
    println!("Deterministic decision replay VERIFIED: exact 1:1 match with recorded plan.");
    println!(
        "Dispatched: {} tasks, Resumed: {} tasks, Reaped: {} agents, Batch resolutions: {}",
        replayed_plan.dispatched_tasks.len(),
        replayed_plan.resumed_tasks.len(),
        replayed_plan.reaped_agents.len(),
        replayed_plan.batch_resolutions.len(),
    );
    Ok(())
}

/// Executes `tacho replay <batch-id|tick:<id>|pr:<n>>`.
pub fn execute_replay(args: ReplayArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    let norm_batch_id = args
        .batch_id
        .strip_prefix("batch-")
        .or_else(|| args.batch_id.strip_prefix("batch:"))
        .map(|r| {
            if r.starts_with('b') {
                r.to_string()
            } else {
                format!("b{r}")
            }
        })
        .unwrap_or_else(|| args.batch_id.clone());

    // 1. Check if args.batch_id refers to a recorded batch
    if let Some(b) = state
        .batches
        .iter()
        .find(|b| b.id == args.batch_id || b.id == norm_batch_id)
    {
        println!(
            "Replaying batch {} (PRs {:?} on main {})",
            b.id, b.prs, b.main_sha
        );
        if !b.log.is_empty() {
            println!("--- Recorded Log ---\n{}", b.log);
        }
        return execute_batch(BatchArgs {
            prs: b.prs.clone(),
            cmd: args.cmd,
            mode: None,
            timeout: None,
            dry_run: args.dry_run,
        });
    }

    // 2. Check if args.batch_id refers to a PR (`pr:<n>` or open PR number)
    let pr_num_opt = args
        .batch_id
        .strip_prefix("pr:")
        .or_else(|| args.batch_id.strip_prefix('#'))
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            let n = args.batch_id.parse::<u64>().ok()?;
            if state.open_prs_cache.iter().any(|p| p.number == n) {
                Some(n)
            } else {
                None
            }
        });
    if let Some(pr_num) = pr_num_opt {
        let cwd = std::env::current_dir()?;
        let live_pr = std::env::var("TACHO_GH_REPO")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(tacho_core::decide::plugin::GitHubForge::new)
            .and_then(|forge| {
                use tacho_core::decide::plugin::ForgePlugin;
                let mut runner = |argv: &[String]| {
                    tacho_daemon::exec::observe::run_argv(
                        Some(&cwd),
                        argv,
                        std::time::Duration::from_secs(15),
                    )
                };
                forge.open_prs(&mut runner).ok()
            })
            .and_then(|prs| prs.into_iter().find(|p| p.number == pr_num));
        let mut pr = live_pr
            .or_else(|| {
                state
                    .open_prs_cache
                    .iter()
                    .find(|p| p.number == pr_num)
                    .cloned()
            })
            .unwrap_or_else(|| PullFacts {
                number: pr_num,
                branch: format!("pr-{pr_num}"),
                base: "main".to_string(),
                state: "OPEN".to_string(),
                mergeable: "MERGEABLE".to_string(),
                ..Default::default()
            });
        // Observe the remote branch head at replay time when origin is configured so
        // `--force-with-lease=<branch>:<observed_head>` uses a live observation rather than
        // an empty or stale cached SHA.
        if !pr.branch.is_empty() {
            let refspec = format!("refs/heads/{}", pr.branch);
            if let Ok(ls_out) = Command::new("git")
                .args(["ls-remote", "origin", &refspec])
                .current_dir(&cwd)
                .output()
            {
                if ls_out.status.success() {
                    if let Some(observed_sha) = String::from_utf8_lossy(&ls_out.stdout)
                        .split_whitespace()
                        .next()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        pr.head = observed_sha.to_string();
                        let _ = Command::new("git")
                            .args(["fetch", "-q", "origin", &refspec])
                            .current_dir(&cwd)
                            .status();
                    }
                }
            }
            if pr.head.is_empty() {
                if let Ok(local_sha) = git_stdout(&cwd, &["rev-parse", &pr.branch]) {
                    pr.head = local_sha;
                }
            }
        }
        let _ = replay_pr_branch(&mut store, &cwd, &pr, args.dry_run)?;
        return Ok(());
    }

    // 3. Check if args.batch_id refers to a decision tick (e.g. "tick:<id>", numeric tick id, or "latest" / "tick")
    let is_tick_request =
        args.batch_id.starts_with("tick:") || args.batch_id == "tick" || args.batch_id == "latest";

    let tick_data = if is_tick_request {
        if let Some(rest) = args.batch_id.strip_prefix("tick:") {
            if let Ok(ts) = rest.parse::<i64>() {
                store.load_decision_tick(ts).ok()
            } else {
                None
            }
        } else {
            store
                .load_latest_decision_tick()
                .ok()
                .flatten()
                .map(|(_, s, o, p)| (s, o, p))
        }
    } else if let Ok(ts) = args.batch_id.parse::<i64>() {
        store.load_decision_tick(ts).ok()
    } else {
        None
    };

    if let Some((logged_state, logged_obs, logged_plan)) = tick_data {
        println!("Replaying pure decision evaluation from recorded tick inputs...");
        let replayed_plan = decide(&logged_state, &logged_obs);
        if replayed_plan != logged_plan {
            return Err(io::Error::other(format!(
                "Replayed decisions diverged from recorded decision plan!\nExpected: {:?}\nReplayed: {:?}",
                logged_plan, replayed_plan
            )));
        }
        println!("Deterministic decision replay VERIFIED: exact 1:1 match with recorded plan.");
        println!(
            "Dispatched: {} tasks, Resumed: {} tasks, Reaped: {} agents, Batch resolutions: {}",
            replayed_plan.dispatched_tasks.len(),
            replayed_plan.resumed_tasks.len(),
            replayed_plan.reaped_agents.len(),
            replayed_plan.batch_resolutions.len(),
        );
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("batch, PR, or decision tick '{}' not found", args.batch_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_pr_branch_force_with_lease_rejects_stale_remote_and_preserves_store() {
        let dir = tempfile::tempdir().unwrap();
        let origin_dir = dir.path().join("origin.git");
        let repo_dir = dir.path().join("repo");
        let other_dir = dir.path().join("other");
        std::fs::create_dir_all(&origin_dir).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();

        let run = |cwd: &Path, args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} in {} failed", cwd.display());
        };
        run(&origin_dir, &["init", "--bare", "-q", "-b", "main"]);
        run(&repo_dir, &["init", "-q", "-b", "main"]);
        run(&repo_dir, &["config", "user.email", "t@example.com"]);
        run(&repo_dir, &["config", "user.name", "Tester"]);
        run(
            &repo_dir,
            &["remote", "add", "origin", origin_dir.to_str().unwrap()],
        );

        std::fs::write(repo_dir.join("base.txt"), "v1\n").unwrap();
        run(&repo_dir, &["add", "base.txt"]);
        run(&repo_dir, &["commit", "-q", "-m", "init"]);
        run(&repo_dir, &["push", "-q", "-u", "origin", "main"]);

        // Create PR branch `feat/lease`
        run(&repo_dir, &["checkout", "-q", "-b", "feat/lease"]);
        std::fs::write(repo_dir.join("feat.txt"), "feature v1\n").unwrap();
        run(&repo_dir, &["add", "feat.txt"]);
        run(&repo_dir, &["commit", "-q", "-m", "feat 1"]);
        let old_pr_head = git_stdout(&repo_dir, &["rev-parse", "HEAD"]).unwrap();
        run(&repo_dir, &["push", "-q", "-u", "origin", "feat/lease"]);

        // Advance `main` cleanly
        run(&repo_dir, &["checkout", "-q", "main"]);
        std::fs::write(repo_dir.join("base.txt"), "v2\n").unwrap();
        run(&repo_dir, &["add", "base.txt"]);
        run(&repo_dir, &["commit", "-q", "-m", "advance main"]);
        run(&repo_dir, &["push", "-q", "origin", "main"]);

        // Clone into `other_dir` and advance `origin/feat/lease` behind `repo_dir`'s back!
        run(
            dir.path(),
            &[
                "clone",
                "-q",
                origin_dir.to_str().unwrap(),
                other_dir.to_str().unwrap(),
            ],
        );
        run(&other_dir, &["config", "user.email", "o@example.com"]);
        run(&other_dir, &["config", "user.name", "Other"]);
        run(
            &other_dir,
            &["checkout", "-q", "-b", "feat/lease", "origin/feat/lease"],
        );
        std::fs::write(other_dir.join("feat.txt"), "concurrent push\n").unwrap();
        run(&other_dir, &["add", "feat.txt"]);
        run(&other_dir, &["commit", "-q", "-m", "concurrent commit"]);
        run(&other_dir, &["push", "-q", "origin", "feat/lease"]);

        let mut store = FleetStore::open(&dir.path().join("events.sqlite")).unwrap();
        let pr = PullFacts {
            number: 77,
            title: "Feature lease".to_string(),
            branch: "feat/lease".to_string(),
            base: "main".to_string(),
            head: old_pr_head.clone(),
            state: "OPEN".to_string(),
            mergeable: "CONFLICTING".to_string(),
            files: vec!["feat.txt".to_string()],
            ..Default::default()
        };
        store
            .mutate(None, |st| {
                st.open_prs_cache.push(pr.clone());
            })
            .unwrap();

        let res = replay_pr_branch(&mut store, &repo_dir, &pr, false);
        assert!(
            res.is_err(),
            "replay_pr_branch must fail when --force-with-lease detects moved remote branch"
        );
        let st = store.load().unwrap();
        assert!(
            !st.rebase_attempts.contains_key(&77),
            "store.rebase_attempts must not be mutated when push fails"
        );
        assert_eq!(
            st.open_prs_cache[0].head, old_pr_head,
            "store.open_prs_cache head must remain untouched when push fails"
        );
        assert_eq!(st.open_prs_cache[0].mergeable, "CONFLICTING");
    }
}
