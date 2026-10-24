//! Telling the owner once: one ledger `key -> (fingerprint, at)` for every fact, notices for what
//! is new, delivery settlement with retry, untied-failure counting, and the daily digest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::short;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TellPolicy {
    /// Per-kind cap on remembered keys (oldest dropped first).
    pub ledger_cap: usize,
    /// A red batch is told only if it finished within this window.
    pub red_batch_window: i64,
    /// Untied failures stop retrying at this count.
    pub untied_tries: u32,
    pub digest_every: i64,
}

impl Default for TellPolicy {
    fn default() -> Self {
        Self {
            ledger_cap: 500,
            red_batch_window: 3600,
            untied_tries: 2,
            digest_every: 86400,
        }
    }
}

/// A told fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Told {
    pub fingerprint: String,
    pub at: i64,
}

/// The single tell-once ledger. Keys are namespaced by [`FactKind::tag`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Ledger {
    pub told: BTreeMap<String, Told>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub enum FactKind {
    ReadySet,
    Decision,
    MainJobFailed,
    BatchRed,
    Untied,
    DeployStarted,
    DeployDone,
}

impl FactKind {
    pub fn tag(self) -> &'static str {
        match self {
            FactKind::ReadySet => "ready",
            FactKind::Decision => "decision",
            FactKind::MainJobFailed => "mainjob",
            FactKind::BatchRed => "batchred",
            FactKind::Untied => "untied",
            FactKind::DeployStarted => "deploystart",
            FactKind::DeployDone => "deploydone",
        }
    }
    /// Facts whose key should be forgotten when no longer standing, so they are told again if
    /// they come back.
    pub fn forget_when_gone(self) -> bool {
        matches!(self, FactKind::Decision)
    }
}

/// One ready PR in a ready set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyPr {
    pub number: u64,
    pub head: String,
    pub title: String,
    pub body: String,
}

/// A fact the owner may need to hear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fact {
    /// Told when a new `n@head` or `train:<id>` appears. Needs a train id.
    ReadySet {
        train: String,
        prs: Vec<ReadyPr>,
        land_command: String,
    },
    /// Told once per (key, sentence) while it stands.
    Decision {
        key: String,
        sentence: String,
    },
    MainJobFailed {
        run: u64,
        job: String,
        sha: String,
        first_line: String,
        url: String,
    },
    BatchRed {
        id: String,
        finished_at: i64,
        lines: Vec<String>,
    },
    Untied {
        pr: u64,
        sha: String,
        times: u32,
        why: String,
    },
    DeployStarted {
        run: u64,
        sha: String,
        url: String,
    },
    DeployDone {
        run: u64,
        sha: String,
        ok: bool,
        what: String,
    },
}

/// What to send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mail {
    pub subject: String,
    pub body: String,
}

/// A pending notice: the keys it settles, the mail, the event line, and why it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub kind: FactKind,
    pub keys: Vec<(String, String)>,
    pub mail: Mail,
    pub event: String,
    pub reason: String,
}

/// Outcome of one send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    Delivered,
    /// Not recorded: retried next tick.
    Failed,
    /// No notifier: recorded (never retried) but surfaced as attention.
    NotConfigured,
}

fn first_paragraph(body: &str) -> String {
    body.split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Fact {
    pub fn kind(&self) -> FactKind {
        match self {
            Fact::ReadySet { .. } => FactKind::ReadySet,
            Fact::Decision { .. } => FactKind::Decision,
            Fact::MainJobFailed { .. } => FactKind::MainJobFailed,
            Fact::BatchRed { .. } => FactKind::BatchRed,
            Fact::Untied { .. } => FactKind::Untied,
            Fact::DeployStarted { .. } => FactKind::DeployStarted,
            Fact::DeployDone { .. } => FactKind::DeployDone,
        }
    }

    /// `(key, fingerprint)` pairs; a fact is new when any pair is missing or differs.
    pub fn keys(&self) -> Vec<(String, String)> {
        let t = self.kind().tag();
        match self {
            Fact::ReadySet { train, prs, .. } => {
                let mut v: Vec<_> = prs
                    .iter()
                    .map(|p| (format!("{t}:{}@{}", p.number, p.head), String::new()))
                    .collect();
                v.push((format!("{t}:train:{train}"), String::new()));
                v
            }
            Fact::Decision { key, sentence } => vec![(format!("{t}:{key}"), sentence.clone())],
            Fact::MainJobFailed { run, job, .. } => {
                vec![(format!("{t}:{run}:{job}"), String::new())]
            }
            Fact::BatchRed { id, .. } => vec![(format!("{t}:{id}"), String::new())],
            Fact::Untied { pr, sha, times, .. } => {
                vec![(format!("{t}:{pr}@{sha}"), times.to_string())]
            }
            Fact::DeployStarted { run, .. } => vec![(format!("{t}:{run}"), String::new())],
            Fact::DeployDone { run, .. } => vec![(format!("{t}:{run}"), String::new())],
        }
    }

    fn notice(&self, cfg: &super::OpsConfig) -> Notice {
        let (p, tool) = (&cfg.project, &cfg.tool);
        let (subject, body, event, reason) = match self {
            Fact::ReadySet {
                train,
                prs,
                land_command,
            } => {
                let n = prs.len();
                let subject = if n == 1 {
                    format!("{p}: 1 pull request is ready to merge")
                } else {
                    format!("{p}: {n} pull requests are ready to merge")
                };
                let mut body = String::new();
                for pr in prs {
                    body.push_str(&format!(
                        "#{} {}\n{}\n\n",
                        pr.number,
                        pr.title,
                        first_paragraph(&pr.body)
                    ));
                }
                body.push_str(&format!(
                    "Land them, in this order, as verified:\n{land_command}\n"
                ));
                let list = prs
                    .iter()
                    .map(|p| format!("#{}", p.number))
                    .collect::<Vec<_>>()
                    .join(" ");
                (
                    subject,
                    body,
                    format!("emailed the owner: {list} ready to merge"),
                    format!("train {train} is verified and has news for the owner"),
                )
            }
            Fact::Decision { key, sentence } => (
                format!("{p}: {key} needs you"),
                sentence.clone(),
                format!("{key} needs a person: {sentence}"),
                "a decision only a person can make is standing".into(),
            ),
            Fact::MainJobFailed {
                run,
                job,
                sha,
                first_line,
                url,
            } => (
                format!("{p}: main {} failed {job}", short(sha)),
                format!("{first_line}\n\n{url}\n"),
                format!("main {} failed {job} (run {run})", short(sha)),
                "a job on main failed".into(),
            ),
            Fact::BatchRed { id, lines, .. } => (
                format!("{p}: batch {id} is red"),
                lines.join("\n"),
                lines.join("; "),
                "a batch finished red within the report window".into(),
            ),
            Fact::Untied {
                pr,
                sha,
                times,
                why,
            } => {
                let tries = cfg.tell.untied_tries;
                let (head, tail) = if *times >= tries {
                    (
                        format!("#{pr} failed outside its code twice"),
                        format!(
                            "Push to the branch or run `{tool} batch {pr}` when the cause is gone."
                        ),
                    )
                } else {
                    (
                        format!("#{pr} failed outside its code, trying once more"),
                        String::new(),
                    )
                };
                (
                    format!("{p}: {head}"),
                    format!("{head} at {}: {why}\n{tail}", short(sha))
                        .trim_end()
                        .to_string(),
                    head,
                    format!("untied failure {times} of {tries}"),
                )
            }
            Fact::DeployStarted { sha, url, .. } => (
                format!("{p}: deploy of {} started", short(sha)),
                url.clone(),
                format!("deploy of {} started: {url}", short(sha)),
                "a deploy of main started".into(),
            ),
            Fact::DeployDone { sha, ok, what, .. } => {
                let line = if *ok {
                    format!("deployed {}: {what}", short(sha))
                } else {
                    format!("deploy of {} failed: {what}", short(sha))
                };
                (
                    format!("{p}: {line}"),
                    line.clone(),
                    line,
                    "a deploy of main finished".into(),
                )
            }
        };
        Notice {
            kind: self.kind(),
            keys: self.keys(),
            mail: Mail { subject, body },
            event,
            reason,
        }
    }
}

/// Notices for facts the ledger has not told (in fact order). Ready sets need a train id.
/// Red batches outside the window are never told.
pub fn pending(ledger: &Ledger, facts: &[Fact], now: i64, cfg: &super::OpsConfig) -> Vec<Notice> {
    facts
        .iter()
        .filter(|f| match f {
            Fact::ReadySet { train, prs, .. } => !train.is_empty() && !prs.is_empty(),
            Fact::BatchRed { finished_at, .. } => now - finished_at <= cfg.tell.red_batch_window,
            Fact::Untied { times, .. } => *times > 0,
            _ => true,
        })
        .filter(|f| {
            f.keys()
                .iter()
                .any(|(k, fp)| ledger.told.get(k).is_none_or(|t| &t.fingerprint != fp))
        })
        .map(|f| f.notice(cfg))
        .collect()
}

/// Record the outcome of a send. Returns an attention line when nothing could be sent.
pub fn settle(
    mut ledger: Ledger,
    notice: &Notice,
    delivery: Delivery,
    now: i64,
    cap: usize,
) -> (Ledger, Option<String>) {
    let attention = match delivery {
        Delivery::Failed => return (ledger, None),
        Delivery::Delivered => None,
        Delivery::NotConfigured => Some(format!(
            "no notifier is configured, so the owner was not told: {}",
            notice.mail.subject
        )),
    };
    for (k, fp) in &notice.keys {
        ledger.told.insert(
            k.clone(),
            Told {
                fingerprint: fp.clone(),
                at: now,
            },
        );
    }
    (bound(ledger, notice.kind, cap), attention)
}

/// Keep at most `cap` keys of `kind`, dropping the oldest.
pub fn bound(mut ledger: Ledger, kind: FactKind, cap: usize) -> Ledger {
    let prefix = format!("{}:", kind.tag());
    let mut keys: Vec<(i64, String)> = ledger
        .told
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, t)| (t.at, k.clone()))
        .collect();
    if keys.len() > cap {
        keys.sort();
        for (_, k) in keys.iter().take(keys.len() - cap) {
            ledger.told.remove(k);
        }
    }
    ledger
}

/// Forget keys of kinds that should be told again when they come back and are not standing.
pub fn prune(mut ledger: Ledger, standing: &[Fact]) -> Ledger {
    let keep: Vec<String> = standing
        .iter()
        .flat_map(|f| f.keys().into_iter().map(|(k, _)| k))
        .collect();
    let forget: Vec<String> = [FactKind::Decision]
        .iter()
        .filter(|k| k.forget_when_gone())
        .map(|k| format!("{}:", k.tag()))
        .collect();
    ledger
        .told
        .retain(|k, _| keep.contains(k) || !forget.iter().any(|p| k.starts_with(p.as_str())));
    ledger
}

/// Per-head untied failure record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Untied {
    pub sha: String,
    pub times: u32,
    pub why: String,
}

/// Count an untied failure: a new head starts again at 1.
pub fn record_untied(prev: Option<&Untied>, sha: &str, why: &str) -> Untied {
    let times = match prev {
        Some(u) if u.sha == sha => u.times + 1,
        _ => 1,
    };
    Untied {
        sha: sha.into(),
        times,
        why: why.into(),
    }
}

/// Stopped: no more automatic retries at this head.
pub fn untied_stopped(u: &Untied, policy: &TellPolicy) -> bool {
    u.times >= policy.untied_tries
}

/// The digest buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Digest {
    /// `None` until the first look, which only stamps it.
    pub sent: Option<i64>,
    pub lines: Vec<(i64, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestStep {
    /// First call: stamp `sent = now`, so the first digest comes a day later.
    Stamp {
        now: i64,
    },
    Wait {
        reason: String,
    },
    Send {
        mail: Mail,
        reason: String,
    },
}

/// Decide whether the daily digest goes out. On a delivered send the caller clears the lines and
/// sets `sent = now`.
pub fn digest_step(
    d: &Digest,
    stuck: &[String],
    progress: &[String],
    now: i64,
    cfg: &super::OpsConfig,
) -> DigestStep {
    let Some(sent) = d.sent else {
        return DigestStep::Stamp { now };
    };
    if d.lines.is_empty() && stuck.is_empty() {
        return DigestStep::Wait {
            reason: "nothing to report".into(),
        };
    }
    if now - sent < cfg.tell.digest_every {
        return DigestStep::Wait {
            reason: format!("the last digest went out {} s ago", now - sent),
        };
    }
    let mut body = String::new();
    if d.lines.is_empty() {
        body.push_str("No events.\n");
    }
    for (at, line) in &d.lines {
        body.push_str(&format!("{at}  {line}\n"));
    }
    if !stuck.is_empty() {
        body.push_str("\nSTUCK\n");
        for s in stuck {
            body.push_str(&format!("{s}\n"));
        }
    }
    if !progress.is_empty() {
        body.push_str("\nPROGRESS\n");
        for s in progress {
            body.push_str(&format!("{s}\n"));
        }
    }
    DigestStep::Send {
        mail: Mail {
            subject: format!("{}: the day's events", cfg.project),
            body,
        },
        reason: format!("{} event(s) and a day since the last digest", d.lines.len()),
    }
}

/// Apply a digest step's outcome.
pub fn settle_digest(mut d: Digest, step: &DigestStep, delivery: Delivery, now: i64) -> Digest {
    match (step, delivery) {
        (DigestStep::Stamp { now }, _) => d.sent = Some(*now),
        (DigestStep::Send { .. }, Delivery::Delivered | Delivery::NotConfigured) => {
            d.lines.clear();
            d.sent = Some(now);
        }
        _ => {}
    }
    d
}
