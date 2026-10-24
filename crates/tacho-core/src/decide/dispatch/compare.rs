//! Provider/model inference, `--compare` expansion, and `session` validation (R59–R62).

use super::config::DispatchConfig;
use super::model::{ItemKind, Session, TodoItem};

/// Infer which provider runs `model` (R59).
pub fn provider_of(model: &str, cfg: &DispatchConfig) -> Result<String, String> {
    if model.starts_with("claude-") || cfg.providers.claude_families.iter().any(|f| f == model) {
        return Ok("claude".to_string());
    }
    if cfg
        .providers
        .codex_prefixes
        .iter()
        .any(|p| model.starts_with(p.as_str()))
    {
        return Ok("codex".to_string());
    }
    if cfg
        .providers
        .gemini_prefixes
        .iter()
        .any(|p| model.starts_with(p.as_str()))
    {
        return Ok("gemini".to_string());
    }
    Err(format!(
        "cannot tell which provider runs '{model}'; add --provider codex or --provider claude"
    ))
}

/// `(provider, model)` for a task that names either, or `Ok(None)` when it names neither and the
/// session decides (R60).
pub fn resolve(
    provider: Option<&str>,
    model: Option<&str>,
    cfg: &DispatchConfig,
) -> Result<Option<(String, String)>, String> {
    let p = provider.filter(|s| !s.is_empty());
    let m = model.filter(|s| !s.is_empty());
    if p.is_none() && m.is_none() {
        return Ok(None);
    }
    let prov = match p {
        Some(p) => p.to_string(),
        None => provider_of(m.unwrap_or_default(), cfg)?,
    };
    let Some(m) = m else {
        let def = match prov.as_str() {
            "codex" => cfg.providers.codex_default.clone(),
            "gemini" => cfg.providers.gemini_default.clone(),
            _ => cfg.providers.claude_default.clone(),
        };
        return Ok(Some((prov, def)));
    };
    if provider_of(m, cfg)? != prov {
        return Err(format!("'{m}' is not a {prov} model"));
    }
    Ok(Some((prov, m.to_string())))
}

/// Parse `codex:gpt-5.4,claude:sonnet` into unique `(provider, model)` workers in order; a bare
/// word is a model with an inferred provider (R61).
pub fn parse_compare(spec: &str, cfg: &DispatchConfig) -> Result<Vec<(String, String)>, String> {
    let mut workers = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let chosen = match part.split_once(':') {
            Some((prov, model)) if !model.is_empty() => resolve(Some(prov), Some(model), cfg)?,
            _ => resolve(None, Some(part), cfg)?,
        };
        if let Some(pair) = chosen {
            if !workers.contains(&pair) {
                workers.push(pair);
            }
        }
    }
    Ok(workers)
}

/// The branch and PR suffix that tells compared runs apart (R61).
pub fn worker_suffix(model: &str) -> String {
    model
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// The brief with the instructions that keep compared runs apart and their PRs drafts (R61).
pub fn compare_task(task: &str, key: &str, suffix: &str, model: &str) -> String {
    format!(
        "{task}\n\nThis is one of several runs of the same brief that the owner compares on real \
         work. Use the branch name `{key}-{suffix}`, open the PR as a draft and leave it a draft, \
         and end its title with `[{model}]`. Do not look at the other runs' branches."
    )
}

/// One queue entry per worker, each in its own lane (`area-suffix`) so they run side by side on
/// separate branches (R61).
pub fn compare_items(
    key: &str,
    task: &str,
    area: &str,
    after: &[String],
    workers: &[(String, String)],
) -> Vec<TodoItem> {
    workers
        .iter()
        .map(|(provider, model)| {
            let suffix = worker_suffix(model);
            TodoItem {
                key: format!("{key}-{suffix}"),
                task: compare_task(task, key, &suffix, model),
                model: Some(model.clone()),
                provider: Some(provider.clone()),
                area: if area.is_empty() {
                    String::new()
                } else {
                    format!("{area}-{suffix}")
                },
                after: after.to_vec(),
                now: false,
                kind: ItemKind::Task,
                pr: None,
            }
        })
        .collect()
}

/// Validate and expand a `--compare` spec for `todo` (requires at least 2 workers; case 100 in `test_task_provider.py`).
pub fn plan_compare(
    key: &str,
    task: &str,
    area: &str,
    after: &[String],
    spec: &str,
    cfg: &DispatchConfig,
) -> Result<Vec<TodoItem>, String> {
    let workers = parse_compare(spec, cfg)?;
    if workers.len() < 2 {
        return Err("--compare needs at least two workers".to_string());
    }
    Ok(compare_items(key, task, area, after, &workers))
}

/// `provider/model` for a task that chose its provider, else the model alone (R61).
pub fn label(item: &TodoItem) -> String {
    let m = item.model.as_deref().unwrap_or_default();
    match item.provider.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}/{m}"),
        None => m.to_string(),
    }
}

/// Validate the `session` command arguments (R62).
///
/// Requires a name and `--provider`; a non-codex session requires `--model` (exit 2); a codex
/// session defaults to `codex_default`.
pub fn resolve_session(
    name: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    cfg: &DispatchConfig,
) -> Result<Session, String> {
    let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("session requires a name".to_string());
    };
    let Some(provider) = provider.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("session requires --provider".to_string());
    };
    let model_opt = model.map(str::trim).filter(|s| !s.is_empty());
    if provider != "codex" && model_opt.is_none() {
        return Err(format!("a {provider} session requires --model"));
    }
    let (_, resolved_model) = resolve(Some(provider), model_opt, cfg)?
        .ok_or_else(|| "session requires a provider".to_string())?;
    Ok(Session {
        name: name.to_string(),
        provider: provider.to_string(),
        model: resolved_model,
    })
}
