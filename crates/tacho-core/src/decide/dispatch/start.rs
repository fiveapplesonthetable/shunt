//! Starting a run: refusal checks, refuse-once, already-ran at head, provider/model selection,
//! and launch specification (R42–R58).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::brief::{
    brief, command_argv, has_sessions, provider_name, settings_json, with_session, with_settings,
};
use super::config::DispatchConfig;
use super::model::{
    Action, ActionKind, Base, BlockedRecord, Holds, PrRecord, PrState, PullFacts, RefusedRecord,
    Run, RunKey, RunStatus, Runs, Session, SessionMode, StartKind, StartOutcome, StartSpec,
};
use super::runs::lineage;

/// Why an agent task cannot start under the fleet's current ownership and slot rules (R42).
pub fn dispatch_refusal(
    key: &str,
    holds: &Holds,
    slots: i32,
    cfg: &DispatchConfig,
) -> Option<String> {
    let base = RunKey::parse(key).base;
    if let Some(h) = holds.get(&base) {
        return Some(format!("{base} is held: {}", h.why));
    }
    if slots <= 0 {
        return Some(format!(
            "automatic agent starts are paused because {} is 0",
            cfg.env("AGENT_SLOTS")
        ));
    }
    None
}

/// Why this process may not start agents (`<P>_LIVE` gate), or `None` when it may (R44).
pub fn missing_licence(env_live: bool, cfg: &DispatchConfig) -> Option<String> {
    if env_live {
        return None;
    }
    let live = cfg.env("LIVE");
    let tool = &cfg.tool;
    Some(format!(
        "{live} is not set (the installed {tool}.service predates it; `{tool} setup` reinstalls the unit)"
    ))
}

/// Update the blocked record when `reason` is met, keeping `since` and `mailed` while unchanged (R44).
pub fn noted_blocked(blocked: Option<&BlockedRecord>, reason: &str, now: i64) -> BlockedRecord {
    if let Some(b) = blocked {
        if b.reason == reason {
            return b.clone();
        }
    }
    BlockedRecord {
        reason: reason.to_string(),
        since: now,
        mailed: false,
    }
}

/// The red line for `status`; `""` when nothing blocks (R44).
pub fn blocked_line(blocked: Option<&BlockedRecord>, hh_mm: &str) -> String {
    match blocked {
        Some(b) => format!("daemon cannot start agents: {}, since {hh_mm}", b.reason),
        None => String::new(),
    }
}

/// Why a task for PR `pr` must not start, or `None` when it may (R45).
pub fn refusal(pr: u64, facts: &PullFacts) -> Option<String> {
    if facts.state != PrState::Open {
        return Some(format!("#{pr} is {}", facts.state.lower()));
    }
    if !facts.branch_exists {
        return Some(format!(
            "the branch {} of #{pr} no longer exists",
            facts.branch
        ));
    }
    if !facts.head_exists {
        let short = &facts.head[..facts.head.len().min(8)];
        return Some(format!("the head {short} of #{pr} no longer exists"));
    }
    None
}

/// True when a run of this task already started at this head (R49).
pub fn already_ran(key: &str, head: &str, runs: &Runs) -> bool {
    !head.is_empty()
        && lineage(&RunKey::parse(key).base, runs)
            .iter()
            .any(|(_, r)| r.head.as_deref() == Some(head))
}

/// Record that `key` was refused with `why`. Returns `true` the first time, `false` while the
/// same reason stands (R46).
pub fn refuse_once(
    key: &str,
    why: &str,
    refused: &mut BTreeMap<String, RefusedRecord>,
    now: i64,
) -> bool {
    if refused.get(key).is_some_and(|r| r.reason == why) {
        return false;
    }
    refused.insert(
        key.to_string(),
        RefusedRecord {
            reason: why.to_string(),
            at: now,
        },
    );
    true
}

/// Resolve a task's provider/model choice: `pinned && provider_override` > active `profile` >
/// `provider_override` > `configured` (R57).
///
/// Returns `(selected_provider, selected_model, command_template)`.
pub fn selection(
    profile: Option<&Session>,
    model: &str,
    provider_override: Option<&str>,
    pinned: bool,
    cfg: &DispatchConfig,
) -> (String, String, String) {
    let override_ = provider_override.filter(|s| !s.is_empty());
    let fallback_model = if model.is_empty() {
        cfg.providers.claude_default.as_str()
    } else {
        model
    };
    let (provider, selected_model) = match (pinned, override_) {
        (true, Some(ov)) => (ov.to_string(), fallback_model.to_string()),
        _ => {
            if let Some(prof) = profile {
                let p = if prof.provider.is_empty() {
                    "configured".to_string()
                } else {
                    prof.provider.clone()
                };
                let m = if prof.model.is_empty() {
                    fallback_model.to_string()
                } else {
                    prof.model.clone()
                };
                (p, m)
            } else {
                (
                    override_.unwrap_or("configured").to_string(),
                    fallback_model.to_string(),
                )
            }
        }
    };
    let command = match provider.as_str() {
        "codex" => cfg.codex_command.clone(),
        "claude" => cfg.claude_command.clone(),
        _ => cfg.command.clone(),
    };
    (provider, selected_model, command)
}

/// Input parameters for [`decide_start`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    pub key: String,
    pub task: String,
    pub model: String,
    pub dry_run: bool,
    pub area: String,
    pub worktree: String,
    pub base: Option<Base>,
    pub pr: Option<u64>,
    pub resume_session: String,
    pub provider_override: Option<String>,
    pub pinned: bool,
    pub kind: StartKind,
    pub new_session_id: Option<String>,
}

impl Default for StartRequest {
    fn default() -> Self {
        StartRequest {
            key: String::new(),
            task: String::new(),
            model: String::new(),
            dry_run: false,
            area: String::new(),
            worktree: String::new(),
            base: None,
            pr: None,
            resume_session: String::new(),
            provider_override: None,
            pinned: false,
            kind: StartKind::Direct,
            new_session_id: None,
        }
    }
}

/// Pure decision for starting a run (R42–R58).
#[allow(clippy::too_many_arguments)]
pub fn decide_start(
    req: &StartRequest,
    runs: &mut Runs,
    holds: &Holds,
    refused: &mut BTreeMap<String, RefusedRecord>,
    blocked: &mut Option<BlockedRecord>,
    profile: Option<&Session>,
    pull_facts: Option<&Result<PullFacts, String>>,
    env_live: bool,
    now: i64,
    cfg: &DispatchConfig,
) -> (StartOutcome, Vec<Action>) {
    let mut actions = Vec::new();
    let base_key = RunKey::parse(&req.key).base;
    if let Some(h) = holds.get(&base_key) {
        let why = format!("{base_key} is held: {}", h.why);
        return (StartOutcome::Held(why), actions);
    }
    if cfg.slots <= 0 {
        let why = format!(
            "automatic agent starts are paused because {} is 0",
            cfg.env("AGENT_SLOTS")
        );
        return (StartOutcome::Paused(why), actions);
    }
    if req.dry_run {
        if runs
            .get(&req.key)
            .is_some_and(|r| r.status == RunStatus::Running || r.task == req.task)
        {
            return (StartOutcome::AlreadyStarted, actions);
        }
        let (sel_prov, sel_model, cmd) = selection(
            profile,
            &req.model,
            req.provider_override.as_deref(),
            req.pinned,
            cfg,
        );
        let prov = provider_name(&cmd, &sel_prov);
        return (
            StartOutcome::WouldStart {
                provider: prov,
                model: sel_model,
                key: req.key.clone(),
            },
            actions,
        );
    }
    if let Some(why) = missing_licence(env_live, cfg) {
        let rec = noted_blocked(blocked.as_ref(), &why, now);
        if !rec.mailed {
            let line = format!("daemon cannot start agents: {why}, since {}", rec.since);
            actions.push(Action::new(
                ActionKind::Blocked {
                    subject: format!("{}: the daemon cannot start agents", cfg.tool),
                    body: line.clone(),
                },
                line,
            ));
        }
        *blocked = Some(rec);
        return (StartOutcome::Unlicensed(why), actions);
    }
    let mut head: Option<String> = None;
    if let Some(pr) = req.pr {
        match pull_facts {
            None => {
                let why = format!("could not check #{pr}: missing pull facts");
                return (StartOutcome::CannotCheck(why), actions);
            }
            Some(Err(err)) => {
                let why = format!("could not check #{pr}: {err}");
                return (StartOutcome::CannotCheck(why), actions);
            }
            Some(Ok(facts)) => {
                if let Some(why) = refusal(pr, facts) {
                    let first_time = refuse_once(&req.key, &why, refused, now);
                    if first_time {
                        actions.push(Action::new(
                            ActionKind::Refuse {
                                key: req.key.clone(),
                            },
                            format!("not starting {}: {why}", req.key),
                        ));
                    }
                    return (
                        StartOutcome::Refused {
                            reason: why,
                            first_time,
                        },
                        actions,
                    );
                }
                head = Some(facts.head.clone());
            }
        }
    }
    if runs
        .get(&req.key)
        .is_some_and(|r| r.status == RunStatus::Running || r.task == req.task)
    {
        return (StartOutcome::AlreadyStarted, actions);
    }
    if RunKey::parse(&req.key).attempt == 1
        && head
            .as_deref()
            .is_some_and(|h| already_ran(&req.key, h, runs))
    {
        return (StartOutcome::AlreadyRanAtHead, actions);
    }
    refused.remove(&req.key);
    *blocked = None;

    let (selected_provider, selected_model, command) = selection(
        profile,
        &req.model,
        req.provider_override.as_deref(),
        req.pinned,
        cfg,
    );
    let provider = provider_name(&command, &selected_provider);
    let capable = has_sessions(&command, cfg);
    let (session_mode, session_str, is_resume) = if !req.resume_session.is_empty() {
        (
            SessionMode::Resume(req.resume_session.clone()),
            req.resume_session.clone(),
            true,
        )
    } else if capable {
        let sid = req.new_session_id.clone().unwrap_or_else(|| {
            format!(
                "00000000-0000-4000-8000-{:012x}",
                now.unsigned_abs() ^ (req.key.len() as u64)
            )
        });
        (SessionMode::New(sid.clone()), sid, false)
    } else {
        (SessionMode::None, String::new(), false)
    };
    let prompt = if is_resume {
        req.task.clone()
    } else {
        brief(&req.task, &req.key, cfg)
    };
    let home = cfg.home.trim_end_matches('/');
    let settings_path = format!("{home}/agent-settings.json");
    let settings_content = settings_json(&cfg.hook_script);
    let argv = command_argv(&command, &selected_model, &prompt);
    let argv = with_settings(argv, &settings_path, cfg);
    let argv = with_session(argv, &session_str, is_resume);
    let settings = capable.then_some((settings_path, settings_content));
    let safe_key = req.key.replace('/', "-");
    let log = format!("{home}/agents/{safe_key}-{now}.log");
    let unit = format!("{}-agent-{safe_key}-{now}", cfg.tool);
    let env = vec![
        ("TMPDIR".to_string(), format!("{home}/tmp")),
        (cfg.env("AGENT_KEY"), base_key.clone()),
    ];
    let spec = StartSpec {
        key: req.key.clone(),
        base_key,
        kind: req.kind,
        brief: prompt.clone(),
        task: req.task.clone(),
        provider_selection: selected_provider.clone(),
        provider: provider.clone(),
        model: selected_model.clone(),
        pinned: req.pinned,
        session: session_mode,
        argv,
        settings,
        area: req.area.clone(),
        worktree: req.worktree.clone(),
        base: req.base.clone(),
        for_pr: req.pr,
        head: head.clone(),
        log: log.clone(),
        unit: unit.clone(),
        env,
    };
    let run = Run {
        status: RunStatus::Running,
        task: req.task.clone(),
        prompt,
        head,
        for_pr: req.pr,
        area: req.area.clone(),
        worktree: req.worktree.clone(),
        session: (!session_str.is_empty()).then_some(session_str),
        provider_selection: Some(selected_provider),
        provider: provider.clone(),
        model: selected_model.clone(),
        pinned: req.pinned,
        nudge: false,
        started: now,
        finished: None,
        summary: String::new(),
        pr: PrRecord::NotLooked,
        no_change: false,
        branches: Vec::new(),
        pid: None,
        process_start: None,
        unit,
        log,
    };
    runs.insert(req.key.clone(), run);
    let msg = format!(
        "started a {provider} {selected_model} agent for {}",
        req.key
    );
    actions.push(Action::new(ActionKind::Start(Box::new(spec.clone())), msg));
    (StartOutcome::Started(Box::new(spec)), actions)
}
