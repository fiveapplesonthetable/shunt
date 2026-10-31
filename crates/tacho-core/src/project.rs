//! Versioned repository configuration for the shared CLI and daemon runtime.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::decide::verify::{AreaRule, GroupMatch, GroupRule};

/// Ordered project rules for assigning a stack to an independently held batch group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchGrouping {
    /// Group for changes to the verification pipeline; each runs alone.
    pub ci_group: String,
    /// Regexes that identify pipeline changes.
    pub ci_files: Vec<String>,
    /// Areas that do not affect a stack's group.
    pub group_ignore: BTreeSet<String>,
    /// First matching area rule determines the group.
    pub groups: Vec<GroupRule>,
    /// Group used when no rule matches.
    pub fallback_group: String,
}

impl BatchGrouping {
    /// Determines the group from the union of a stack's changed areas.
    pub fn group(&self, areas: &BTreeSet<String>) -> String {
        let rest: BTreeSet<_> = areas.difference(&self.group_ignore).cloned().collect();
        self.groups
            .iter()
            .find(|rule| match &rule.when {
                GroupMatch::SupersetOf(want) => want.is_subset(&rest),
                GroupMatch::SubsetOf(allowed) => rest.is_subset(allowed),
                GroupMatch::NonEmptySubsetOf(allowed) => {
                    !rest.is_empty() && rest.is_subset(allowed)
                }
            })
            .map(|rule| rule.name.clone())
            .unwrap_or_else(|| self.fallback_group.clone())
    }

    /// Identifies a pipeline change using validated project patterns.
    pub fn pipeline(&self, files: &[String]) -> bool {
        self.ci_files.iter().any(|pattern| {
            regex::Regex::new(pattern)
                .map(|pattern| files.iter().any(|file| pattern.is_match(file)))
                .unwrap_or(true)
        })
    }

    fn validate(&self) -> Result<(), String> {
        let mut names = BTreeSet::new();
        if self.ci_group.trim().is_empty()
            || self.fallback_group.trim().is_empty()
            || self
                .groups
                .iter()
                .any(|rule| rule.name.trim().is_empty() || !names.insert(&rule.name))
        {
            return Err(
                "batch groups need distinct nonempty rule names and named pipeline/fallback groups"
                    .into(),
            );
        }
        generated_output_matcher(&self.ci_files)?;
        Ok(())
    }
}

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
    /// Publishes current verified queue state independently of announcements.
    Publish,
}

/// A project transport's explicit acknowledgment of a notification attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    /// The owner announcement was accepted or already acknowledged.
    Sent,
    /// No delivery was acknowledged; the same key remains pending.
    Pending,
    /// Acceptance cannot be established and requires an operator finding.
    Uncertain,
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

/// A project-owned list of reference files that remain immutable check inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputList {
    /// Repository-relative UTF-8 list file, with one path per line and `#` comments.
    pub file: String,
    /// Repository-relative directory against which listed paths are resolved.
    pub root: String,
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
    /// Regexes of derived files collected as artifacts and restored after checks.
    #[serde(default)]
    pub generated_outputs: Vec<String>,
    /// Reference-file lists that take precedence over generated-output patterns.
    #[serde(default)]
    pub input_lists: Vec<InputList>,
    /// Whether a worker's `PR: none` report requires explicit confirmation.
    #[serde(default)]
    pub review_no_pr: bool,
    /// Project grouping policy consumed by both planning and batch claims.
    #[serde(default)]
    pub batch_grouping: Option<BatchGrouping>,
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
    /// Identifies the installed policy that selects and executes verification.
    pub fn verification_identity(&self) -> String {
        let hooks: BTreeMap<_, _> = self
            .hooks
            .iter()
            .filter(|(kind, _)| {
                matches!(
                    kind,
                    HookKind::Prepare
                        | HookKind::ProofPlan
                        | HookKind::Verify
                        | HookKind::RunnerTrust
                )
            })
            .collect();
        content_id(serde_json::json!({"repository":self.repository,"areas":self.area_rules,"hooks":hooks,"generated_outputs":self.generated_outputs,"input_lists":self.input_lists,"workflow_path":self.workflow_path,"workflow_jobs":self.workflow_jobs,"workflow_expressions":self.workflow_expressions}).to_string().as_bytes())
    }

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
            generated_output_matcher(&rule.patterns)?;
            generated_output_matcher(&rule.fallback_patterns)?;
            generated_output_matcher(&rule.required_patterns)?;
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
        generated_output_matcher(&self.generated_outputs)?;
        if let Some(grouping) = &self.batch_grouping {
            grouping.validate()?;
        }
        for list in &self.input_lists {
            for path in [&list.file, &list.root] {
                if path.contains('\0')
                    || std::path::Path::new(path)
                        .components()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err("input list paths must stay inside the repository".into());
                }
            }
            if list.file.is_empty() {
                return Err("an input list requires its file path".into());
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

/// Compiles the project's declared derived-file patterns once for a collection run.
pub fn generated_output_matcher(patterns: &[String]) -> Result<regex::RegexSet, String> {
    regex::RegexSet::new(patterns)
        .map_err(|error| format!("invalid generated output pattern: {error}"))
}

/// Identifies an immutable migration snapshot by its complete bytes.
pub fn content_id(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
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
