//! Repository configuration applied independently of live queue state.

use std::fs;
use std::io;

use tacho_core::project::ProjectConfig;

use crate::cli::{ProjectArgs, ProjectOperation};

use super::store::{git_stdout, open_fleet_store};

/// Executes project configuration validation, application or inspection.
pub fn execute(args: ProjectArgs) -> io::Result<()> {
    if let ProjectOperation::Select {
        path,
        files_file,
        each,
    } = &args.command
    {
        let project: ProjectConfig =
            serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
        project.validate().map_err(io::Error::other)?;
        let files: Vec<String> =
            serde_json::from_slice(&fs::read(files_file)?).map_err(io::Error::other)?;
        let router = tacho_core::decide::verify::AreaRouter::new(&project.area_rules)
            .map_err(io::Error::other)?;
        let value = if *each {
            serde_json::to_value(
                files
                    .iter()
                    .map(|file| (file, router.select(std::slice::from_ref(file))))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
        } else {
            serde_json::to_value(router.select(&files))
        }
        .map_err(io::Error::other)?;
        println!(
            "{}",
            serde_json::to_string(&value).map_err(io::Error::other)?
        );
        return Ok(());
    }
    if let ProjectOperation::ImportState { path, dry_run } = &args.command {
        let bytes = fs::read(path)?;
        let (_, _, preview) = tacho_daemon::fleet::state_import::validate(&bytes)?;
        let receipt = if *dry_run {
            preview
        } else {
            open_fleet_store()?.import_snapshot(&bytes)?
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).map_err(io::Error::other)?
        );
        return Ok(());
    }
    if let ProjectOperation::Resolve { key } = &args.command {
        open_fleet_store()?.mutate(
            Some((
                "project",
                "resolve",
                key,
                "project decision explicitly resolved",
            )),
            |state| {
                state.external_attention.remove(key);
            },
        )?;
        return Ok(());
    }
    if matches!(args.command, ProjectOperation::Show) {
        let project = open_fleet_store()?.load()?.project;
        println!(
            "{}",
            serde_json::to_string_pretty(&project).map_err(io::Error::other)?
        );
        return Ok(());
    }
    let (path, apply) = match args.command {
        ProjectOperation::Validate { path } => (path, false),
        ProjectOperation::Apply { path } => (path, true),
        ProjectOperation::Show => return Ok(()),
        ProjectOperation::ImportState { .. }
        | ProjectOperation::Resolve { .. }
        | ProjectOperation::Select { .. } => return Ok(()),
    };
    let project: ProjectConfig =
        serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
    project.validate().map_err(io::Error::other)?;
    if apply {
        let cwd = crate::kit::find_worktree_root(&std::env::current_dir()?);
        let root = git_stdout(&cwd, &["rev-parse", "--show-toplevel"])?;
        let root = std::path::Path::new(root.trim());
        open_fleet_store()?.mutate(
            Some((
                "project",
                "apply",
                "configuration",
                "validated project contract replaces project-owned settings",
            )),
            |state| {
                tacho_daemon::project::apply(state, project, root);
            },
        )?;
    }
    println!(
        "Project configuration {}",
        if apply { "applied" } else { "valid" }
    );
    Ok(())
}
