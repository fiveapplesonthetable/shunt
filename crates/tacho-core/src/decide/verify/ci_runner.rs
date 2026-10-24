//! Container & host CI step compilation (R34, R37, R38): GitHub Actions YAML subset
//! translation, service container argv, job container argv with root `chown` handback
//! and process-group signal trap, and host-job `systemd-run --user --scope` + `taskset`.

use super::ci_refs::{CiJob, CiService};
use super::config::CiRunnerConfig;
use super::stats::cpu_list;
use super::worktree::sh_quote;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// R37: compile the `docker run` argv for a service container (`postgres`, etc.).
pub fn native_service_argv(
    service: &CiService,
    container_name: &str,
    network: &str,
) -> Vec<String> {
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "--network".to_string(),
        network.to_string(),
        "--network-alias".to_string(),
        service.name.clone(),
    ];
    for (k, v) in &service.env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }
    for opt in service.options.split_whitespace() {
        argv.push(opt.to_string());
    }
    argv.push(service.image.clone());
    argv
}

/// R37: compile the shell script executed inside the job container or host runner:
/// - Initializes `$GITHUB_PATH` and `$GITHUB_ENV` tempfiles.
/// - Traps `TERM` and `INT` to kill the whole process group (`kill -TERM -$$`).
/// - Runs each `run` step in a subshell sourcing `$GITHUB_ENV` and prepending `$GITHUB_PATH`.
/// - On `EXIT`, when running inside a container as root (`host_uid_gid.is_some()`), hands back
///   ownership (`chown -R uid:gid`) of `/w` plus writable cache mounts and `/target/<area>`
///   (never touching `:ro` mounts or sibling areas' `/target/*` directories).
pub fn compile_job_script(
    job: &CiJob,
    area: &str,
    writable_mounts: &[String],
    host_uid_gid: Option<(u32, u32)>,
) -> String {
    let mut lines = vec![
        "set -eo pipefail".to_string(),
        "export GITHUB_PATH=$(mktemp)".to_string(),
        "export GITHUB_ENV=$(mktemp)".to_string(),
    ];

    let mut cleanup_parts = vec!["rm -f \"$GITHUB_PATH\" \"$GITHUB_ENV\"".to_string()];
    if let Some((uid, gid)) = host_uid_gid {
        let mut chown_paths = vec!["/w".to_string()];
        for m in writable_mounts {
            if m == "/target" {
                chown_paths.push(format!("/target/{area}"));
            } else {
                chown_paths.push(m.clone());
            }
        }
        cleanup_parts.push(format!(
            "chown -R {uid}:{gid} {} 2>/dev/null || true",
            chown_paths.join(" ")
        ));
    }
    lines.push(format!("_cleanup() {{ {}; }}", cleanup_parts.join("; ")));
    lines.push("trap '_cleanup' EXIT".to_string());
    lines.push("trap 'kill -TERM -$$ 2>/dev/null || true; exit 143' TERM INT".to_string());

    for (idx, step) in job.steps.iter().enumerate() {
        let step_tag = format!("__TACHO_STEP_{idx}__");
        lines.push(format!(
            "echo {} >&2",
            sh_quote(&format!("=== step {}: {} ===", idx + 1, step.name))
        ));
        lines.push("(".to_string());
        lines.push(
            "  while IFS= read -r _p; do [ -n \"$_p\" ] && export PATH=\"$_p:$PATH\"; done < \"$GITHUB_PATH\""
                .to_string(),
        );
        lines.push("  set -a; . \"$GITHUB_ENV\"; set +a".to_string());
        if let Some(wd) = &step.working_directory {
            lines.push(format!("  cd {}", sh_quote(wd)));
        }
        for (k, v) in &step.env {
            lines.push(format!("  export {k}={}", sh_quote(v)));
        }
        lines.push(format!("  bash -eo pipefail <<'{step_tag}'"));
        lines.push(step.run.clone());
        lines.push(step_tag);
        lines.push(")".to_string());
    }

    lines.join("\n")
}

/// R37: compile the `docker run` argv for a containerized CI job.
///
/// - Runs as `--user 0:0` inside the container so package installs work, and hands back ownership
///   of `/w`, writable cache mounts, and `/target/<area>` to `uid:gid` on exit.
/// - Mounts read-only cache directories with `:ro` and excludes them from `chown`.
/// - Pins CPUs with `--cpuset-cpus`, memory with `--memory`, and job parallelism with
///   `CARGO_BUILD_JOBS`, `CMAKE_BUILD_PARALLEL_LEVEL`, `MAKEFLAGS`, and `VITEST_MAX_THREADS`.
#[allow(clippy::too_many_arguments)]
pub fn native_job_argv(
    job: &CiJob,
    area: &str,
    container_name: &str,
    network: &str,
    worktree: &str,
    mounts: &[(&str, &str, bool)],
    cpus: &[usize],
    jobs: usize,
    uid: u32,
    gid: u32,
    cfg: &CiRunnerConfig,
) -> Vec<String> {
    let cpuset = cpu_list(cpus);
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--init".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "--network".to_string(),
        network.to_string(),
        "--user".to_string(),
        "0:0".to_string(),
        "--cpuset-cpus".to_string(),
        cpuset,
        "--memory".to_string(),
        cfg.container_memory.clone(),
        "--memory-swap".to_string(),
        cfg.container_memory_swap.clone(),
        "--pids-limit".to_string(),
        cfg.container_pids_limit.to_string(),
        "-v".to_string(),
        format!("{worktree}:/w"),
        "-w".to_string(),
        "/w".to_string(),
    ];

    let mut writable_mounts = Vec::new();
    for &(host_path, container_path, read_only) in mounts {
        argv.push("-v".to_string());
        if read_only {
            argv.push(format!("{host_path}:{container_path}:ro"));
        } else {
            argv.push(format!("{host_path}:{container_path}"));
            writable_mounts.push(container_path.to_string());
        }
    }

    let mut env = job.env.clone();
    env.insert("CI".to_string(), "true".to_string());
    env.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
    env.insert("GITHUB_WORKSPACE".to_string(), "/w".to_string());
    env.insert("CARGO_INCREMENTAL".to_string(), "0".to_string());
    env.insert("CARGO_BUILD_JOBS".to_string(), jobs.to_string());
    env.insert("CMAKE_BUILD_PARALLEL_LEVEL".to_string(), jobs.to_string());
    env.insert("MAKEFLAGS".to_string(), format!("-j{jobs}"));
    env.insert("VITEST_MAX_THREADS".to_string(), jobs.to_string());
    env.insert("CARGO_TARGET_DIR".to_string(), format!("/target/{area}"));

    for (k, v) in &env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }

    let image = job.image.as_deref().unwrap_or(&cfg.default_image);
    argv.push(image.to_string());
    argv.push("bash".to_string());
    argv.push("-c".to_string());
    argv.push(compile_job_script(
        job,
        area,
        &writable_mounts,
        Some((uid, gid)),
    ));
    argv
}

/// R38: execution plan for a host-native (`container: None`) CI job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostJobPlan {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub scope_unit: Option<String>,
    pub reason: String,
}

/// R38: compile the argv and environment for a host-native CI job (`container: None`):
/// - Wraps with `systemd-run --user --scope --quiet --unit <scope_unit> -p AllowedCPUs=<cpus> -p MemoryMax=<mem> -p TasksMax=<pids>` when `systemd_user_available` is true.
/// - Always pins CPU affinity with `taskset -c <cpus>`.
/// - Injects warm cache paths (`CARGO_TARGET_DIR=<cache_root>/target/<area>`, `SCCACHE_DIR`, etc.).
#[allow(clippy::too_many_arguments)]
pub fn host_job_plan(
    job: &CiJob,
    area: &str,
    scope_unit: &str,
    worktree: &str,
    cache_root: &str,
    cpus: &[usize],
    jobs: usize,
    systemd_user_available: bool,
    sccache_available: bool,
    cfg: &CiRunnerConfig,
) -> HostJobPlan {
    let cpuset = cpu_list(cpus);
    let mut argv = Vec::new();
    let unit_used = if systemd_user_available {
        argv.extend([
            "systemd-run".to_string(),
            "--user".to_string(),
            "--scope".to_string(),
            "--quiet".to_string(),
            "--unit".to_string(),
            scope_unit.to_string(),
            "-p".to_string(),
            format!("AllowedCPUs={cpuset}"),
            "-p".to_string(),
            format!("MemoryMax={}", cfg.container_memory.to_uppercase()),
            "-p".to_string(),
            format!("TasksMax={}", cfg.container_pids_limit),
        ]);
        Some(scope_unit.to_string())
    } else {
        None
    };
    argv.extend([
        "taskset".to_string(),
        "-c".to_string(),
        cpuset.clone(),
        "bash".to_string(),
        "-c".to_string(),
        compile_job_script(job, area, &[], None),
    ]);

    let mut env = job.env.clone();
    for (k, v) in super::stats::rust_cache_env(cache_root, area, sccache_available, cfg) {
        env.insert(k, v);
    }
    env.insert("CI".to_string(), "true".to_string());
    env.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
    env.insert("GITHUB_WORKSPACE".to_string(), worktree.to_string());
    env.insert("CARGO_BUILD_JOBS".to_string(), jobs.to_string());
    env.insert("CMAKE_BUILD_PARALLEL_LEVEL".to_string(), jobs.to_string());
    env.insert("MAKEFLAGS".to_string(), format!("-j{jobs}"));
    env.insert("VITEST_MAX_THREADS".to_string(), jobs.to_string());

    HostJobPlan {
        argv,
        env,
        scope_unit: unit_used,
        reason: format!(
            "host job for area '{area}' pinned to CPUs [{cpuset}] with {jobs} build jobs (systemd_scope={systemd_user_available})"
        ),
    }
}
