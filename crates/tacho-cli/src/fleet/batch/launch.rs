use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use tacho_core::decide::verify::{new_batch_id, BatchMode, BatchPlan, BatchRun};
use tacho_daemon::FleetStore;

use super::super::store::{now_secs, record_batch_pid_identity, resolve_live_main_sha};

struct PendingChild(Option<Child>);

impl Drop for PendingChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Starts a planned batch only after recording its exact inputs and child identity.
pub fn spawn_planned_batch(
    store: &mut FleetStore,
    plan: &BatchPlan,
    main_sha: &str,
    repo: &Path,
) -> io::Result<Option<u32>> {
    if plan.prs.is_empty() || plan.areas.is_empty() || main_sha.is_empty() {
        return Err(io::Error::other("batch plan lacks verification inputs"));
    }
    if resolve_live_main_sha(repo) != main_sha {
        return Err(io::Error::other("main moved after batch planning"));
    }
    let logs = store.home().join("logs");
    fs::create_dir_all(&logs)?;
    let log = tempfile::Builder::new()
        .prefix("batch-")
        .suffix(".log")
        .tempfile_in(&logs)?;
    let (file, path) = log.keep().map_err(|error| error.error)?;
    let mut command = Command::new(crate::client::resolve_tacho_bin());
    command.args([
        "batch",
        "--foreground",
        "--areas",
        &plan.areas.join(","),
        "--mode",
        match plan.mode {
            BatchMode::Presubmit => "presubmit",
            BatchMode::Postsubmit => "postsubmit",
        },
    ]);
    command.args(plan.prs.iter().map(u64::to_string));
    command
        .current_dir(repo)
        .env("TACHO_HOME", store.home())
        .env("TACHO_BATCH_CLAIMED", "1")
        .env("TACHO_BATCH_ORIGIN", "daemon")
        .stdin(Stdio::piped())
        .stderr(file.try_clone()?)
        .stdout(file)
        .process_group(0);
    crate::kit::use_private_tmp(&mut command, "planned-batch")?;
    let mut pending = PendingChild(Some(command.spawn()?));
    let child = pending.0.as_mut().unwrap();
    let pid = child.id();
    let id = store.mutate(None, |state| -> io::Result<Option<String>> {
        if state
            .main_sha
            .as_deref()
            .is_some_and(|main| main != main_sha)
            || plan.shas.len() != plan.prs.len()
            || plan.prs.iter().any(|number| {
                !state
                    .open_prs_cache
                    .iter()
                    .any(|pull| pull.number == *number && plan.shas.get(number) == Some(&pull.head))
            })
        {
            return Err(io::Error::other(
                "batch observation changed before its claim",
            ));
        }
        if state.batches.iter().any(|batch| {
            matches!(batch.status.as_str(), "running" | "bisecting")
                && (batch.group == plan.group
                    || batch.prs.iter().any(|number| plan.prs.contains(number)))
        }) {
            return Ok(None);
        }
        let fresh = tacho_core::decide::verify::batches_with_grouping(
            &state.open_prs_cache,
            &state.holds,
            &state.passed,
            &state.failed,
            &state.batches,
            &state.area_rules,
            tacho_core::decide::verify::MAX_BATCH_SIZE,
            state
                .project
                .as_ref()
                .and_then(|project| project.batch_grouping.as_ref()),
        );
        if !fresh.iter().any(|batch| {
            batch.group == plan.group
                && batch.prs == plan.prs
                && batch.shas == plan.shas
                && batch.areas == plan.areas
                && batch.checks == plan.checks
                && batch.mode == plan.mode
        }) {
            return Ok(None);
        }
        let id = new_batch_id(&state.batches);
        // The child is blocked on stdin until this record and its PID identity are durable.
        record_batch_pid_identity(&id, pid)?;
        state.batches.push(BatchRun {
            id: id.clone(),
            group: plan.group.clone(),
            prs: plan.prs.clone(),
            shas: plan.shas.clone(),
            main_sha: main_sha.to_string(),
            areas: plan.areas.clone(),
            checks: plan.checks.clone(),
            mode: plan.mode,
            status: "running".to_string(),
            started_at: now_secs(),
            pid: Some(pid),
            log: path.to_string_lossy().to_string(),
            ..Default::default()
        });
        Ok(Some(id))
    })??;
    let Some(id) = id else {
        return Ok(None);
    };
    if let Err(error) = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("batch handshake is unavailable"))
        .and_then(|mut input| input.write_all(id.as_bytes()))
    {
        store.mutate(None, |state| {
            if let Some(batch) = state.batches.iter_mut().find(|batch| batch.id == id) {
                batch.status = "infrastructure".to_string();
                batch.finished_at = Some(now_secs());
                batch.pid = None;
                batch.log = error.to_string();
            }
        })?;
        return Err(error);
    }
    let mut child = pending.0.take().unwrap();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(Some(pid))
}

pub(super) fn receive_claim() -> io::Result<Option<String>> {
    if std::env::var("TACHO_BATCH_CLAIMED").ok().as_deref() != Some("1") {
        return Ok(None);
    }
    let mut id = String::new();
    io::stdin().take(128).read_to_string(&mut id)?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::other("batch claim handshake was not completed"));
    }
    Ok(Some(id))
}
