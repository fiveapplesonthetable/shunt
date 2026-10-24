//! Machine tidy: pure decisions over an observed snapshot, returned as actions for the janitor
//! (hands) to carry out. Nothing a live process, batch or `start` worktree uses is ever touched.

use serde::{Deserialize, Serialize};

use super::hours;

/// Thresholds and names; defaults are generic, projects override them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TidyPolicy {
    /// How often the tidy pass is due.
    pub interval: i64,
    pub build_idle: i64,
    pub container_idle: i64,
    pub checkout_idle: i64,
    /// A batch's database outlives its batch only when the batch was killed.
    pub batch_container_idle: i64,
    /// A running test runtime keeps mapping libraries it unpacked hours earlier.
    pub unpack_idle: i64,
    /// Containers we made; anything else is never touched.
    pub container_prefixes: Vec<String>,
    /// Prefix of per-worktree databases (`<prefix><slug>`).
    pub database_prefix: String,
    /// Prefix of every resource a batch makes (`<prefix><batch id>...`).
    pub batch_prefix: String,
}

impl Default for TidyPolicy {
    fn default() -> Self {
        Self {
            interval: 15 * 60,
            build_idle: 90 * 60,
            container_idle: 2 * 3600,
            checkout_idle: 6 * 3600,
            batch_container_idle: 20 * 60,
            unpack_idle: 24 * 3600,
            container_prefixes: vec!["tacho-".into(), "agentdb-".into()],
            database_prefix: "agentdb-".into(),
            batch_prefix: "tacho-b-".into(),
        }
    }
}

/// A directory and the seconds since anything inside it changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    pub path: String,
    pub idle: i64,
}

/// A cache directory, its size in GB, its cap, and whether a build holds it right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sized {
    pub path: String,
    pub gb: u64,
    pub cap: u64,
    pub held: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Container {
    pub name: String,
    pub age: i64,
    pub clients: u32,
    /// Host port its database answers on.
    pub port: Option<u16>,
    /// Exited, never-reaped processes inside it.
    pub zombies: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    pub containers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Checkout {
    pub path: String,
    pub idle: i64,
    pub dirty: bool,
    pub unpublished: bool,
}

/// Everything the janitor observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub builds: Vec<Tree>,
    pub unpacks: Vec<Tree>,
    pub containers: Vec<Container>,
    pub checkouts: Vec<Checkout>,
    pub networks: Vec<Network>,
    /// cwd, CARGO_TARGET_DIR and open fd targets of live processes.
    pub held_paths: Vec<String>,
    /// Command-line arguments of live processes.
    pub held_args: Vec<String>,
    /// Database ports named by live processes.
    pub held_ports: Vec<u16>,
    /// Ids of batches that are live (running and pid 0 or alive).
    pub batches: Vec<String>,
    /// `(slug, path)` of each `start` worktree.
    pub starts: Vec<(String, String)>,
    pub caches: Vec<Sized>,
}

/// What to remove, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TidyAction {
    RemoveTree {
        path: String,
        why: String,
    },
    RemoveContainer {
        name: String,
        why: String,
    },
    /// Attached containers go first; docker will not drop the network otherwise.
    RemoveNetwork {
        name: String,
        why: String,
        containers: Vec<String>,
    },
    /// `rescue`: save unpublished or dirty work before removing.
    RemoveCheckout {
        path: String,
        why: String,
        rescue: bool,
    },
}

impl TidyAction {
    pub fn target(&self) -> &str {
        match self {
            TidyAction::RemoveTree { path, .. } | TidyAction::RemoveCheckout { path, .. } => path,
            TidyAction::RemoveContainer { name, .. } | TidyAction::RemoveNetwork { name, .. } => {
                name
            }
        }
    }
    pub fn why(&self) -> &str {
        match self {
            TidyAction::RemoveTree { why, .. }
            | TidyAction::RemoveContainer { why, .. }
            | TidyAction::RemoveNetwork { why, .. }
            | TidyAction::RemoveCheckout { why, .. } => why,
        }
    }
    /// `tidied <t>: <why>` or, in a dry run, `would tidy <t>: <why>`.
    pub fn line(&self, dry: bool) -> String {
        let verb = if dry { "would tidy" } else { "tidied" };
        format!("{verb} {}: {}", self.target(), self.why())
    }
}

/// Tidy is due every `policy.interval` seconds.
pub fn due(last: i64, now: i64, policy: &TidyPolicy) -> bool {
    now - last >= policy.interval
}

/// `path == root` or `path` is under `root/`.
pub fn inside(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root || (path.starts_with(root) && path[root.len()..].starts_with('/'))
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

struct Ownership<'a> {
    snap: &'a Snapshot,
    policy: &'a TidyPolicy,
}

impl Ownership<'_> {
    fn used(&self, path: &str) -> bool {
        !path.is_empty()
            && (self.snap.held_paths.iter().any(|h| inside(h, path))
                || self.snap.held_args.iter().any(|a| a.contains(path)))
    }
    fn batch(&self, name: &str) -> bool {
        self.snap
            .batches
            .iter()
            .any(|b| name.starts_with(&format!("{}{b}", self.policy.batch_prefix)))
    }
    fn slug(&self, name: &str) -> bool {
        self.snap
            .starts
            .iter()
            .any(|(s, p)| s == name && self.used(p))
    }
    fn busy(&self, name: &str, path: &str) -> bool {
        self.batch(name) || self.slug(name) || self.used(path)
    }
}

/// One line per container that holds unreaped processes.
pub fn zombie_lines(snap: &Snapshot) -> Vec<String> {
    snap.containers
        .iter()
        .filter(|c| c.zombies > 0)
        .map(|c| {
            let s = if c.zombies == 1 { "" } else { "es" };
            format!("{} holds {} zombie process{s}", c.name, c.zombies)
        })
        .collect()
}

/// Everything idle, finished or abandoned that no live agent, batch or process is using.
pub fn decide(snap: &Snapshot, policy: &TidyPolicy) -> Vec<TidyAction> {
    let own = Ownership { snap, policy };
    let mut out = Vec::new();
    for t in &snap.builds {
        if t.idle >= policy.build_idle && !own.busy(basename(&t.path), &t.path) {
            out.push(TidyAction::RemoveTree {
                path: t.path.clone(),
                why: format!("build output untouched for {}", hours(t.idle)),
            });
        }
    }
    for c in &snap.caches {
        if c.gb > c.cap && !c.held {
            out.push(TidyAction::RemoveTree {
                path: c.path.clone(),
                why: format!("cache is {} GB, over its {} GB", c.gb, c.cap),
            });
        }
    }
    for t in &snap.unpacks {
        if t.idle >= policy.unpack_idle && !own.used(&t.path) {
            out.push(TidyAction::RemoveTree {
                path: t.path.clone(),
                why: format!("test runtime unpacked {} ago", hours(t.idle)),
            });
        }
    }
    for c in &snap.containers {
        if c.name.starts_with(&policy.batch_prefix) {
            // A batch container is judged only by its batch: clients of a dead batch are leaks.
            if c.age >= policy.batch_container_idle && !own.batch(&c.name) {
                out.push(TidyAction::RemoveContainer {
                    name: c.name.clone(),
                    why: format!(
                        "database of a batch that is no longer running, up {}",
                        hours(c.age)
                    ),
                });
            }
            continue;
        }
        let ours = policy
            .container_prefixes
            .iter()
            .any(|p| !p.is_empty() && c.name.starts_with(p.as_str()));
        let slug = c
            .name
            .strip_prefix(policy.database_prefix.as_str())
            .unwrap_or(&c.name);
        if ours
            && c.age >= policy.container_idle
            && c.clients == 0
            && !c.port.is_some_and(|p| snap.held_ports.contains(&p))
            && !own.busy(&c.name, "")
            && !own.slug(slug)
        {
            out.push(TidyAction::RemoveContainer {
                name: c.name.clone(),
                why: format!("up {} with no client", hours(c.age)),
            });
        }
    }
    for n in &snap.networks {
        if n.name.starts_with(&policy.batch_prefix) && !own.batch(&n.name) {
            out.push(TidyAction::RemoveNetwork {
                name: n.name.clone(),
                why: "network of a batch that is no longer running".into(),
                containers: n.containers.clone(),
            });
        }
    }
    for w in &snap.checkouts {
        let base = basename(&w.path);
        if w.idle >= policy.checkout_idle && !own.busy(base, &w.path) {
            let rescue = (w.dirty || w.unpublished) && !base.starts_with(&policy.batch_prefix);
            out.push(TidyAction::RemoveCheckout {
                path: w.path.clone(),
                why: format!(
                    "untouched for {} with no live batch or process",
                    hours(w.idle)
                ),
                rescue,
            });
        }
    }
    out
}

/// Lifecycle of a recorded run, as far as stale-run retirement cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Running,
    Unfinished,
    Abandoned,
    Done,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFacts {
    pub key: String,
    pub state: RunState,
    pub started: i64,
    pub finished: Option<i64>,
    pub pr: Option<u64>,
    pub branch: Option<String>,
}

/// A run to retire with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleRun {
    pub key: String,
    pub why: String,
}

/// Unfinished or abandoned runs older than `days`, excluding live keys and runs whose PR number
/// or branch is open. `days < 1` is refused.
pub fn stale_runs(
    runs: &[RunFacts],
    now: i64,
    days: i64,
    live: &[String],
    open_refs: &[String],
) -> Result<Vec<StaleRun>, String> {
    if days < 1 {
        return Err("--stale-runs must be at least 1 day".into());
    }
    let cutoff = now - days * 86400;
    let mut out: Vec<StaleRun> = runs
        .iter()
        .filter(|r| matches!(r.state, RunState::Unfinished | RunState::Abandoned))
        .filter(|r| r.finished.unwrap_or(r.started) <= cutoff)
        .filter(|r| !live.contains(&r.key))
        .filter(|r| !r.pr.is_some_and(|n| open_refs.contains(&n.to_string())))
        .filter(|r| !r.branch.as_ref().is_some_and(|b| open_refs.contains(b)))
        .map(|r| StaleRun {
            key: r.key.clone(),
            why: format!("stale run: no progress for over {days} day(s) and no open PR"),
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// `retired N stale run(s)` / `no stale runs`.
pub fn stale_runs_summary(n: usize) -> String {
    if n == 0 {
        "no stale runs".into()
    } else {
        format!("retired {n} stale run(s)")
    }
}

/// Ceiling conversion from bytes to GiB (`(bytes + 2^30 - 1) / 2^30`).
pub fn bytes_to_gb(bytes: u64) -> u64 {
    const GIB: u64 = 1 << 30;
    bytes.saturating_add(GIB - 1) / GIB
}

/// Specification of a size-capped cache directory (R73, J33–J35).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpec {
    pub path: String,
    pub cap_gb: u64,
    pub lock_file: String,
}

/// Builds the list of shared caches (`sccache` 80G, `npm` 8G, `cargo-registry` 20G, `.rescued` 20G)
/// plus per-slot `<warm>/slot-N/target` (80G, locked by `<warm>/slot-N.lock`) (R73, J33).
pub fn default_caches(
    sccache: &str,
    cache: &str,
    work: &str,
    warm: &str,
    warm_entries: &[String],
) -> Vec<CacheSpec> {
    let cache = cache.trim_end_matches('/');
    let mut out = vec![
        CacheSpec {
            path: sccache.to_string(),
            cap_gb: 80,
            lock_file: String::new(),
        },
        CacheSpec {
            path: format!("{cache}/npm"),
            cap_gb: 8,
            lock_file: String::new(),
        },
        CacheSpec {
            path: format!("{cache}/cargo-registry"),
            cap_gb: 20,
            lock_file: String::new(),
        },
    ];
    if !work.is_empty() {
        out.push(CacheSpec {
            path: format!("{}/.rescued", work.trim_end_matches('/')),
            cap_gb: 20,
            lock_file: String::new(),
        });
    }
    if !warm.is_empty() {
        let warm = warm.trim_end_matches('/');
        let mut names: Vec<&String> = warm_entries.iter().collect();
        names.sort();
        for name in names {
            if let Some(s) = name
                .strip_prefix("slot-")
                .and_then(|s| s.strip_suffix(".lock"))
            {
                out.push(CacheSpec {
                    path: format!("{warm}/slot-{s}/target"),
                    cap_gb: 80,
                    lock_file: format!("{warm}/{name}"),
                });
            }
        }
    }
    out
}

/// Filters `/tmp` subdirectories to Robolectric test runtime unpacks (`robolectric*`) (J26).
pub fn filter_unpacks(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| basename(p).starts_with("robolectric"))
        .cloned()
        .collect()
}

/// Returns IDs of batches whose status is `"running"` and whose `pid == 0` (starting) or alive (J10–J12).
pub fn live_batches<F: Fn(u32) -> bool>(
    batches: &[(String, String, u32)],
    is_alive: F,
) -> Vec<String> {
    batches
        .iter()
        .filter(|(_, status, pid)| status == "running" && (*pid == 0 || is_alive(*pid)))
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// Extracts `<port>` from `DATABASE_URL` (`...@host:port/db`) (J9).
pub fn extract_db_port(database_url: &str) -> Option<u16> {
    let after_at = database_url.split_once('@')?.1;
    let host_port = after_at.split_once('/')?.0;
    host_port.rsplit_once(':')?.1.parse::<u16>().ok()
}

/// Extracts `<port>` from Docker port mapping `:5000->5432/tcp` (J8).
pub fn extract_published_port(ports_str: &str) -> Option<u16> {
    ports_str
        .split_once("->5432/tcp")?
        .0
        .rsplit_once(':')?
        .1
        .parse::<u16>()
        .ok()
}

/// Counts zombie processes (`Z*` state after the `STAT` header line) in `docker top <c> -eo stat` (J28).
pub fn count_zombies(top_output: &str) -> u32 {
    top_output
        .lines()
        .skip(1)
        .filter(|l| l.trim_start().starts_with('Z'))
        .count() as u32
}

/// Deterministic rescue directory path: `<work>/.rescued/<basename>-<sha256(path)[:12]>-<head>` (J14–J19).
pub fn rescue_dir_name(work: &str, checkout_path: &str, head_sha: &str) -> String {
    let key = &super::kit::sha256_hex(checkout_path.as_bytes())[..12];
    format!(
        "{}/.rescued/{}-{key}-{}",
        work.trim_end_matches('/'),
        basename(checkout_path),
        head_sha.trim()
    )
}

/// Pure plan for rescuing a dirty or unpublished worktree before removal (R75, J14–J20, J22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescuePlan {
    pub target_dir: String,
    pub reuse_existing: bool,
    pub head_ref: String,
    pub other_refs: Vec<String>,
    pub bundle_cmd: Option<Vec<String>>,
    pub diff_cmd: Vec<String>,
    pub untracked_files: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn plan_rescue(
    work: &str,
    checkout_path: &str,
    head_sha: &str,
    already_complete: bool,
    current_branch: &str,
    all_refs: &[String],
    unique_commits: u64,
    untracked: &[String],
) -> RescuePlan {
    let target_dir = rescue_dir_name(work, checkout_path, head_sha);
    let head_ref: String = if current_branch.trim().is_empty() {
        "HEAD".into()
    } else {
        current_branch.trim().into()
    };
    let other_refs: Vec<String> = all_refs
        .iter()
        .filter(|r| r.as_str() != current_branch.trim())
        .cloned()
        .collect();
    let bundle_cmd = if !already_complete && unique_commits > 0 {
        let mut cmd = vec![
            "git".into(),
            "-C".into(),
            checkout_path.into(),
            "bundle".into(),
            "create".into(),
            format!("{target_dir}/history.bundle"),
            head_ref.clone(),
            "--not".into(),
        ];
        cmd.extend(other_refs.clone());
        Some(cmd)
    } else {
        None
    };
    let diff_cmd = vec![
        "git".into(),
        "-C".into(),
        checkout_path.into(),
        "diff".into(),
        "--binary".into(),
        "HEAD".into(),
    ];
    let untracked_files = untracked
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    RescuePlan {
        target_dir,
        reuse_existing: already_complete,
        head_ref,
        other_refs,
        bundle_cmd,
        diff_cmd,
        untracked_files,
    }
}

/// Entry in `<work>/.rescued` for over-cap pruning (J21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueEntry {
    pub name: String,
    pub path: String,
    pub mtime: i64,
    pub bytes: u64,
    pub head_sha: String,
    pub is_referenced: bool,
}

/// Prunes oldest referenced rescue directories until `total_bytes <= cap_bytes`; unreferenced
/// (loose) rescues are never deleted (R75, J21).
pub fn plan_prune_rescues(
    entries: &[RescueEntry],
    cap_bytes: u64,
) -> (Vec<String>, Option<String>) {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| (a.mtime, a.name.as_str()).cmp(&(b.mtime, b.name.as_str())));
    let mut total: u64 = sorted.iter().map(|e| e.bytes).sum();
    let mut removed = Vec::new();
    for entry in &sorted {
        if total <= cap_bytes {
            break;
        }
        if entry.is_referenced {
            removed.push(entry.path.clone());
            total = total.saturating_sub(entry.bytes);
        }
    }
    let err = (total > cap_bytes).then(|| {
        format!(
            "rescue directory remains over {} GB; no referenced rescues can be pruned",
            cap_bytes >> 30
        )
    });
    (removed, err)
}

/// Fallback Docker command to remove root-owned files left behind by containers (J6).
pub fn docker_rm_fallback_cmd(path: &str) -> Vec<String> {
    let trimmed = path.trim_end_matches('/');
    let (parent, base) = trimmed.rsplit_once('/').unwrap_or(("/", trimmed));
    let parent = if parent.is_empty() { "/" } else { parent };
    vec![
        "docker".into(),
        "run".into(),
        "--rm".into(),
        "--init".into(),
        "-v".into(),
        format!("{parent}:/b"),
        "alpine".into(),
        "rm".into(),
        "-rf".into(),
        format!("/b/{base}"),
    ]
}

/// Pure check for `core.bare=true` corruption caused by concurrent worktree operations (R77).
pub fn core_bare_repair(is_bare: Option<bool>) -> Option<String> {
    (is_bare == Some(true))
        .then(|| "core.bare was true on working repository; reset core.bare=false".into())
}
