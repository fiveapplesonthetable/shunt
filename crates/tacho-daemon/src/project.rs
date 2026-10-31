//! Shared project hook execution for CLI and daemon operations.

use std::io;
use std::path::Path;
use std::time::Duration;

use tacho_core::project::{HookCommand, HookKind, HookRequest, ProjectConfig};
use tacho_core::Scrubber;

use crate::FleetState;

/// Applies project-owned fields without changing worker limits, holds or proof records.
pub fn apply(state: &mut FleetState, project: ProjectConfig, root: &Path) {
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
