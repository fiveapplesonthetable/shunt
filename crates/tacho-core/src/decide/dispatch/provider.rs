//! Provider command specs: argv templates, sessions, tell/stop plans.
#![allow(unused_imports)]

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

#[allow(unused_imports)]
use super::*;

// =============================================================================
// Provider Interface & Configuration
// =============================================================================

/// Planned command invocation for a provider lifecycle step (`start` or `resume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCommand {
    /// Binary or script to execute.
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
    /// Environment variables `(key, value)` to set for the child process.
    pub env: Vec<(String, String)>,
}

/// Planned delivery for `tacho tell`: either live delivery into a running worker
/// (via an inbox / live command) or queued for the next turn/resume without killing
/// the running session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TellPlan {
    /// Deliver to a live running session via a custom provider tell command (`{session}`, `{message}`, `{worktree}`).
    LiveCommand { program: String, args: Vec<String> },
    /// Deliver to a live running session via the managed hook inbox so the next PreToolUse/UserPromptSubmit
    /// hook injects the message into the running turn without restarting the worker.
    LiveInbox {
        key: String,
        session_id: Option<String>,
        message: String,
    },
    /// Worker is not currently live; resume the session with the message.
    ResumeWithMessage {
        key: String,
        session_id: Option<String>,
        command: ProviderCommand,
    },
}

/// Planned stop action for a provider worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopPlan {
    pub key: String,
    pub pid: Option<u32>,
    pub custom_cmd: Option<(String, Vec<String>)>,
}

/// Trait representing an agent provider (Claude, Codex, Gemini, or custom configured provider).
///
/// Every method is a pure planner or parser over plain data structures:
/// - `start`: plan the command to start a new worker session with a prompt in a worktree.
/// - `resume`: plan the command to resume an existing session (by `session_id` or in-place worktree) with a prompt.
/// - `tell`: plan how to deliver a message to a live or stopped worker without cold-restarting a live session.
/// - `alive`: check whether a worker is alive given its PID and observed live process table.
/// - `stop`: plan how to stop a worker session cleanly.
/// - `cost`: parse or compute the USD cost of a provider run from its transcript/usage observation.
pub trait Provider {
    /// Provider identifier (e.g. `"claude"`, `"codex"`, `"gemini"`).
    fn name(&self) -> &str;

    /// Plan the command to launch a fresh worker session.
    fn start(
        &self,
        key: &str,
        worktree: &str,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand;

    /// Plan the command to resume a prior worker session (`session_id` if available) with `message`.
    fn resume(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand;

    /// Plan how to deliver `message` to a worker (`is_live` indicates whether the worker process is currently running).
    #[allow(clippy::too_many_arguments)]
    fn tell(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        is_live: bool,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> TellPlan;

    /// Return whether the worker is currently alive given its recorded `pid` and the set of live PIDs on the host.
    fn alive(&self, pid: Option<u32>, live_pids: &BTreeSet<u32>) -> bool;

    /// Plan how to stop the worker.
    fn stop(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        pid: Option<u32>,
    ) -> StopPlan;

    /// Compute or extract USD cost from a provider transcript (`jsonl_lines`) or fallback token counts.
    fn cost(&self, jsonl_lines: &[&str], model: Option<&str>) -> f64;
}

/// Configurable specification for an agent provider.
///
/// Allows projects to configure custom providers or override flags/binaries for built-in providers
/// (`claude`, `codex`, `gemini`) in fleet config without modifying Rust code.
///
/// Template placeholders supported in `start_args`, `resume_args`, `tell_args`, and `stop_args`:
/// - `{prompt}` / `{message}`: the task brief or follow-up message
/// - `{session}`: the provider session ID
/// - `{worktree}`: the worker worktree path
/// - `{key}`: the worker key
/// - `{model}`: the resolved model name
/// - `{effort}`: the resolved effort level
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderSpec {
    pub name: String,
    pub binary: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub start_args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub resume_without_session_args: Vec<String>,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub effort_flag: Option<String>,
    #[serde(default)]
    pub tell_program: Option<String>,
    #[serde(default)]
    pub tell_args: Vec<String>,
    #[serde(default)]
    pub supports_hook_inbox: bool,
    #[serde(default)]
    pub stop_program: Option<String>,
    #[serde(default)]
    pub stop_args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
}

const DEFAULT_PROVIDERS_JSON: &str = include_str!("../default_providers.json");

impl ProviderSpec {
    /// Load `claude` specification from default provider config.
    pub fn claude() -> Self {
        builtin_providers()
            .remove("claude")
            .unwrap_or_else(|| Self {
                name: "claude".to_string(),
                binary: "claude".to_string(),
                supports_hook_inbox: true,
                ..Default::default()
            })
    }

    /// Load `codex` specification from default provider config.
    pub fn codex() -> Self {
        builtin_providers().remove("codex").unwrap_or_else(|| Self {
            name: "codex".to_string(),
            binary: "codex".to_string(),
            ..Default::default()
        })
    }

    /// Load `gemini` specification from default provider config.
    pub fn gemini() -> Self {
        builtin_providers()
            .remove("gemini")
            .unwrap_or_else(|| Self {
                name: "gemini".to_string(),
                binary: "gemini".to_string(),
                ..Default::default()
            })
    }

    /// Plan how to deliver `message` to a session taking into account whether the process is
    /// running (`is_live`) and whether the session is currently idle between turns (`is_idle`).
    ///
    /// - If `is_live` and the provider defines `tell_program`/`tell_args`, runs `LiveCommand`.
    /// - If `is_live`, `!is_idle`, and `supports_hook_inbox`, queues `LiveInbox` for the active hook turn.
    /// - Otherwise (stopped session, idle session, or provider with no `tell_args` and no hook inbox),
    ///   returns `ResumeWithMessage` so the session is resumed immediately with `message`.
    #[allow(clippy::too_many_arguments)]
    pub fn tell_session(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        is_live: bool,
        is_idle: bool,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> TellPlan {
        if is_live {
            if let Some(ref prog) = self.tell_program {
                let args = Self::expand_template(
                    &self.tell_args,
                    key,
                    worktree,
                    session_id,
                    message,
                    model,
                    effort,
                );
                return TellPlan::LiveCommand {
                    program: prog.clone(),
                    args,
                };
            }
            if self.supports_hook_inbox && !is_idle {
                return TellPlan::LiveInbox {
                    key: key.to_string(),
                    session_id: session_id.map(str::to_string),
                    message: message.to_string(),
                };
            }
        }
        TellPlan::ResumeWithMessage {
            key: key.to_string(),
            session_id: session_id.map(str::to_string),
            command: self.resume(key, worktree, session_id, message, model, effort),
        }
    }

    fn expand_template(
        template: &[String],
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        prompt_or_msg: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Vec<String> {
        template
            .iter()
            .map(|arg| {
                arg.replace("{key}", key)
                    .replace("{worktree}", worktree)
                    .replace("{session}", session_id.unwrap_or(""))
                    .replace("{prompt}", prompt_or_msg)
                    .replace("{message}", prompt_or_msg)
                    .replace("{model}", model.unwrap_or(""))
                    .replace("{effort}", effort.unwrap_or(""))
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn build_command(
        &self,
        template: &[String],
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        prompt_or_msg: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        let eff_model = model
            .filter(|s| !s.trim().is_empty())
            .or(self.default_model.as_deref());
        let eff_effort = effort
            .filter(|s| !s.trim().is_empty())
            .or(self.default_effort.as_deref());

        let mut args = Vec::new();
        let template_uses_model = template.iter().any(|a| a.contains("{model}"));
        let template_uses_effort = template.iter().any(|a| a.contains("{effort}"));

        if !template_uses_model {
            if let (Some(flag), Some(m)) = (&self.model_flag, eff_model) {
                args.push(flag.clone());
                args.push(m.to_string());
            }
        }
        if !template_uses_effort {
            if let (Some(flag), Some(e)) = (&self.effort_flag, eff_effort) {
                args.push(flag.clone());
                args.push(e.to_string());
            }
        }

        args.extend(Self::expand_template(
            template,
            key,
            worktree,
            session_id,
            prompt_or_msg,
            eff_model,
            eff_effort,
        ));

        let mut env = self.env.clone();
        env.push(("TACHO_AGENT".to_string(), key.to_string()));
        env.push(("TACHO_WORKTREE".to_string(), worktree.to_string()));

        ProviderCommand {
            program: self.binary.clone(),
            args,
            env,
        }
    }
}

impl Provider for ProviderSpec {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(
        &self,
        key: &str,
        worktree: &str,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        self.build_command(&self.start_args, key, worktree, None, prompt, model, effort)
    }

    fn resume(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        let template = if session_id.filter(|s| !s.trim().is_empty()).is_some() {
            &self.resume_args
        } else if !self.resume_without_session_args.is_empty() {
            &self.resume_without_session_args
        } else {
            &self.start_args
        };
        self.build_command(template, key, worktree, session_id, message, model, effort)
    }

    #[allow(clippy::too_many_arguments)]
    fn tell(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        is_live: bool,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> TellPlan {
        self.tell_session(
            key, worktree, session_id, message, is_live, false, model, effort,
        )
    }

    fn alive(&self, pid: Option<u32>, live_pids: &BTreeSet<u32>) -> bool {
        pid.is_some_and(|p| p > 0 && live_pids.contains(&p))
    }

    fn stop(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        pid: Option<u32>,
    ) -> StopPlan {
        let custom_cmd = self.stop_program.as_ref().map(|prog| {
            (
                prog.clone(),
                Self::expand_template(&self.stop_args, key, worktree, session_id, "", None, None),
            )
        });
        StopPlan {
            key: key.to_string(),
            pid,
            custom_cmd,
        }
    }

    fn cost(&self, jsonl_lines: &[&str], model: Option<&str>) -> f64 {
        let eff_model = model
            .or(self.default_model.as_deref())
            .unwrap_or(&self.name);
        let mut explicit_usd = 0.0;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;
        for line in jsonl_lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(c) = v.get("total_cost_usd").and_then(|x| x.as_f64()) {
                if c > explicit_usd {
                    explicit_usd = c;
                }
            }
            if let Some(u) = v.get("usage") {
                in_tok += u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                out_tok += u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                cache_read += u
                    .get("cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                cache_write += u
                    .get("cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
            }
        }
        if explicit_usd > 0.0 {
            explicit_usd
        } else {
            crate::decide::watch::price(eff_model, in_tok, out_tok, cache_read, cache_write)
        }
    }
}

/// Return the default provider registry loaded from `default_providers.json`.
pub fn builtin_providers() -> BTreeMap<String, ProviderSpec> {
    serde_json::from_str::<BTreeMap<String, ProviderSpec>>(DEFAULT_PROVIDERS_JSON)
        .unwrap_or_default()
}

/// Resolve `(provider, model, effort)` against a configured provider map (falling back to default config).
pub fn resolve_with_providers(
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    default_provider: &str,
    default_model: Option<&str>,
    default_effort: Option<&str>,
    custom_providers: &BTreeMap<String, ProviderSpec>,
) -> Result<Choice, String> {
    let mut merged = builtin_providers();
    for (k, v) in custom_providers {
        merged.insert(k.clone(), v.clone());
    }

    let raw_model = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| default_model.map(str::trim).filter(|s| !s.is_empty()));

    let mut alias_provider: Option<String> = None;
    let mut canonical_model: Option<String> = raw_model.map(str::to_string);
    if let Some(m) = raw_model {
        let lower = m.to_ascii_lowercase();
        for (prov_name, spec) in &merged {
            if let Some(canon) = spec.model_aliases.get(&lower) {
                alias_provider = Some(prov_name.clone());
                canonical_model = Some(canon.clone());
                break;
            }
        }
        if alias_provider.is_none() {
            if let Some(inf) = provider_of(Some(m)) {
                alias_provider = Some(inf.to_string());
            }
        }
    }

    let prov = match provider.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => {
            if !merged.contains_key(p) {
                let valid: Vec<&str> = merged.keys().map(String::as_str).collect();
                return Err(format!(
                    "unknown provider '{p}' (expected one of {valid:?})"
                ));
            }
            if let Some(ref inf) = alias_provider {
                if inf != p
                    && merged.get(p).is_none_or(|s| {
                        !s.model_aliases
                            .contains_key(&raw_model.unwrap_or_default().to_ascii_lowercase())
                    })
                {
                    return Err(format!(
                        "model '{}' belongs to provider '{inf}', not '{p}'",
                        raw_model.unwrap_or_default()
                    ));
                }
            }
            p.to_string()
        }
        None => alias_provider.unwrap_or_else(|| default_provider.to_string()),
    };

    let spec = merged.get(&prov);
    let final_model = canonical_model.or_else(|| spec.and_then(|s| s.default_model.clone()));
    let final_effort = effort
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| spec.and_then(|s| s.default_effort.clone()))
        .or_else(|| {
            default_effort
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });

    Ok(Choice {
        provider: prov,
        model: final_model,
        effort: final_effort,
    })
}
