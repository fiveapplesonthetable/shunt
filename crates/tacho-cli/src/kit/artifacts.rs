//! Collects declared derived files without changing the source a proof identifies.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Serialize)]
struct Entry {
    path: String,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Serialize)]
struct Manifest<'a> {
    version: u32,
    input_tree: &'a str,
    output_tree: &'a str,
    files: Vec<Entry>,
}

fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .arg("-C")
        .arg(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

fn output(command: &mut Command) -> io::Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot read generated output snapshot"));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

fn blob(root: &Path, tree: &str, path: &str) -> io::Result<Option<String>> {
    let entry = output(git(root).args(["ls-tree", "-z", tree, "--", path]))?;
    if entry.is_empty() {
        return Ok(None);
    }
    let metadata = entry
        .split_once('\t')
        .ok_or_else(|| io::Error::other("invalid output tree entry"))?
        .0;
    let mut fields = metadata.split_whitespace();
    let mode = fields.next();
    let kind = fields.next();
    let oid = fields.next();
    if !matches!(mode, Some("100644" | "100755")) || kind != Some("blob") {
        return Err(io::Error::other("generated outputs must be regular files"));
    }
    oid.map(|value| Some(value.to_owned()))
        .ok_or_else(|| io::Error::other("missing output object"))
}

fn copy_blob(root: &Path, oid: &str, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(destination)?;
    let status = git(root)
        .args(["cat-file", "blob", oid])
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other("cannot collect generated output object"));
    }
    Ok(())
}

/// Archives changed declared outputs from immutable Git objects, then restores their input bytes.
pub fn collect(
    root: &Path,
    before: &str,
    after: &str,
    project: &tacho_core::project::ProjectConfig,
    destination: &Path,
) -> io::Result<()> {
    if project.generated_outputs.is_empty() {
        return Ok(());
    }
    let patterns = tacho_core::project::generated_output_matcher(&project.generated_outputs)
        .map_err(io::Error::other)?;
    let mut protected = BTreeSet::new();
    for list in &project.input_lists {
        protected.insert(list.file.clone());
        let reference = format!("{before}:{}", list.file);
        let contents = output(git(root).args(["show", &reference]))?;
        for path in contents
            .lines()
            .map(str::trim)
            .filter(|path| !path.is_empty() && !path.starts_with('#'))
        {
            let path = Path::new(&list.root).join(path);
            if path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(io::Error::other(
                    "listed input path must stay inside its checkout",
                ));
            }
            protected.insert(
                path.to_str()
                    .ok_or_else(|| io::Error::other("listed input path is not UTF-8"))?
                    .to_owned(),
            );
        }
    }
    let changed = output(git(root).args(["diff", "--name-only", "-z", before, after]))?;
    let mut files = Vec::new();
    for path in changed.split('\0').filter(|path| !path.is_empty()) {
        if !patterns.is_match(path) {
            continue;
        }
        if protected.contains(path) {
            continue;
        }
        if Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(io::Error::other(
                "generated output path must stay inside its checkout",
            ));
        }
        let before_oid = blob(root, before, path)?;
        let after_oid = blob(root, after, path)?;
        if let Some(oid) = &before_oid {
            copy_blob(root, oid, &destination.join("before").join(path))?;
        }
        if let Some(oid) = &after_oid {
            copy_blob(root, oid, &destination.join("after").join(path))?;
        }
        files.push(Entry {
            path: path.to_owned(),
            before: before_oid,
            after: after_oid,
        });
    }
    fs::create_dir_all(destination)?;
    let manifest = Manifest {
        version: 1,
        input_tree: before,
        output_tree: after,
        files,
    };
    super::write_atomic(
        &destination.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?,
    )?;
    for entry in &manifest.files {
        if entry.before.is_some() {
            let status = git(root)
                .args([
                    "restore",
                    "--worktree",
                    "--source",
                    before,
                    "--",
                    &entry.path,
                ])
                .status()?;
            if !status.success() {
                return Err(io::Error::other("cannot restore generated output input"));
            }
        } else {
            match fs::remove_file(root.join(&entry.path)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}
