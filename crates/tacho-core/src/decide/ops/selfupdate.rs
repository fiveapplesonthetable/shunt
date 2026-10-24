//! Self-update and version reporting: target selection, behind-distance text, the rollback state
//! machine, version output, and area coverage rules.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{short, OpsConfig};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdatePolicy {
    /// Seconds before behind commits raise an attention line (default 30 min).
    pub stale_after: i64,
    /// Newest behind commits whose CI is inspected in one pass.
    pub candidates: usize,
    /// Optional path prefix that identifies commits touching the tool.
    pub tool_path_prefix: String,
    /// The systemd service unit whose drift requires a service restart instead of re-exec.
    pub service_unit: String,
}

impl Default for SelfUpdatePolicy {
    fn default() -> Self {
        Self {
            stale_after: 30 * 60,
            candidates: 5,
            tool_path_prefix: "crates/tacho".into(),
            service_unit: "tacho.service".into(),
        }
    }
}

/// A commit on `main` not yet in the running checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Behind {
    pub sha: String,
    pub at: i64,
    pub touches_tool: bool,
    pub ci_green: bool,
}

impl Behind {
    pub fn new(sha: impl Into<String>, at: i64) -> Self {
        Self {
            sha: sha.into(),
            at,
            touches_tool: true,
            ci_green: false,
        }
    }

    pub fn green(sha: impl Into<String>, at: i64) -> Self {
        Self {
            sha: sha.into(),
            at,
            touches_tool: true,
            ci_green: true,
        }
    }

    pub fn untouching(sha: impl Into<String>, at: i64, ci_green: bool) -> Self {
        Self {
            sha: sha.into(),
            at,
            touches_tool: false,
            ci_green,
        }
    }
}

/// Record of the last successful self-update move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMoved {
    pub from: String,
    pub to: String,
    pub at: i64,
}

/// The canonical rule sentence shown in `--help`, `version`, `setup`, docs and worker briefs.
pub fn rule_sentence(tool: &str, env_prefix: &str) -> String {
    format!(
        "{tool} runs from the checkout named by {env_prefix}CODE, which the daemon moves to main when main's ci is green; `{tool} version` says what runs and how far behind it is."
    )
}

fn tool_commits(behind: &[Behind]) -> Vec<&Behind> {
    behind.iter().filter(|c| c.touches_tool).collect()
}

/// The newest green commit among the first `policy.candidates` tool-touching behind commits
/// (newest first), or `None` if none is green or the chosen target matches `refused`.
pub fn target(
    behind: &[Behind],
    refused: Option<&str>,
    policy: &SelfUpdatePolicy,
) -> Option<String> {
    let sha = tool_commits(behind)
        .into_iter()
        .take(policy.candidates)
        .find(|c| c.ci_green)
        .map(|c| c.sha.clone())?;
    if refused == Some(sha.as_str()) {
        return None;
    }
    Some(sha)
}

/// Whether the oldest tool-touching behind commit is older than `policy.stale_after`.
pub fn stale(behind: &[Behind], now: i64, policy: &SelfUpdatePolicy) -> bool {
    let commits = tool_commits(behind);
    commits
        .iter()
        .map(|c| c.at)
        .min()
        .is_some_and(|oldest| now - oldest > policy.stale_after)
}

/// `up to date with main` or `behind main by N <tool> commit[s], the oldest M min old`.
pub fn distance(behind: &[Behind], now: i64, tool: &str) -> String {
    let commits = tool_commits(behind);
    let Some(min_at) = commits.iter().map(|c| c.at).min() else {
        return "up to date with main".into();
    };
    let oldest = ((now - min_at) / 60).max(0);
    let n = commits.len();
    let s = if n == 1 { "" } else { "s" };
    format!("behind main by {n} {tool} commit{s}, the oldest {oldest} min old")
}

/// One-line stderr warning when the running checkout is behind `main`.
pub fn warning(behind: &[Behind], now: i64, tool: &str) -> Option<String> {
    let commits = tool_commits(behind);
    if commits.is_empty() {
        return None;
    }
    Some(format!(
        "{tool} is {}; the daemon moves it when main's ci is green. See `{tool} version`.",
        distance(behind, now, tool)
    ))
}

/// Attention text when the code has been behind `main` for over `policy.stale_after`.
pub fn attention(
    behind: &[Behind],
    now: i64,
    tool: &str,
    policy: &SelfUpdatePolicy,
) -> Option<String> {
    if !stale(behind, now, policy) {
        return None;
    }
    Some(format!(
        "{tool} code is {} (over {} min)",
        distance(behind, now, tool),
        policy.stale_after / 60
    ))
}

/// Every CLI command except `run-batch`, `summary-line` and `shrink-warm` prints the behind warning.
pub fn should_warn_cli(cmd: &str) -> bool {
    !matches!(cmd, "run-batch" | "summary-line" | "shrink-warm")
}

/// Pure state machine for self-update with rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Upd {
    #[default]
    Idle,
    Testing {
        from: String,
        to: String,
    },
    Rolled {
        refused: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdEvent {
    Start {
        detached: bool,
        head: String,
        behind: Vec<Behind>,
        refused: Option<String>,
    },
    Moved {
        ok: bool,
    },
    Tested {
        ok: bool,
        output: String,
        drifted_units: Vec<String>,
        now: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdEffect {
    Move {
        to: String,
        reason: String,
    },
    SelfTest {
        sha: String,
        reason: String,
    },
    Refuse {
        sha: String,
        reason: String,
    },
    Tell {
        issue: u64,
        body: String,
        reason: String,
    },
    RecordMoved {
        moved: CodeMoved,
        reason: String,
    },
    InstallUnits {
        units: Vec<String>,
        reason: String,
    },
    Restart {
        service_changed: bool,
        reason: String,
    },
}

/// Advance the self-update state machine by one event.
pub fn step(state: Upd, event: UpdEvent, cfg: &OpsConfig) -> (Upd, Vec<UpdEffect>) {
    let refused_from_state = match &state {
        Upd::Rolled { refused } => Some(refused.clone()),
        _ => None,
    };
    match (state, event) {
        (
            st,
            UpdEvent::Start {
                detached,
                head,
                behind,
                refused,
            },
        ) => {
            if !detached {
                return (st, vec![]);
            }
            let effective_refused = refused.as_deref().or(refused_from_state.as_deref());
            let Some(to) = target(&behind, effective_refused, &cfg.selfupdate) else {
                return (st, vec![]);
            };
            if to == head {
                return (st, vec![]);
            }
            (
                Upd::Testing {
                    from: head,
                    to: to.clone(),
                },
                vec![UpdEffect::Move {
                    to: to.clone(),
                    reason: format!("newest green {} commit on main is {}", cfg.tool, short(&to)),
                }],
            )
        }
        (Upd::Testing { from, to }, UpdEvent::Moved { ok }) => {
            if !ok {
                return (Upd::Idle, vec![]);
            }
            (
                Upd::Testing {
                    from,
                    to: to.clone(),
                },
                vec![UpdEffect::SelfTest {
                    sha: to.clone(),
                    reason: format!("verify {}'s own tests on {}", cfg.tool, short(&to)),
                }],
            )
        }
        (
            Upd::Testing { from, to },
            UpdEvent::Tested {
                ok,
                output,
                drifted_units,
                now,
            },
        ) => {
            if !ok {
                let body = format!(
                    "{} stays on {}: its own tests fail on {}.\n\n```\n{output}\n```",
                    cfg.tool,
                    short(&from),
                    short(&to)
                );
                return (
                    Upd::Rolled {
                        refused: to.clone(),
                    },
                    vec![
                        UpdEffect::Move {
                            to: from.clone(),
                            reason: format!(
                                "roll back to {} after self-test failure on {}",
                                short(&from),
                                short(&to)
                            ),
                        },
                        UpdEffect::Refuse {
                            sha: to.clone(),
                            reason: format!("self-test failed on {}", short(&to)),
                        },
                        UpdEffect::Tell {
                            issue: cfg.owner_issue,
                            body,
                            reason: format!("report failing self-test on {} once", short(&to)),
                        },
                    ],
                );
            }
            let service_changed = drifted_units.contains(&cfg.selfupdate.service_unit);
            let mut effects = vec![UpdEffect::RecordMoved {
                moved: CodeMoved {
                    from: from.clone(),
                    to: to.clone(),
                    at: now,
                },
                reason: format!("self-test passed on {}", short(&to)),
            }];
            if !drifted_units.is_empty() {
                effects.push(UpdEffect::InstallUnits {
                    units: drifted_units,
                    reason: "reinstall drifted systemd units before restart".into(),
                });
            }
            effects.push(UpdEffect::Restart {
                service_changed,
                reason: if service_changed {
                    format!(
                        "{} changed; systemd must restart the service",
                        cfg.selfupdate.service_unit
                    )
                } else {
                    format!("re-exec daemon on {}", short(&to))
                },
            });
            (Upd::Idle, effects)
        }
        (st, _) => (st, vec![]),
    }
}

/// Complete single-pass input for `advance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvanceInput {
    pub detached: bool,
    pub head: String,
    pub behind: Vec<Behind>,
    pub refused: Option<String>,
    pub move_ok: bool,
    pub self_test: (bool, String),
    pub drifted_units: Vec<String>,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvanceOutcome {
    pub effects: Vec<UpdEffect>,
    pub head_after: String,
    pub refused_after: Option<String>,
    pub code_moved: Option<CodeMoved>,
    pub summary: String,
}

/// Run a complete `advance` pass over observed checkout facts.
pub fn advance(input: &AdvanceInput, cfg: &OpsConfig) -> AdvanceOutcome {
    let (st, mut effects) = step(
        Upd::Idle,
        UpdEvent::Start {
            detached: input.detached,
            head: input.head.clone(),
            behind: input.behind.clone(),
            refused: input.refused.clone(),
        },
        cfg,
    );
    let Upd::Testing { from, to } = st.clone() else {
        return AdvanceOutcome {
            effects,
            head_after: input.head.clone(),
            refused_after: input.refused.clone(),
            code_moved: None,
            summary: String::new(),
        };
    };
    let (st, move_fx) = step(st, UpdEvent::Moved { ok: input.move_ok }, cfg);
    effects.extend(move_fx);
    if !input.move_ok {
        return AdvanceOutcome {
            effects,
            head_after: from,
            refused_after: input.refused.clone(),
            code_moved: None,
            summary: String::new(),
        };
    }
    let (_, test_fx) = step(
        st,
        UpdEvent::Tested {
            ok: input.self_test.0,
            output: input.self_test.1.clone(),
            drifted_units: input.drifted_units.clone(),
            now: input.now,
        },
        cfg,
    );
    let mut code_moved = None;
    for fx in &test_fx {
        if let UpdEffect::RecordMoved { moved, .. } = fx {
            code_moved = Some(moved.clone());
        }
    }
    effects.extend(test_fx);
    if input.self_test.0 {
        AdvanceOutcome {
            effects,
            head_after: to.clone(),
            refused_after: input.refused.clone(),
            code_moved,
            summary: format!("moved from {} to {}", short(&from), short(&to)),
        }
    } else {
        AdvanceOutcome {
            effects,
            head_after: from,
            refused_after: Some(to.clone()),
            code_moved: None,
            summary: format!(
                "not moving to {}: {}'s tests fail there",
                short(&to),
                cfg.tool
            ),
        }
    }
}

/// Format a unix timestamp as `YYYY-MM-DD HH:MM` UTC.
pub fn format_utc_hm(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Render the output of `<tool> version`.
#[allow(clippy::too_many_arguments)]
pub fn version_text(
    head: &str,
    code_path: &str,
    main_sha: &str,
    behind: &[Behind],
    moved: Option<&CodeMoved>,
    reinstalled_units: &[String],
    now: i64,
    cfg: &OpsConfig,
) -> String {
    let last_move = match moved {
        Some(m) => format!(
            "{} from {} to {}",
            format_utc_hm(m.at),
            short(&m.from),
            short(&m.to)
        ),
        None => "none recorded".into(),
    };
    let units_line = if reinstalled_units.is_empty() {
        "units: match bin/".into()
    } else {
        let mut s = format!(
            "units: {} differed from bin/ and were reinstalled",
            reinstalled_units.join(", ")
        );
        if reinstalled_units.contains(&cfg.selfupdate.service_unit) {
            s.push_str(&format!(
                "; restart to apply: systemctl --user restart {}",
                cfg.selfupdate.service_unit
            ));
        }
        s
    };
    [
        format!("running {} from {code_path}", short(head)),
        format!(
            "main is {}; {}",
            short(main_sha),
            distance(behind, now, &cfg.tool)
        ),
        format!("last move: {last_move}"),
        rule_sentence(&cfg.tool, &cfg.env_prefix),
        units_line,
    ]
    .join("\n")
}

/// Area coverage rules for PRs touching screenshot reports or slow areas (SU21–SU24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaCheckPolicy {
    pub gate: String,
    pub postsubmit_only: BTreeSet<String>,
    pub pixel_patterns: Vec<String>,
    pub pixel_areas: BTreeSet<String>,
    pub tla_area: String,
    pub area_inputs: BTreeMap<String, Vec<String>>,
    pub train_inputs: BTreeMap<String, Vec<String>>,
}

impl Default for AreaCheckPolicy {
    fn default() -> Self {
        let pixel = vec![
            "golden/".into(),
            "goldens/".into(),
            "screenshots/".into(),
            "scripts/shot-report".into(),
            ".github/workflows/shots.yaml".into(),
        ];
        let mut area_inputs = BTreeMap::new();
        area_inputs.insert(
            "console".into(),
            vec![
                "frontend/admin/".into(),
                "scripts/shot".into(),
                ".github/workflows/shots.yaml".into(),
                "screenshots".into(),
                "golden".into(),
            ],
        );
        area_inputs.insert(
            "android".into(),
            vec![
                "frontend/android/".into(),
                "scripts/shot-report".into(),
                ".github/workflows/shots.yaml".into(),
                "screenshots".into(),
                "golden".into(),
            ],
        );
        area_inputs.insert(
            "fuzz".into(),
            vec!["crates/".into(), "fuzz/".into(), "Cargo.".into()],
        );
        let mut train_inputs = BTreeMap::new();
        train_inputs.insert(
            "console".into(),
            vec!["frontend/admin/".into(), "scripts/contract.sh".into()],
        );
        train_inputs.insert(
            "android".into(),
            vec!["frontend/android/".into(), "scripts/contract.sh".into()],
        );
        Self {
            gate: "compile".into(),
            postsubmit_only: BTreeSet::from(["formal".to_string()]),
            pixel_patterns: pixel,
            pixel_areas: BTreeSet::from(["console".to_string(), "android".to_string()]),
            tla_area: "formal".into(),
            area_inputs,
            train_inputs,
        }
    }
}

/// Required check areas before readiness, given selected areas, touched files, and skipped areas.
pub fn check_areas(
    selected: &BTreeSet<String>,
    files: &[String],
    skipped: &BTreeSet<String>,
    train: bool,
    policy: &AreaCheckPolicy,
) -> BTreeSet<String> {
    let mut required: BTreeSet<String> = selected
        .difference(skipped)
        .filter(|a| !policy.postsubmit_only.contains(*a))
        .cloned()
        .collect();
    if train {
        required.insert(policy.gate.clone());
    }
    let touches_pixels = files
        .iter()
        .any(|f| policy.pixel_patterns.iter().any(|p| f.contains(p.as_str())));
    if !train && touches_pixels {
        for a in policy.pixel_areas.difference(skipped) {
            required.insert(a.clone());
        }
    }
    if !train && !skipped.contains(&policy.tla_area) && files.iter().any(|f| f.ends_with(".tla")) {
        required.insert(policy.tla_area.clone());
    }
    let inputs = if train {
        &policy.train_inputs
    } else {
        &policy.area_inputs
    };
    for (area, patterns) in inputs {
        if !files
            .iter()
            .any(|f| patterns.iter().any(|p| f.contains(p.as_str())))
        {
            required.remove(area);
        }
    }
    required
}

/// Whether `checked` covers all of `touched` minus `skipped`.
pub fn covered(touched: &BTreeSet<String>, checked: &[String], skipped: &BTreeSet<String>) -> bool {
    let checked_set: BTreeSet<&str> = checked.iter().map(String::as_str).collect();
    touched
        .difference(skipped)
        .all(|a| checked_set.contains(a.as_str()))
}
