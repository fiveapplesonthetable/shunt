//! Shared project hook execution for CLI and daemon operations.

use std::io;
use std::path::Path;
use std::time::Duration;

use tacho_core::project::{HookCommand, HookKind, HookRequest, ProjectConfig};
use tacho_core::Scrubber;

use crate::FleetState;

/// Applies project policy while preserving queue history and invalidating incompatible active proofs.
pub fn apply(state: &mut FleetState, project: ProjectConfig, root: &Path) {
    if state
        .project
        .as_ref()
        .is_none_or(|previous| previous.verification_identity() != project.verification_identity())
    {
        state.passed.clear();
    }
    if state.project.as_ref() != Some(&project) {
        state.owner_queue_digest = None;
    }
    state.area_rules = project.area_rules.clone();
    state.workflow_path = project.workflow_path.clone();
    state.workflow_jobs = project.workflow_jobs.clone();
    state.workflow_expressions = project.workflow_expressions.clone();
    state.repo_root = Some(root.to_string_lossy().into_owned());
    state.project = Some(project);
}

/// Runs a configured argv with versioned JSON stdin and bounded process lifetime.
pub fn invoke(command: &HookCommand, cwd: &Path, request: &HookRequest) -> io::Result<String> {
    let input = serde_json::to_vec(request).map_err(io::Error::other)?;
    let output = crate::exec::observe::run_argv_with_input(
        Some(cwd),
        &command.argv(),
        Duration::from_secs(command.timeout_secs),
        Some(input),
    )
    .map_err(|error| io::Error::other(Scrubber::enabled().scrub(&error)))?;
    if output.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("project hook response exceeds 8 MiB"));
    }
    Ok(output)
}

/// Validates preparation output before applying it to a worker environment.
pub fn prepared_environment(
    output: &str,
) -> io::Result<std::collections::BTreeMap<String, String>> {
    let prepared: tacho_core::project::PrepareResult = if output.trim().is_empty() {
        Default::default()
    } else {
        serde_json::from_str(output).map_err(io::Error::other)?
    };
    prepared.validate().map_err(io::Error::other)?;
    Ok(prepared.environment)
}

/// The explicit environment override or the project's configured forge repository.
pub fn forge_repository(state: &FleetState) -> Option<String> {
    std::env::var("TACHO_GH_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            state
                .project
                .as_ref()
                .and_then(|project| project.repository.clone())
        })
}

/// Delivers through the explicitly configured project hook in both runtime entry points.
pub fn notify_owner(home: &Path, subject: &str, body: &str) -> Result<bool, String> {
    let state = crate::FleetStore::open(&home.join("events.sqlite"))
        .and_then(|store| store.load())
        .map_err(|error| error.to_string())?;
    let Some(project) = state.project else {
        return tacho_core::decide::plugin::notify_owner_command(subject, body);
    };
    let Some(command) = project.hooks.get(&HookKind::Notify) else {
        return Ok(false);
    };
    let env = std::env::vars().collect();
    let (subject, body) = tacho_core::project::scrub_notification(subject, body, &env);
    let root = state
        .repo_root
        .ok_or("project notification requires its repository root")?;
    let request = HookRequest {
        version: 1,
        kind: HookKind::Notify,
        repository_root: root.clone(),
        worktree: root.clone(),
        base: String::new(),
        head: String::new(),
        tree: String::new(),
        files: Vec::new(),
        areas: Vec::new(),
        payload: serde_json::json!({"subject": subject, "body": body}),
    };
    invoke(command, Path::new(&root), &request).map_err(|error| error.to_string())?;
    Ok(true)
}

/// Delivers typed owner notices with a stable transport key and explicit acknowledgment.
pub fn notify_notice(
    home: &Path,
    notice: &tacho_core::decide::watch::OwnerNotification,
) -> Result<tacho_core::project::DeliveryOutcome, String> {
    use tacho_core::project::DeliveryOutcome;
    let state = crate::FleetStore::open(&home.join("events.sqlite"))
        .and_then(|store| store.load())
        .map_err(|error| error.to_string())?;
    let project = state
        .project
        .ok_or("typed notification requires a project contract")?;
    let Some(command) = project.hooks.get(&HookKind::Notify) else {
        return Ok(DeliveryOutcome::Pending);
    };
    let env = std::env::vars().collect();
    let (subject, body) =
        tacho_core::project::scrub_notification(&notice.subject, &notice.body, &env);
    let root = state
        .repo_root
        .ok_or("project notification requires its repository root")?;
    let request = HookRequest {
        version: 1,
        kind: HookKind::Notify,
        repository_root: root.clone(),
        worktree: root.clone(),
        base: String::new(),
        head: String::new(),
        tree: String::new(),
        files: Vec::new(),
        areas: Vec::new(),
        payload: serde_json::json!({"key":notice.key,"kind":notice.kind,"subject":subject,"body":body,"ready":notice.ready}),
    };
    let output = invoke(command, Path::new(&root), &request).map_err(|error| error.to_string())?;
    if output.trim().is_empty() {
        return Ok(DeliveryOutcome::Sent);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&output) else {
        return Ok(DeliveryOutcome::Sent);
    };
    let Some(delivery) = value.get("delivery") else {
        return Ok(DeliveryOutcome::Sent);
    };
    serde_json::from_value(delivery.clone())
        .map_err(|error| format!("invalid notification acknowledgment: {error}"))
}

/// Publishes a changed queue snapshot before announcing newly verified work.
pub fn publish_queue(
    store: &mut crate::FleetStore,
    plan: &tacho_core::decide::DecisionPlan,
    main: &str,
) -> std::io::Result<()> {
    let state = store.load()?;
    let Some(project) = &state.project else {
        return Ok(());
    };
    let Some(command) = project.hooks.get(&HookKind::Publish) else {
        return Ok(());
    };
    let root = state
        .repo_root
        .as_deref()
        .ok_or_else(|| std::io::Error::other("queue publication requires its repository root"))?;
    let mut ready = plan.ready_trains.clone();
    for notice in &mut ready {
        notice.announce = false;
    }
    let payload = serde_json::json!({"main":main,"ready":ready,"attention":plan.attention_items});
    let encoded = serde_json::to_vec(&payload).map_err(std::io::Error::other)?;
    let digest = tacho_core::project::content_id(&encoded);
    if state.owner_queue_digest.as_deref() == Some(digest.as_str()) {
        return Ok(());
    }
    let request = HookRequest {
        version: 1,
        kind: HookKind::Publish,
        repository_root: root.into(),
        worktree: root.into(),
        base: main.into(),
        head: String::new(),
        tree: String::new(),
        files: Vec::new(),
        areas: Vec::new(),
        payload,
    };
    invoke(command, Path::new(root), &request)?;
    store.mutate(None, |state| state.owner_queue_digest = Some(digest))?;
    Ok(())
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use tacho_core::decide::verify::{BatchRun, PassedRecord};

    fn project() -> ProjectConfig {
        serde_json::from_value(serde_json::json!({
            "version":1,"repository":"owner/repository",
            "area_rules":[{"area":"runtime","prefixes":["src/"]}],
            "hooks":{"verify":{"program":"verify-v1"},"runner_trust":{"program":"trust"}}
        }))
        .unwrap()
    }

    fn proven() -> FleetState {
        let mut state = FleetState::default();
        apply(&mut state, project(), Path::new("/project"));
        state.passed.insert(
            7,
            PassedRecord {
                pr: 7,
                sha: "a".repeat(40),
                ..Default::default()
            },
        );
        state.batches.push(BatchRun {
            id: "b001".into(),
            status: "passed".into(),
            ..Default::default()
        });
        state
    }

    #[test]
    fn transport_changes_keep_active_verification_and_reset_queue_publication() {
        let mut state = proven();
        state.owner_queue_digest = Some("previous".into());
        let mut changed = project();
        changed.hooks.insert(
            HookKind::Notify,
            HookCommand {
                program: "notify-v2".into(),
                args: Vec::new(),
                timeout_secs: 60,
            },
        );
        changed.hooks.insert(
            HookKind::Publish,
            HookCommand {
                program: "publish-v2".into(),
                args: Vec::new(),
                timeout_secs: 60,
            },
        );
        apply(&mut state, changed, Path::new("/project"));
        assert!(state.passed.contains_key(&7));
        assert!(state.owner_queue_digest.is_none());
        assert_eq!(state.batches.len(), 1);
    }

    #[test]
    fn changed_verification_policy_invalidates_active_passes_but_keeps_history() {
        for change in ["repository", "verify", "areas"] {
            let mut state = proven();
            let workers = state.max_workers;
            let mut changed = project();
            match change {
                "repository" => changed.repository = Some("owner/another".into()),
                "verify" => {
                    changed.hooks.get_mut(&HookKind::Verify).unwrap().program = "verify-v2".into()
                }
                _ => changed.area_rules.clear(),
            }
            apply(&mut state, changed, Path::new("/project"));
            assert!(state.passed.is_empty(), "{change}");
            assert_eq!(state.batches.len(), 1);
            assert_eq!(state.max_workers, workers);
        }
    }

    #[test]
    fn adopting_a_contract_does_not_certify_unbound_legacy_passes() {
        let mut state = proven();
        state.project = None;
        apply(&mut state, project(), Path::new("/project"));
        assert!(state.passed.is_empty());
        assert_eq!(state.batches.len(), 1);
    }
}
