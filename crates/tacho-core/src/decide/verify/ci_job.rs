//! CI workflow job mapping and argv.

use super::*;

/// Parse a GitHub Actions workflow file (YAML or JSON) into a `serde_json::Value`.
///
/// Fails with a descriptive error if the workflow has invalid syntax, unclosed quotes,
/// tab indentation, or malformed mappings.
pub fn parse_workflow_yaml(text: &str) -> Result<serde_json::Value, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("workflow file is empty".to_string());
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON workflow: {e}"));
    }

    let mut lines = Vec::new();
    for (i, raw_line) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let leading: String = line
            .chars()
            .take_while(|&c| c == ' ' || c == '\t')
            .collect();
        if leading.contains('\t') {
            return Err(format!(
                "line {line_no}: tabs are not allowed for YAML indentation"
            ));
        }
        let indent = leading.len();
        let raw_content = line[indent..].to_string();
        let stripped = strip_yaml_comment(&raw_content)
            .map_err(|e| format!("line {line_no}: {e}"))?
            .to_string();
        if indent == 0 && (stripped == "---" || stripped == "...") {
            continue;
        }
        lines.push(YamlLine {
            line_no,
            indent,
            raw_content,
            stripped,
        });
    }

    let mut idx = 0;
    let val = parse_yaml_block(&lines, &mut idx, 0)?;
    while idx < lines.len() {
        if !lines[idx].stripped.is_empty() {
            return Err(format!(
                "line {}: unexpected trailing content '{}'",
                lines[idx].line_no, lines[idx].stripped
            ));
        }
        idx += 1;
    }
    Ok(val)
}

/// Parse a YAML or JSON workflow text and extract the `CiJob` for `(area, job_name)`.
pub fn job_for_yaml(
    workflow_text: &str,
    area: &str,
    job_name: &str,
    expressions: &BTreeMap<String, String>,
) -> Result<CiJob, String> {
    let wf = parse_workflow_yaml(workflow_text)?;
    job_for(&wf, area, job_name, expressions)
}

pub(crate) fn parse_env_map(
    val: Option<&serde_json::Value>,
    expressions: &BTreeMap<String, String>,
    context: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let Some(v) = val else {
        return Ok(out);
    };
    let Some(obj) = v.as_object() else {
        return Err(format!("{context}: expected env mapping"));
    };
    for (k, raw_v) in obj {
        let s = match raw_v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => {
                return Err(format!(
                    "{context}: env '{k}' must be scalar (string, number, or bool)"
                ))
            }
        };
        let expanded = expand_workflow_expr(&s, expressions, &format!("{context}.env.{k}"))?;
        out.insert(k.clone(), expanded);
    }
    Ok(out)
}

/// Extract a `CiJob` for `area` (mapped to workflow job `job_name`) from a parsed GitHub Actions
/// workflow JSON/YAML value (`jobs.<job_name>`), strictly validating expressions, services,
/// working directories, and `uses:` actions.
pub fn job_for(
    workflow: &serde_json::Value,
    area: &str,
    job_name: &str,
    expressions: &BTreeMap<String, String>,
) -> Result<CiJob, String> {
    let jobs = workflow
        .get("jobs")
        .and_then(|j| j.as_object())
        .ok_or_else(|| "workflow is missing 'jobs' mapping".to_string())?;
    let job = jobs
        .get(job_name)
        .and_then(|j| j.as_object())
        .ok_or_else(|| format!("workflow has no job '{job_name}' for area '{area}'"))?;

    let ctx = format!("jobs.{job_name}");

    let image = match job.get("container") {
        Some(serde_json::Value::String(s)) => Some(expand_workflow_expr(
            s,
            expressions,
            &format!("{ctx}.container"),
        )?),
        Some(serde_json::Value::Object(map)) => match map.get("image") {
            Some(serde_json::Value::String(s)) => Some(expand_workflow_expr(
                s,
                expressions,
                &format!("{ctx}.container.image"),
            )?),
            _ => return Err(format!("{ctx}.container is missing string 'image'")),
        },
        Some(_) => return Err(format!("{ctx}.container must be a string or mapping")),
        None => None,
    };

    let env = parse_env_map(job.get("env"), expressions, &ctx)?;

    let service = if let Some(services_val) = job.get("services") {
        let services = services_val
            .as_object()
            .ok_or_else(|| format!("{ctx}.services must be a mapping"))?;
        if services.len() > 1 {
            return Err(format!(
                "{ctx}.services has {} services; at most 1 service container is supported",
                services.len()
            ));
        }
        if let Some((svc_name, svc_val)) = services.iter().next() {
            let svc_obj = svc_val
                .as_object()
                .ok_or_else(|| format!("{ctx}.services.{svc_name} must be a mapping"))?;
            let svc_image = svc_obj
                .get("image")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("{ctx}.services.{svc_name} is missing 'image'"))?;
            let svc_image = expand_workflow_expr(
                svc_image,
                expressions,
                &format!("{ctx}.services.{svc_name}.image"),
            )?;
            let svc_env = parse_env_map(
                svc_obj.get("env"),
                expressions,
                &format!("{ctx}.services.{svc_name}"),
            )?;
            let mut ports = Vec::new();
            if let Some(ports_val) = svc_obj.get("ports") {
                let arr = ports_val
                    .as_array()
                    .ok_or_else(|| format!("{ctx}.services.{svc_name}.ports must be a list"))?;
                for p in arr {
                    match p {
                        serde_json::Value::String(s) => ports.push(s.clone()),
                        serde_json::Value::Number(n) => ports.push(n.to_string()),
                        _ => {
                            return Err(format!(
                                "{ctx}.services.{svc_name}.ports entries must be strings or numbers"
                            ))
                        }
                    }
                }
            }
            let options = svc_obj
                .get("options")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(CiService {
                name: svc_name.clone(),
                image: svc_image,
                env: svc_env,
                ports,
                options,
            })
        } else {
            None
        }
    } else {
        None
    };

    let raw_steps = job
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or_else(|| format!("{ctx} is missing 'steps' list"))?;

    let mut steps = Vec::new();
    for (idx, step_val) in raw_steps.iter().enumerate() {
        let step_ctx = format!("{ctx}.steps[{idx}]");
        let step_obj = step_val
            .as_object()
            .ok_or_else(|| format!("{step_ctx} must be a mapping"))?;
        let step_name = step_obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("step-{}", idx + 1));

        if let Some(if_val) = step_obj.get("if") {
            let cond = if_val.as_str().unwrap_or("");
            return Err(format!(
                "{step_ctx} ('{step_name}'): conditional 'if: {cond}' is not supported in local batch runner"
            ));
        }

        if let Some(uses_val) = step_obj.get("uses") {
            let uses = uses_val
                .as_str()
                .ok_or_else(|| format!("{step_ctx}.uses must be a string"))?;
            let action_prefix = uses.split('@').next().unwrap_or(uses);
            if STAND_IN_USES.contains(&action_prefix) {
                continue;
            }
            return Err(format!(
                "{step_ctx} ('{step_name}'): unsupported action 'uses: {uses}'"
            ));
        }

        let Some(run_val) = step_obj.get("run") else {
            return Err(format!(
                "{step_ctx} ('{step_name}'): step has neither 'run' nor a stand-in 'uses'"
            ));
        };
        let run_raw = run_val
            .as_str()
            .ok_or_else(|| format!("{step_ctx}.run must be a string"))?;
        let run = expand_workflow_expr(run_raw, expressions, &format!("{step_ctx}.run"))?;

        let working_directory = if let Some(wd_val) = step_obj.get("working-directory") {
            let wd = wd_val
                .as_str()
                .ok_or_else(|| format!("{step_ctx}.working-directory must be a string"))?;
            let wd =
                expand_workflow_expr(wd, expressions, &format!("{step_ctx}.working-directory"))?;
            if wd.starts_with('/')
                || wd.split('/').any(|seg| seg == "..")
                || wd.contains('\'')
                || wd.contains('\n')
            {
                return Err(format!(
                    "{step_ctx} ('{step_name}'): unsafe working-directory '{wd}'"
                ));
            }
            Some(wd)
        } else {
            None
        };

        let step_env = parse_env_map(step_obj.get("env"), expressions, &step_ctx)?;
        steps.push(CiStep {
            name: step_name,
            run,
            working_directory,
            env: step_env,
        });
    }

    if steps.is_empty() {
        return Err(format!("{ctx} has no runnable steps"));
    }

    Ok(CiJob {
        area: area.to_string(),
        name: job_name.to_string(),
        image,
        env,
        service,
        steps,
    })
}

/// Build the `docker run -d` command line for a job's attached `CiService`.
pub fn service_argv(service: &CiService, container_name: &str) -> Vec<String> {
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
    ];
    for (k, v) in &service.env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }
    if !service.options.trim().is_empty() {
        for tok in service.options.split_whitespace() {
            argv.push(tok.to_string());
        }
    }
    argv.push(service.image.clone());
    argv
}

/// Render the shell script that executes all `CiStep`s in a `CiJob` sequentially with `set -euo pipefail`
/// and `=== step: <name> ===` progress markers.
pub fn job_script(job: &CiJob) -> String {
    let mut script = String::from("set -euo pipefail\n");
    for step in &job.steps {
        let safe_name = step.name.replace('\n', " ");
        script.push_str(&format!(
            "echo '=== step: {} ==='\n",
            safe_name.replace('\'', "'\\''")
        ));
        script.push_str("(\n");
        if let Some(ref wd) = step.working_directory {
            script.push_str(&format!("  cd '{wd}'\n"));
        }
        for (k, v) in &step.env {
            let escaped = v.replace('\'', "'\\''");
            script.push_str(&format!("  export {k}='{escaped}'\n"));
        }
        script.push_str(&step.run);
        if !step.run.ends_with('\n') {
            script.push('\n');
        }
        script.push_str(")\n");
    }
    script
}

/// Build the `docker run --rm` command line (or host `bash -lc` command if `job.image` is `None`)
/// to execute a `CiJob` against `worktree`, sharing the network namespace with `service_container`
/// when present and mounting `cache_mounts` (`(host_path, container_path)`).
pub fn job_argv(
    job: &CiJob,
    worktree: &str,
    service_container: Option<&str>,
    cache_mounts: &[(String, String)],
) -> Vec<String> {
    let script = job_script(job);
    let Some(ref image) = job.image else {
        let mut full_script = String::new();
        for (k, v) in &job.env {
            let escaped = v.replace('\'', "'\\''");
            full_script.push_str(&format!("export {k}='{escaped}'\n"));
        }
        full_script.push_str(&script);
        return vec!["bash".to_string(), "-lc".to_string(), full_script];
    };

    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        format!("{worktree}:/work"),
        "-w".to_string(),
        "/work".to_string(),
    ];
    for (host_dir, container_dir) in cache_mounts {
        argv.push("-v".to_string());
        argv.push(format!("{host_dir}:{container_dir}"));
    }
    if let Some(svc) = service_container {
        argv.push("--network".to_string());
        argv.push(format!("container:{svc}"));
    }
    for (k, v) in &job.env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }
    argv.push(image.clone());
    argv.push("bash".to_string());
    argv.push("-lc".to_string());
    argv.push(script);
    argv
}
