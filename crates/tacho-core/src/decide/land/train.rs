//! The one `Train` aggregate (both landing paths) and its state machine:
//! `Open -> Landing -> Landed | Stopped | Mismatch`, and `Open -> Invalidated`.

use super::config::short;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A git object id (commit or tree), hex.
pub type Sha = String;

/// A PR pinned at the head that was verified.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Member {
    pub pr: u64,
    pub head: Sha,
}

impl Member {
    pub fn new(pr: u64, head: impl Into<Sha>) -> Self {
        Self {
            pr,
            head: head.into(),
        }
    }
}

/// How a train lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrainKind {
    /// Path A: the members are merged one by one, in order, by `<tool> land <id>`.
    Sequential,
    /// Path B: the members were assembled into one PR the owner merges with a pinned command.
    Combined { pr: u64, branch: String },
}

/// Where a train is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrainStatus {
    Open,
    /// Merging member `next` (index into `members`). `attempts` counts failed tries of it.
    Landing {
        next: usize,
        attempts: u32,
    },
    /// Merged; now waiting for main's tree to compare (Sequential) or done (Combined).
    VerifyingTree,
    Landed,
    Stopped {
        why: String,
    },
    Mismatch {
        why: String,
    },
    Invalidated {
        why: String,
    },
    /// A combined train PR closed without merging; its members are released.
    Abandoned,
}

impl TrainStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Landing { .. } | Self::VerifyingTree => "landing",
            Self::Landed => "landed",
            Self::Stopped { .. } => "stopped",
            Self::Mismatch { .. } => "mismatch",
            Self::Invalidated { .. } => "invalidated",
            Self::Abandoned => "abandoned",
        }
    }
    pub fn why(&self) -> &str {
        match self {
            Self::Stopped { why } | Self::Mismatch { why } | Self::Invalidated { why } => why,
            _ => "",
        }
    }
    pub fn is_landing(&self) -> bool {
        matches!(self, Self::Landing { .. } | Self::VerifyingTree)
    }
}

/// Outbound owner mail for a combined train, as an event-sourced two-phase commit.
/// `Intent` with no outcome means "uncertain": never re-sent automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MailState {
    #[default]
    NotSent,
    Intent {
        head: Sha,
    },
    Sent {
        head: Sha,
    },
    /// A send that reported failure; it may be retried.
    Failed {
        head: Sha,
        err: String,
    },
}

/// Content-derived train id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct TrainId(pub String);

impl fmt::Display for TrainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The train aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Train {
    pub id: TrainId,
    pub kind: TrainKind,
    /// Main the train was verified on (or for Combined, the train PR's current head SHA).
    pub base: Sha,
    pub members: Vec<Member>,
    /// The verified merged tree.
    pub tree: Sha,
    pub status: TrainStatus,
    #[serde(default)]
    pub left_out: Vec<(u64, Vec<String>)>,
    #[serde(default)]
    pub dropped: Vec<(u64, String)>,
    /// Whether the assembled combined tree compiled cleanly.
    #[serde(default = "default_compiled")]
    pub compiled: bool,
    /// A train that changes how checks run (travels alone, carries the CI notice).
    #[serde(default)]
    pub ci: bool,
    /// Combined: the head the owner released for mail.
    #[serde(default)]
    pub reviewed: Option<Sha>,
    #[serde(default)]
    pub mail: MailState,
    /// Combined: the member heads the train was assembled from.
    #[serde(default)]
    pub source_heads: BTreeMap<u64, Sha>,
    /// Combined: the train PR's red verdict sha that fix tasks were queued for.
    #[serde(default)]
    pub fixed_for: Option<Sha>,
    /// Members merged so far while landing.
    #[serde(default)]
    pub merged: Vec<u64>,
}

fn default_compiled() -> bool {
    true
}

/// `blake3(" ".join([base, tree, *"n@sha"]))[..8]`: same inputs, same id.
pub fn train_id(base: &str, tree: &str, members: &[Member]) -> TrainId {
    let mut parts = vec![base.to_string(), tree.to_string()];
    parts.extend(members.iter().map(|m| format!("{}@{}", m.pr, m.head)));
    let hex = blake3::hash(parts.join(" ").as_bytes()).to_hex();
    TrainId(hex[..8].to_string())
}

impl Train {
    /// A new open sequential train.
    pub fn make(base: impl Into<Sha>, members: Vec<Member>, tree: impl Into<Sha>) -> Self {
        let (base, tree) = (base.into(), tree.into());
        Self {
            id: train_id(&base, &tree, &members),
            kind: TrainKind::Sequential,
            base,
            members,
            tree,
            status: TrainStatus::Open,
            left_out: Vec::new(),
            dropped: Vec::new(),
            compiled: true,
            ci: false,
            reviewed: None,
            mail: MailState::NotSent,
            source_heads: BTreeMap::new(),
            fixed_for: None,
            merged: Vec::new(),
        }
    }

    /// A newly opened combined train PR (`train-<date>-<n>`).
    pub fn make_combined(
        pr: u64,
        branch: impl Into<String>,
        head: impl Into<Sha>,
        members: Vec<Member>,
        left_out: Vec<(u64, Vec<String>)>,
        dropped: Vec<(u64, String)>,
        ci: bool,
    ) -> Self {
        let head = head.into();
        let branch = branch.into();
        let source_heads = members.iter().map(|m| (m.pr, m.head.clone())).collect();
        Self {
            id: TrainId(format!("pr-{pr}")),
            kind: TrainKind::Combined { pr, branch },
            base: head.clone(),
            members,
            tree: head,
            status: TrainStatus::Open,
            left_out,
            dropped,
            compiled: true,
            ci,
            reviewed: None,
            mail: MailState::NotSent,
            source_heads,
            fixed_for: None,
            merged: Vec::new(),
        }
    }

    pub fn numbers(&self) -> Vec<u64> {
        self.members.iter().map(|m| m.pr).collect()
    }

    pub fn combined_pr(&self) -> Option<u64> {
        match self.kind {
            TrainKind::Combined { pr, .. } => Some(pr),
            TrainKind::Sequential => None,
        }
    }

    pub fn combined_branch(&self) -> Option<&str> {
        match &self.kind {
            TrainKind::Combined { branch, .. } => Some(branch.as_str()),
            TrainKind::Sequential => None,
        }
    }
}

/// What to record for a verified member list: an existing open train with the same base and
/// members is reused; otherwise a new train on `tree`. `tree == None` (some member did not
/// merge) records nothing.
pub fn reuse_or_make(
    trains: &[Train],
    base: &str,
    members: &[Member],
    tree: Option<&str>,
) -> Option<Train> {
    if let Some(t) = trains
        .iter()
        .find(|t| t.status == TrainStatus::Open && t.base == base && t.members == members)
    {
        return Some(t.clone());
    }
    let tree = tree.filter(|t| !t.is_empty())?;
    Some(Train::make(base, members.to_vec(), tree))
}

fn invalidate(t: &mut Train, why: String) {
    if t.status == TrainStatus::Open && matches!(t.kind, TrainKind::Sequential) {
        t.status = TrainStatus::Invalidated { why };
    }
}

/// Adds `train`; every other open sequential train is superseded, so only one command is ever current.
pub fn with_train(trains: &[Train], train: Train) -> Vec<Train> {
    let mut out: Vec<Train> = trains
        .iter()
        .filter(|t| t.id != train.id)
        .cloned()
        .collect();
    for t in &mut out {
        invalidate(t, format!("superseded by {}", train.id));
    }
    out.push(train);
    out
}

/// The open train merging exactly `numbers`, in order.
pub fn open_for<'a>(trains: &'a [Train], numbers: &[u64]) -> Option<&'a Train> {
    trains
        .iter()
        .find(|t| t.status == TrainStatus::Open && t.numbers() == numbers)
}

/// The id of the open train merging `numbers` in order, or `""` when none matches.
pub fn open_for_id(trains: &[Train], numbers: &[u64]) -> String {
    open_for(trains, numbers)
        .map(|t| t.id.0.clone())
        .unwrap_or_default()
}

/// Why a train may not land now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoTrain {
        id: String,
    },
    NotOpen {
        id: TrainId,
        status: &'static str,
        why: String,
    },
    MainMoved {
        main: Sha,
        base: Sha,
    },
    HeadsMoved {
        prs: Vec<u64>,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrain { id } => write!(f, "no train {id}"),
            Self::NotOpen { id, status, why } if why.is_empty() => {
                write!(f, "train {id} is {status}")
            }
            Self::NotOpen { id, status, why } => write!(f, "train {id} is {status}: {why}"),
            Self::MainMoved { main, base } => write!(
                f,
                "main is at {}, not at {} where the train was verified",
                short(main, 8),
                short(base, 8)
            ),
            Self::HeadsMoved { prs } => {
                let list: Vec<String> = prs.iter().map(|n| format!("#{n}")).collect();
                write!(
                    f,
                    "{} changed since the train was verified",
                    list.join(", ")
                )
            }
        }
    }
}

/// First reason the train may not land, checked in order: status, base, member heads.
/// A member missing from `heads` counts as moved (fail closed).
pub fn refusal(train: &Train, main: &str, heads: &BTreeMap<u64, Sha>) -> Option<Refusal> {
    if train.status != TrainStatus::Open {
        return Some(Refusal::NotOpen {
            id: train.id.clone(),
            status: train.status.name(),
            why: train.status.why().to_string(),
        });
    }
    if train.base != main {
        return Some(Refusal::MainMoved {
            main: main.to_string(),
            base: train.base.clone(),
        });
    }
    let moved: Vec<u64> = train
        .members
        .iter()
        .filter(|m| heads.get(&m.pr) != Some(&m.head))
        .map(|m| m.pr)
        .collect();
    (!moved.is_empty()).then_some(Refusal::HeadsMoved { prs: moved })
}

/// Main moved without a train landing: every open sequential train is invalidated, unless one is
/// landing (its own merges move main).
pub fn after_outside_merge(trains: &[Train]) -> Vec<Train> {
    if trains.iter().any(|t| t.status.is_landing()) {
        return trains.to_vec();
    }
    let mut out = trains.to_vec();
    for t in &mut out {
        invalidate(t, "main moved outside a train".into());
    }
    out
}

/// Records how a landing ended; every other open sequential train is invalidated ("landed first").
pub fn finished(trains: &[Train], id: &TrainId, status: TrainStatus) -> Vec<Train> {
    let mut out = trains.to_vec();
    for t in &mut out {
        if &t.id == id {
            t.status = status.clone();
        } else {
            invalidate(t, format!("train {id} landed first"));
        }
    }
    out
}

/// The loud sentence when main's tree after landing is not the verified one.
pub fn mismatch(train: &Train, main_tree: &str) -> Option<String> {
    (main_tree != train.tree).then(|| {
        format!(
            "main's tree after landing train {} is {}, not the verified {}: what landed is not what was checked",
            train.id,
            short(main_tree, 12),
            short(&train.tree, 12)
        )
    })
}

/// Why a train stopped mid-landing (`R-LD-6`):
/// `"stopped at #{n} after merging {#a #b | nothing}: {err}"`.
pub fn stopped_why(n: u64, merged: &[u64], err: &str) -> String {
    let after = if merged.is_empty() {
        "nothing".to_string()
    } else {
        merged
            .iter()
            .map(|m| format!("#{m}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!("stopped at #{n} after merging {after}: {err}")
}

/// Loud post text when a train stops mid-landing (`R-LD-6`).
pub fn stopped_loud_line(id: &TrainId, why: &str) -> String {
    format!("Train {id} {why}")
}

/// Success message when a sequential train lands and main's tree matches (`R-LD-8`).
pub fn landed_line(id: &TrainId, merged: &[u64]) -> String {
    let list = merged
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("train {id} landed: {list}; main's tree is the verified one")
}

/// Mail subject for a loud landing failure (`R-LD-10`).
pub fn did_not_land_subject(project: &str) -> String {
    format!("{project}: a train did not land as verified")
}
