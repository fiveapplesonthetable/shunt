//! Versioned repository configuration for the shared CLI and daemon runtime.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::decide::verify::AreaRule;

/// The operation a repository implements at an orchestration boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Allocates the dependencies of an isolated checkout.
    Prepare,
    /// Selects checks for changed files.
    ProofPlan,
    /// Checks a proposed merged tree.
    Verify,
    /// Admits work to a trusted runner.
    RunnerTrust,
    /// Delivers an owner notification.
    Notify,
}

/// An explicit program and argv; request values are supplied through JSON stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookCommand {
    /// Executable name or path.
    pub program: String,
    /// Literal arguments, without shell expansion.
    #[serde(default)]
    pub args: Vec<String>,
    /// Maximum duration of a hook invocation.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

impl HookCommand {
    /// The complete command passed directly to the operating system.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.clone())
            .collect()
    }
}

/// The repository policy and hooks explicitly consumed by Shunt's runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Contract version; unsupported versions are refused.
    pub version: u32,
    /// Optional forge repository in owner/name form.
    #[serde(default)]
    pub repository: Option<String>,
    /// Project-owned mapping from paths to verification areas.
    pub area_rules: Vec<AreaRule>,
    /// Project implementations of runtime operations.
    #[serde(default)]
    pub hooks: BTreeMap<HookKind, HookCommand>,
    /// Workflow used for main CI observation.
    #[serde(default)]
    pub workflow_path: Option<String>,
    /// Area-to-job mapping for the workflow adapter.
    #[serde(default)]
    pub workflow_jobs: BTreeMap<String, String>,
    /// Explicit values for workflow expressions used by the adapter.
    #[serde(default)]
    pub workflow_expressions: BTreeMap<String, String>,
}

impl ProjectConfig {
    /// Refuses incomplete or ambiguous configuration before any runtime state changes.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported project contract version {}",
                self.version
            ));
        }
        if self.repository.as_ref().is_some_and(|repo| {
            let parts: Vec<_> = repo.split('/').collect();
            parts.len() != 2
                || parts.iter().any(|part| {
                    part.is_empty()
                        || !part
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                })
        }) {
            return Err("repository must have owner/name form".into());
        }
        let mut areas = BTreeSet::new();
        if self.area_rules.is_empty() {
            return Err("a project must define its verification areas".into());
        }
        for rule in &self.area_rules {
            if rule.area.trim().is_empty() || !areas.insert(&rule.area) {
                return Err("verification areas must have distinct nonempty names".into());
            }
        }
        for command in self.hooks.values() {
            if command.program.trim().is_empty()
                || command.argv().iter().any(|arg| arg.contains('\0'))
            {
                return Err("hook commands require a program and NUL-free arguments".into());
            }
            if !(1..=3600).contains(&command.timeout_secs) {
                return Err("hook timeout_secs must be between 1 and 3600".into());
            }
        }
        if self.hooks.contains_key(&HookKind::Verify)
            && !self.hooks.contains_key(&HookKind::RunnerTrust)
        {
            return Err("a verification hook requires a runner_trust hook".into());
        }
        Ok(())
    }
}

/// Scrubs notifications using the same configured secrets in every runtime path.
pub fn scrub_notification(
    subject: &str,
    body: &str,
    env: &BTreeMap<String, String>,
) -> (String, String) {
    let scrubber = crate::Scrubber::enabled();
    let mut subject = scrubber.scrub(subject);
    let mut body = scrubber.scrub(body);
    for secret in crate::decide::ops::snapshot::configured_secrets(env) {
        if !secret.is_empty() {
            subject = subject.replace(&secret, "<redacted>");
            body = body.replace(&secret, "<redacted>");
        }
    }
    (subject, body)
}

/// Context passed to a project hook as one JSON document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequest {
    /// Contract version.
    pub version: u32,
    /// Operation requested by the runtime.
    pub kind: HookKind,
    /// Repository checkout supplying trusted configuration.
    pub repository_root: String,
    /// Checkout on which the hook operates.
    pub worktree: String,
    /// Base commit, when the operation has one.
    pub base: String,
    /// Current commit, when the operation has one.
    pub head: String,
    /// Exact input tree, including local edits for a worker proof.
    pub tree: String,
    /// Changed paths supplied by the runtime.
    pub files: Vec<String>,
    /// Verification areas supplied by the runtime.
    pub areas: Vec<String>,
    /// Operation-specific data, such as a notification or batch membership.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Environment produced by a repository's preparation hook.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareResult {
    /// Values for the worker or verification command, without shell evaluation.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

impl PrepareResult {
    /// Prevents preparation from redirecting the controller's state or home directory.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in &self.environment {
            let valid = name.bytes().enumerate().all(|(index, c)| {
                c == b'_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
            });
            if name.is_empty()
                || !valid
                || name.starts_with("TACHO_")
                || matches!(
                    name.as_str(),
                    "HOME" | "CODEX_HOME" | "CODEX_HOME_DIR" | "SHUNT_HOME" | "SHUNT_RUNTIME"
                )
                || value.contains('\0')
            {
                return Err(
                    "preparation contains an invalid or controller-owned environment variable"
                        .into(),
                );
            }
        }
        Ok(())
    }
}
