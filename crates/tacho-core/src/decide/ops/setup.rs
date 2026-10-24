//! Pure setup & bootstrap planning: env template rendering, undocumented-variable detection,
//! symlink planning, tool needs, Docker address pool advice, partition advice, and CI runner steps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::OpsConfig;

/// Policy and defaults for setup planning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupPolicy {
    pub pool_base: String,
    pub pool_size: u8,
    pub pool_networks: u32,
    pub hooks: Vec<String>,
    pub linked_bins: Vec<String>,
    pub units: Vec<String>,
    pub prune_lock: String,
    pub prune_bin: String,
}

impl Default for SetupPolicy {
    fn default() -> Self {
        Self {
            pool_base: "10.210.0.0/16".into(),
            pool_size: 24,
            pool_networks: 256,
            hooks: vec![
                "agent-guard.py".into(),
                "bash-guard.py".into(),
                "context-cap.py".into(),
            ],
            linked_bins: vec!["tacho".into(), "agent-db".into(), "gradle-slot".into()],
            units: vec![
                "tacho.service".into(),
                "tacho-serve.service".into(),
                "tacho-watchdog.timer".into(),
            ],
            prune_lock: "/var/tmp/tacho-docker-prune.lock".into(),
            prune_bin: "/usr/bin/docker".into(),
        }
    }
}

/// Default `.env.example` template with `@HOME@`, `@REPO@`, `@WORK@`, `@CODE@` placeholders
/// and no hardcoded secret values (TP1–TP3).
pub fn default_env_template(prefix: &str) -> String {
    let p = prefix;
    format!(
        r#"# Fleet orchestrator environment configuration
{p}HOME=@HOME@
{p}REPO=@REPO@
{p}WORK=@WORK@
{p}CODE=@CODE@
{p}AGENT_SLOTS=12
{p}MAX_CPU_PERCENT=90
{p}MAX_LOAD=0
{p}PSQL=/usr/bin/psql
{p}ANDROID_HOME=/opt/android-sdk
{p}GCLOUD=/usr/bin/gcloud
{p}GIT_NAME=fleet-bot
#{p}ADB_SERVER=
#{p}NOTIFY=
#{p}OWNER=
#{p}OWNER_ISSUE=0
#{p}STATUS_TOKEN_FILE=
{p}AGENT=claude
{p}AGENT_CODEX=codex
{p}CODEX_MODEL=gpt-5.4
{p}WARM=@HOME@/.cache/warm
{p}SCCACHE=@HOME@/.cache/sccache
{p}CACHE=@HOME@/.cache/tacho
#{p}TARGETS=
#{p}TARGET=
{p}BATCH_WALL_CLOCK=5400
{p}SERVE_PORT=8787
#{p}SKIP_AREAS=
#{p}GENERATORS=
#{p}JEV_ACT=
"#
    )
}

/// Replaces `@KEY@` placeholders in `template` with entries from `values` (ST1).
pub fn render_env(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in values {
        out = out.replace(&format!("@{k}@"), v);
    }
    out
}

fn is_upper_var_key(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Returns sorted uppercase variable names documented in `template` (active or `#KEY=` commented)
/// that do not appear anywhere in `env_text` as `<KEY>=` (ST3, TP3).
pub fn missing_variables(template: &str, env_text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for raw in template.lines() {
        let trimmed = raw.trim_start();
        let candidate = if let Some(rest) = trimmed.strip_prefix('#') {
            rest.trim_start()
        } else {
            trimmed
        };
        if let Some((k, _)) = candidate.split_once('=') {
            if is_upper_var_key(k) && !keys.contains(&k.to_string()) {
                keys.push(k.to_string());
            }
        }
    }
    keys.sort();
    keys.into_iter()
        .filter(|k| !env_text.contains(&format!("{k}=")))
        .collect()
}

/// Observed state of a path on disk before linking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkState {
    Missing,
    Symlink { target: String },
    OtherFile,
}

/// Pure decision for linking a binary, hook, or systemd unit (ST15, SU25).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkAction {
    Noop {
        link_path: String,
        why: String,
    },
    Create {
        link_path: String,
        target: String,
        why: String,
    },
    Repoint {
        link_path: String,
        old_target: String,
        target: String,
        why: String,
    },
    KeepNonLink {
        link_path: String,
        why: String,
    },
}

impl LinkAction {
    pub fn line(&self) -> String {
        match self {
            LinkAction::Noop { link_path, .. } => format!("ok      {link_path}"),
            LinkAction::Create {
                link_path, target, ..
            } => format!("linked  {link_path} -> {target}"),
            LinkAction::Repoint {
                link_path, target, ..
            } => format!("linked  {link_path} -> {target}"),
            LinkAction::KeepNonLink { link_path, .. } => {
                format!("keep    {link_path} (not a symlink)")
            }
        }
    }
}

/// Pure symlink decision: never clobbers a regular file (`OtherFile` -> `KeepNonLink`).
pub fn plan_link(target: &str, name: &str, directory: &str, state: &LinkState) -> LinkAction {
    let dir = directory.trim_end_matches('/');
    let link_path = format!("{dir}/{name}");
    match state {
        LinkState::Symlink { target: cur } if cur == target => LinkAction::Noop {
            link_path,
            why: "symlink already points to target".into(),
        },
        LinkState::Symlink { target: cur } => LinkAction::Repoint {
            link_path,
            old_target: cur.clone(),
            target: target.to_string(),
            why: format!("repoint symlink from {cur} to {target}"),
        },
        LinkState::Missing => LinkAction::Create {
            link_path,
            target: target.to_string(),
            why: format!("create symlink to {target}"),
        },
        LinkState::OtherFile => LinkAction::KeepNonLink {
            link_path,
            why: "existing path is not a symlink; refusing to overwrite".into(),
        },
    }
}

/// How a tool check accepts output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accept {
    ExitZero,
    Contains(String),
    Prefix(String),
}

impl Accept {
    pub fn matches(&self, exit_code: i32, output: &str) -> bool {
        if exit_code != 0 {
            return false;
        }
        match self {
            Accept::ExitZero => true,
            Accept::Contains(sub) => output.contains(sub.as_str()),
            Accept::Prefix(pre) => output.starts_with(pre.as_str()),
        }
    }
}

/// A required tool or environment check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Need {
    pub name: String,
    pub argv: Vec<String>,
    pub accept: Accept,
    pub fix: String,
}

/// Observed outcome of running a [`Need`] probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProbe {
    pub name: String,
    pub exit_code: i32,
    pub output: String,
}

/// Standard tool needs (ST6–ST8).
pub fn default_needs(psql: &str, gcloud: &str, android_home: &str) -> Vec<Need> {
    let n = |name: &str, argv: &[&str], accept: Accept, fix: &str| Need {
        name: name.into(),
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        accept,
        fix: fix.into(),
    };
    vec![
        n(
            "git",
            &["git", "--version"],
            Accept::ExitZero,
            "sudo apt install -y git",
        ),
        n(
            "gh",
            &["gh", "auth", "status"],
            Accept::ExitZero,
            "gh auth login",
        ),
        n(
            "docker",
            &["docker", "info", "--format", "{{.ServerVersion}}"],
            Accept::ExitZero,
            "install Docker Engine and add user to the docker group",
        ),
        n(
            "psql",
            &[psql, "--version"],
            Accept::ExitZero,
            "sudo apt install -y postgresql-client",
        ),
        n(
            "node v22",
            &["node", "--version"],
            Accept::Prefix("v22.".into()),
            "install Node.js v22 LTS",
        ),
        n(
            "rust 1.94+",
            &["rustc", "--version"],
            Accept::Contains("1.94".into()),
            "rustup update stable",
        ),
        n(
            "cargo-nextest",
            &["cargo", "nextest", "--version"],
            Accept::ExitZero,
            "cargo install cargo-nextest --locked",
        ),
        n(
            "sccache",
            &["sccache", "--version"],
            Accept::ExitZero,
            "cargo install sccache --locked",
        ),
        n(
            "jq",
            &["jq", "--version"],
            Accept::ExitZero,
            "sudo apt install -y jq",
        ),
        n(
            "ripgrep",
            &["rg", "--version"],
            Accept::ExitZero,
            "sudo apt install -y ripgrep",
        ),
        n(
            "gcloud",
            &[gcloud, "--version"],
            Accept::ExitZero,
            "install Google Cloud SDK",
        ),
        n(
            "android sdk",
            &["test", "-d", android_home],
            Accept::ExitZero,
            &format!("install Android SDK at {android_home}"),
        ),
        n(
            "claude",
            &["claude", "--version"],
            Accept::ExitZero,
            "install Claude Code CLI",
        ),
    ]
}

/// Advice lines for Docker `default-address-pools` based on `docker info` output (AP1–AP4, ST11).
pub fn address_pool_advice(info: &str, policy: &SetupPolicy) -> Vec<String> {
    if info.trim().is_empty() {
        return Vec::new();
    }
    let expected_base = format!("Base: {}", policy.pool_base);
    let expected_size = format!("Size: {}", policy.pool_size);
    if info.contains(&expected_base) && info.contains(&expected_size) {
        return vec![format!(
            "ok      docker address pool {} /{} ({} networks)",
            policy.pool_base, policy.pool_size, policy.pool_networks
        )];
    }
    vec![
        format!(
            "advice  docker bridge pool is still on Docker's 31-network default; raise to {} /{} ({} networks):",
            policy.pool_base, policy.pool_size, policy.pool_networks
        ),
        format!(
            r#"        In /etc/docker/daemon.json set: {{"default-address-pools": [{{"base": "{}", "size": {}}}]}}"#,
            policy.pool_base, policy.pool_size
        ),
        "        Wait until no batches are running, then run: sudo systemctl restart docker"
            .into(),
    ]
}

/// Advice lines if the repo and `bin_dir` live on different filesystems.
pub fn partition_advice(shown: &str, bin_dir: &str) -> Vec<String> {
    let lines: Vec<&str> = shown
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() >= 3 && lines[1] != lines[2] {
        vec![format!(
            "advice  repo and {bin_dir} are on different filesystems; keep them on the same partition where possible"
        )]
    } else {
        Vec::new()
    }
}

/// Self-hosted CI runner verification commands (ST12).
pub fn ci_runner_steps(repo: &str, repo_slug: &str) -> Vec<String> {
    vec![
        "systemctl list-units 'actions.runner.*'".into(),
        format!(
            "gh api repos/{repo_slug}/actions/runners --jq '.runners[] | {{name, status, busy}}'"
        ),
        format!("(requires admin scope on {repo_slug}; repo checkout at {repo})"),
    ]
}

/// Exact root cron line for weekly Sunday 04:00 docker buildx prune with `flock -n` (ST13, ST14).
pub fn root_prune_line(policy: &SetupPolicy) -> String {
    format!(
        "0 4 * * 0 /usr/bin/flock -n {} {} buildx prune -f --keep-storage 50GB --filter until=168h",
        policy.prune_lock, policy.prune_bin
    )
}

/// Advice lines for root-owned tasks (ST13, ST14).
pub fn root_advice(repo: &str, user: &str, policy: &SetupPolicy) -> Vec<String> {
    let cron = root_prune_line(policy);
    vec![
        format!("root    install weekly docker buildx prune cron: {cron}"),
        format!("root     verify git safe.directory for {repo} (user {user})"),
    ]
}

/// Directories created to hold warm caches and gradle state (ST9).
pub fn warm_directories(home: &str, warm: &str, sccache: &str, cache: &str) -> Vec<String> {
    let home = home.trim_end_matches('/');
    let mut dirs = vec![
        warm.to_string(),
        sccache.to_string(),
        cache.to_string(),
        format!("{home}/.gradle"),
    ];
    dirs.dedup();
    dirs
}

/// Everything observed by hands before running `plan_setup`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SetupInputs {
    pub home: String,
    pub repo: String,
    pub work: String,
    pub code: String,
    pub user: String,
    pub repo_slug: String,
    pub env_file_path: String,
    /// `None` if the `.env` file does not exist yet; `Some(contents)` if it exists.
    pub existing_env: Option<String>,
    pub template_override: Option<String>,
    /// Existing link state for each hook name in `~/.claude/hooks/<name>`.
    pub hook_states: BTreeMap<String, LinkState>,
    /// Existing link state for each binary in `~/.local/bin/<name>`.
    pub bin_states: BTreeMap<String, LinkState>,
    /// Existing link state for each systemd user unit in `~/.config/systemd/user/<name>`.
    pub unit_states: BTreeMap<String, LinkState>,
    /// Probes from running each `Need`.
    pub probes: Vec<ToolProbe>,
    pub docker_info: String,
    pub df_output: String,
    pub systemd_functional: bool,
    pub linger_enabled: bool,
    pub start_services: bool,
}

/// Pure setup plan returned by [`plan_setup`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupReport {
    pub wrote_env: Option<String>,
    pub missing_vars: Vec<String>,
    pub hook_actions: Vec<LinkAction>,
    pub bin_actions: Vec<LinkAction>,
    pub unit_actions: Vec<LinkAction>,
    pub warm_dirs: Vec<String>,
    pub failed_needs: Vec<Need>,
    pub systemd_commands: Vec<Vec<String>>,
    pub lines: Vec<String>,
    pub ok: bool,
}

/// Pure setup decision function (ST1–ST15, AP1–AP4, SU20).
pub fn plan_setup(inputs: &SetupInputs, cfg: &OpsConfig, policy: &SetupPolicy) -> SetupReport {
    let mut lines = Vec::new();
    let template = inputs
        .template_override
        .clone()
        .unwrap_or_else(|| default_env_template(&cfg.env_prefix));

    let mut placeholders = BTreeMap::new();
    placeholders.insert("HOME".into(), inputs.home.clone());
    placeholders.insert("REPO".into(), inputs.repo.clone());
    placeholders.insert("WORK".into(), inputs.work.clone());
    placeholders.insert("CODE".into(), inputs.code.clone());

    let (wrote_env, env_text) = match &inputs.existing_env {
        None => {
            let rendered = render_env(&template, &placeholders);
            lines.push(format!("wrote   {}", inputs.env_file_path));
            (Some(rendered.clone()), rendered)
        }
        Some(existing) => {
            lines.push(format!("keep    {} (already exists)", inputs.env_file_path));
            (None, existing.clone())
        }
    };

    let missing_vars = missing_variables(&template, &env_text);
    for var in &missing_vars {
        lines.push(format!(
            "note    {} omits {} (see .env.example)",
            inputs.env_file_path, var
        ));
    }

    let hooks_dir = format!("{}/.claude/hooks", inputs.home.trim_end_matches('/'));
    let mut hook_actions = Vec::new();
    for h in &policy.hooks {
        let target = format!("{}/hooks/{h}", inputs.repo.trim_end_matches('/'));
        let st = inputs
            .hook_states
            .get(h)
            .cloned()
            .unwrap_or(LinkState::Missing);
        let act = plan_link(&target, h, &hooks_dir, &st);
        lines.push(act.line());
        hook_actions.push(act);
    }

    let bin_dir = format!("{}/.local/bin", inputs.home.trim_end_matches('/'));
    let mut bin_actions = Vec::new();
    for b in &policy.linked_bins {
        let target = format!("{}/bin/{b}", inputs.repo.trim_end_matches('/'));
        let st = inputs
            .bin_states
            .get(b)
            .cloned()
            .unwrap_or(LinkState::Missing);
        let act = plan_link(&target, b, &bin_dir, &st);
        lines.push(act.line());
        bin_actions.push(act);
    }

    let unit_dir = format!("{}/.config/systemd/user", inputs.home.trim_end_matches('/'));
    let mut unit_actions = Vec::new();
    for u in &policy.units {
        let target = format!("{}/systemd/{u}", inputs.repo.trim_end_matches('/'));
        let st = inputs
            .unit_states
            .get(u)
            .cloned()
            .unwrap_or(LinkState::Missing);
        let act = plan_link(&target, u, &unit_dir, &st);
        lines.push(act.line());
        unit_actions.push(act);
    }

    let env_map = super::config::to_map(&super::config::parse_env_file(&env_text));
    let get_cfg = |suffix: &str, fallback: &str| -> String {
        env_map
            .get(&format!("{}{suffix}", cfg.env_prefix))
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };
    let psql = get_cfg("PSQL", "/usr/bin/psql");
    let gcloud = get_cfg("GCLOUD", "/usr/bin/gcloud");
    let android_home = get_cfg("ANDROID_HOME", "/opt/android-sdk");
    let warm = get_cfg(
        "WARM",
        &format!("{}/.cache/warm", inputs.home.trim_end_matches('/')),
    );
    let sccache = get_cfg(
        "SCCACHE",
        &format!("{}/.cache/sccache", inputs.home.trim_end_matches('/')),
    );
    let cache = get_cfg(
        "CACHE",
        &format!("{}/.cache/tacho", inputs.home.trim_end_matches('/')),
    );

    let warm_dirs = warm_directories(&inputs.home, &warm, &sccache, &cache);
    for d in &warm_dirs {
        lines.push(format!("mkdir   {d}"));
    }

    let needs = default_needs(&psql, &gcloud, &android_home);
    let mut failed_needs = Vec::new();
    for need in &needs {
        if let Some(probe) = inputs.probes.iter().find(|p| p.name == need.name) {
            if need.accept.matches(probe.exit_code, &probe.output) {
                lines.push(format!("ok      {}", need.name));
            } else {
                lines.push(format!("MISSING {} — fix: {}", need.name, need.fix));
                failed_needs.push(need.clone());
            }
        }
    }

    let mut systemd_commands = Vec::new();
    if inputs.systemd_functional {
        systemd_commands.push(vec![
            "systemctl".into(),
            "--user".into(),
            "daemon-reload".into(),
        ]);
        let mut enable_cmd = vec!["systemctl".into(), "--user".into(), "enable".into()];
        if inputs.start_services {
            enable_cmd.push("--now".into());
        }
        for u in &policy.units {
            enable_cmd.push(u.clone());
        }
        systemd_commands.push(enable_cmd);
        if !inputs.linger_enabled {
            lines.push(format!(
                "advice  enable linger so user services survive logout: sudo loginctl enable-linger {}",
                inputs.user
            ));
        }
    } else {
        lines.push("note    systemd --user is not running; linked unit files only".into());
    }

    lines.extend(address_pool_advice(&inputs.docker_info, policy));
    lines.extend(partition_advice(&inputs.df_output, &bin_dir));
    for step in ci_runner_steps(&inputs.repo, &inputs.repo_slug) {
        lines.push(format!("runner  {step}"));
    }
    lines.extend(root_advice(&inputs.repo, &inputs.user, policy));

    let ok = failed_needs.is_empty();
    SetupReport {
        wrote_env,
        missing_vars,
        hook_actions,
        bin_actions,
        unit_actions,
        warm_dirs,
        failed_needs,
        systemd_commands,
        lines,
        ok,
    }
}
