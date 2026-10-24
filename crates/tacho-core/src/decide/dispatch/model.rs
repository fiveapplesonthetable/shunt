//! Typed data model of the dispatch engine: run keys, run records, queue items, holds,
//! PR facts, and the actions the engine emits (each with its human reason).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use super::runs::RunKey;

/// The recorded status of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    #[default]
    Running,
    /// Ended with its final report.
    Done,
    /// Ended without its final report (died, timed out, or stopped by the supervisor).
    Unfinished,
    /// Stopped by a person; never resumed.
    Stopped,
    /// The supervisor gave up resuming it; a person decides.
    Abandoned,
    /// Retired (e.g. its PR closed while it was stuck).
    Retired,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Unfinished => "unfinished",
            RunStatus::Stopped => "stopped",
            RunStatus::Abandoned => "abandoned",
            RunStatus::Retired => "retired",
        }
    }
}

/// The state of a task across its lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Queued,
    Running,
    Done,
    Resuming,
    Stuck,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Done => "done",
            RunState::Resuming => "resuming",
            RunState::Stuck => "stuck",
        }
    }
}

/// How a PR was attributed to a run. Ordered by trust: `Attached > Stated > Guessed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrHow {
    Guessed,
    Stated,
    Attached,
}

impl PrHow {
    pub fn as_str(self) -> &'static str {
        match self {
            PrHow::Guessed => "guessed",
            PrHow::Stated => "stated",
            PrHow::Attached => "attached",
        }
    }
}

/// A PR attributed to a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundPr {
    pub number: u64,
    pub branch: String,
    pub how: PrHow,
}

/// What is known about a run's PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrRecord {
    /// Not looked up yet.
    #[default]
    NotLooked,
    /// Looked up, nothing found; `stated` when the agent wrote `PR: none`.
    NoPr {
        stated: bool,
    },
    Found(FoundPr),
}

/// Where a PR stands for dispatch purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PrStanding {
    #[default]
    Waiting,
    Passed,
    Merged,
    Closed,
}

impl PrStanding {
    pub fn as_str(self) -> &'static str {
        match self {
            PrStanding::Waiting => "waiting",
            PrStanding::Passed => "passed",
            PrStanding::Merged => "merged",
            PrStanding::Closed => "closed",
        }
    }
}

/// What a run builds on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    Main,
    Pr { number: u64, branch: String },
}

/// The area gate's verdict for one queue item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gate {
    Go { base: Base, skipped: Option<u64> },
    Wait { why: String, person: bool },
}

/// One recorded run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Run {
    pub status: RunStatus,
    /// The task argument it was launched with (before `brief()` wrapping).
    pub task: String,
    /// The full prompt handed to the provider command.
    #[serde(default)]
    pub prompt: String,
    /// PR head at launch, for PR tasks (`already_ran`).
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub for_pr: Option<u64>,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub worktree: String,
    #[serde(default)]
    pub session: Option<String>,
    /// The selection that chose the provider (`configured` or a provider name).
    #[serde(default)]
    pub provider_selection: Option<String>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub nudge: bool,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub finished: Option<i64>,
    /// The tail of its log (at most `summary_chars`), plus appended notes.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub pr: PrRecord,
    #[serde(default)]
    pub no_change: bool,
    /// Branches of its worktree (used to attribute the PR).
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub process_start: Option<String>,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub log: String,
}

/// Runs by full run key (`base` / `base~n`).
pub type Runs = BTreeMap<String, Run>;

/// The kind of a queue item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    #[default]
    Task,
    Hands,
    Fix,
    TrainFix,
    Rebase,
    FixCompile,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::Task => "task",
            ItemKind::Hands => "hands",
            ItemKind::Fix => "fix",
            ItemKind::TrainFix => "train-fix",
            ItemKind::Rebase => "rebase",
            ItemKind::FixCompile => "fix-compile",
        }
    }
}

/// A queued brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoItem {
    pub key: String,
    pub task: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub after: Vec<String>,
    /// Urgent: jumps the queue, ignores the lane and the slot limit.
    #[serde(default)]
    pub now: bool,
    #[serde(default)]
    pub kind: ItemKind,
    /// The PR the item works on, if any (checked before start).
    #[serde(default)]
    pub pr: Option<u64>,
    /// An explicit provider pins the item's provider.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// A hold with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub why: String,
    pub since: i64,
}

pub type Holds = BTreeMap<String, Hold>;

/// Record of an explicitly retired queue item (`retire`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredRecord {
    pub why: String,
    pub at: i64,
    pub task: String,
    pub held_for: String,
}

/// An archived queue item in `task_history` (from `compact`, `withdraw`, or `retire_closed_afters`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub item: TodoItem,
    pub status: RunStatus,
    pub finished: Option<i64>,
    #[serde(default)]
    pub why: String,
}

/// A rebase that needs hands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HandsRec {
    pub sha: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub parent: Option<u64>,
    #[serde(default)]
    pub sibling: bool,
}

/// Forge state of a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    #[default]
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn lower(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }
}

/// Facts checked before a PR task starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullFacts {
    pub state: PrState,
    pub head: String,
    pub branch: String,
    pub branch_exists: bool,
    pub head_exists: bool,
}

/// GitHub's mergeability verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    #[default]
    Unknown,
}

/// An open PR as listed by the forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OpenPr {
    pub number: u64,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub mergeable: Mergeable,
}

/// A PR as seen when attributing a run's PR (`find_pr`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgePr {
    pub number: u64,
    pub branch: String,
    pub owner: String,
    pub state: PrState,
    pub updated: i64,
}

/// Landing trains as seen by dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrainView {
    /// Members of open trains and of train PRs neither landed nor abandoned.
    pub in_flight: BTreeSet<u64>,
    /// Members of landed train PRs.
    pub landed: BTreeSet<u64>,
    /// The train PR numbers themselves.
    pub train_prs: BTreeSet<u64>,
}

/// The owner's active session profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub provider: String,
    pub model: String,
}

/// Known worktree record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeRec {
    pub path: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
}

/// Where a run left its worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Stopped {
    pub path: String,
    pub commits: Vec<String>,
    pub changes: Vec<String>,
}

/// A task whose runs are used up without a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stuck {
    pub key: String,
    pub runs: u32,
    pub since: i64,
    pub tail: Vec<String>,
    pub worktree: String,
}

/// Why a task was refused at dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedRecord {
    pub reason: String,
    pub at: i64,
}

/// Why the daemon is blocked from starting agents (e.g. missing live licence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedRecord {
    pub reason: String,
    pub since: i64,
    pub mailed: bool,
}

/// A code failure on a PR's batch (`fixes.py`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FailureRec {
    pub sha: String,
    pub why: String,
    #[serde(default)]
    pub areas: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub log: String,
}

/// Record of the one fix-agent attempt for a PR (`fixes.py`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixAttemptRec {
    pub sha: String,
    pub told: bool,
}

/// Step for a failing PR's fix agent (`fixes.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FixStep {
    Start,
    Wait,
    Tell,
    Done,
}

/// How the provider session is handled on start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// The provider has no sessions.
    None,
    /// Start a new session with this id.
    New(String),
    /// Resume this session.
    Resume(String),
}

impl SessionMode {
    pub fn id(&self) -> Option<&str> {
        match self {
            SessionMode::None => None,
            SessionMode::New(s) | SessionMode::Resume(s) => Some(s.as_str()),
        }
    }
}

/// Why a start was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartKind {
    Urgent,
    Hands,
    Queued,
    Resume,
    Fix,
    Direct,
}

/// Everything the hands need to launch one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSpec {
    pub key: String,
    pub base_key: String,
    pub kind: StartKind,
    /// The full prompt handed to the provider.
    pub brief: String,
    /// The task text alone (recorded for resume/identity).
    pub task: String,
    pub provider_selection: String,
    pub provider: String,
    pub model: String,
    pub pinned: bool,
    pub session: SessionMode,
    pub argv: Vec<String>,
    pub settings: Option<(String, String)>,
    pub area: String,
    pub worktree: String,
    pub base: Option<Base>,
    pub for_pr: Option<u64>,
    pub head: Option<String>,
    pub log: String,
    pub unit: String,
    /// Environment the hands add (agent key, private TMPDIR).
    pub env: Vec<(String, String)>,
}

/// The typed outcome of trying to start a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartOutcome {
    Started(Box<StartSpec>),
    /// Dry-run preview (`would start a <provider> <model> agent for <key>`).
    WouldStart {
        provider: String,
        model: String,
        key: String,
    },
    /// Running, or known with the identical task.
    AlreadyStarted,
    /// A run of this lineage already started at this PR head.
    AlreadyRanAtHead,
    /// The PR check refused it; `first_time` is false while the same reason stands.
    Refused {
        reason: String,
        first_time: bool,
    },
    /// Slots are 0.
    Paused(String),
    /// The base key is held.
    Held(String),
    /// The PR could not be checked (not recorded as refused).
    CannotCheck(String),
    /// The process has no live licence.
    Unlicensed(String),
}

impl StartOutcome {
    /// Render the reference string return of `agents.start`.
    pub fn message(&self, key: &str) -> String {
        match self {
            StartOutcome::Started(spec) => {
                format!(
                    "started a {} {} agent for {}",
                    spec.provider, spec.model, spec.key
                )
            }
            StartOutcome::WouldStart {
                provider,
                model,
                key,
            } => {
                format!("would start a {provider} {model} agent for {key}")
            }
            StartOutcome::AlreadyStarted | StartOutcome::AlreadyRanAtHead => String::new(),
            StartOutcome::Refused { reason, first_time } => {
                if *first_time {
                    format!("not starting {key}: {reason}")
                } else {
                    String::new()
                }
            }
            StartOutcome::Paused(why) | StartOutcome::Held(why) => {
                format!("not starting {key}: {why}")
            }
            StartOutcome::CannotCheck(why) => format!("not starting {key}: {why}"),
            StartOutcome::Unlicensed(why) => format!("not starting an agent for {key}: {why}"),
        }
    }
}

/// One thing the hands should do, with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    pub reason: String,
}

/// The actions the dispatch engine emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Start(Box<StartSpec>),
    /// Said once while the reason stands.
    Refuse {
        key: String,
    },
    /// Queue items for a refused PR leave the queue.
    Withdraw {
        key: String,
        pr: u64,
    },
    Abandon {
        key: String,
    },
    RetireClosedAfter {
        key: String,
        pr: u64,
    },
    /// A stuck task whose PR closed: retired silently (one event).
    RetireStuck {
        key: String,
        pr: u64,
    },
    /// A stuck task or chain decision: mail the owner once with this subject and body.
    MailOwner {
        key: String,
        subject: String,
        body: String,
    },
    /// A stuck task: comment on the owner issue once with this text.
    CommentIssue {
        issue: u64,
        key: String,
        body: String,
    },
    /// A line of news/event.
    Announce {
        news: bool,
    },
    /// The licence gate blocked a start; notify once.
    Blocked {
        subject: String,
        body: String,
    },
    /// Stop the scope unit / process group of a run.
    StopUnit {
        key: String,
        unit: Option<String>,
        pid: Option<u32>,
    },
}

impl Action {
    pub fn new(kind: ActionKind, reason: impl Into<String>) -> Self {
        Action {
            kind,
            reason: reason.into(),
        }
    }
}
