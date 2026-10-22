//! Command-line interface definitions for `tacho`.
//!
//! Help text is kept short, concrete, and plain-worded, with one example per command.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "tacho",
    about = "Flight recorder for coding agents.",
    long_about = "tacho records agent turns, tool calls, and system stats into a persistent SQLite WAL log.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the background flight recorder daemon.
    ///
    /// Example: tacho daemon
    Daemon(DaemonArgs),

    /// Emit an event or span to the flight recorder.
    ///
    /// Example: tacho emit state worker-1 Thinking --because "analyzing code"
    Emit(EmitArgs),

    /// Process hook payload from stdin and send to daemon. Always exits 0.
    ///
    /// Example: tacho hook claude < payload.json
    Hook(HookArgs),

    /// Run a command, teeing output to terminal and flight recorder.
    ///
    /// Example: tacho run -- cargo test
    Run(RunArgs),

    /// Write a trace snapshot to a Perfetto file.
    ///
    /// Example: tacho trace -o trace.pftrace
    Trace(TraceArgs),

    /// Show live status of all recorded agents.
    ///
    /// Example: tacho status
    Status(StatusArgs),

    /// Print a short timeline report for an agent.
    ///
    /// Example: tacho report worker-1 --since 30m
    Report(ReportArgs),

    /// Watch live state transitions and detector firings.
    ///
    /// Example: tacho watch --until stalled
    Watch(WatchArgs),

    /// Run a PerfettoSQL query against a fresh snapshot.
    ///
    /// Example: tacho query agents-now
    Query(QueryArgs),

    /// Install tachograph hooks into an agent's configuration.
    ///
    /// Example: tacho install claude --user
    Install(InstallArgs),

    /// Remove tachograph hooks from an agent's configuration.
    ///
    /// Example: tacho uninstall claude --user
    Uninstall(UninstallArgs),

    /// Manage background Perfetto system tracing sessions.
    ///
    /// Example: tacho system start --profile default
    System(SystemArgs),

    /// Tail on-disk agent JSONL transcripts into the flight recorder.
    ///
    /// Example: tacho tail ~/.claude/projects
    Tail(TailArgs),
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Optional OTLP HTTP listen address.
    ///
    /// Example: --otlp 127.0.0.1:4318
    #[arg(long)]
    pub otlp: Option<String>,

    /// Optional shell command to run when a stall detector fires.
    ///
    /// Example: --on-stall "notify-send \"Stall: $TACHO_AGENT ($TACHO_STALL_KIND)\""
    #[arg(long)]
    pub on_stall: Option<String>,

    /// Run daemon in foreground instead of background.
    #[arg(long)]
    pub foreground: bool,
}

#[derive(Args, Debug)]
pub struct EmitArgs {
    /// Read NDJSON inputs line-by-line from stdin.
    ///
    /// Example: tacho emit --json < events.jsonl
    #[arg(long)]
    pub json: bool,

    #[command(subcommand)]
    pub subcommand: Option<EmitSubcommands>,
}

#[derive(Subcommand, Debug)]
pub enum EmitSubcommands {
    /// Declare a task and its intended file paths.
    ///
    /// Example: tacho emit task auth-flow --paths src/auth.rs --brief-file brief.md
    Task {
        /// Task slug or name.
        slug: String,
        /// Comma-separated list of expected paths.
        #[arg(long)]
        paths: Option<String>,
        /// Path to task brief file.
        #[arg(long)]
        brief_file: Option<PathBuf>,
        /// Assigned agent identifier.
        #[arg(long)]
        agent: Option<String>,
        /// Base commit SHA.
        #[arg(long)]
        base_commit: Option<String>,
        /// Token budget for this task.
        #[arg(long)]
        token_budget: Option<u64>,
        /// Cost budget in micros for this task.
        #[arg(long)]
        cost_budget: Option<u64>,
    },

    /// Update an agent or task state.
    ///
    /// Example: tacho emit state worker-1 Thinking --because "analyzing code"
    State {
        /// Target agent or task slug.
        target: String,
        /// State name (e.g. Idle, Thinking, Tool, Waiting, Stalled).
        state: String,
        /// Human-readable explanation.
        #[arg(long)]
        because: Option<String>,
    },

    /// Record the beginning or end of a custom span.
    ///
    /// Example: tacho emit span begin agent:1 compile --id s1 --arg opt=2
    Span {
        #[command(subcommand)]
        action: SpanAction,
    },

    /// Record a single point-in-time instant event.
    ///
    /// Example: tacho emit instant agent:1 checkpoint --arg step=5
    Instant {
        /// Track key (e.g. agent:1).
        track: String,
        /// Name of the instant event.
        name: String,
        /// Custom key=value attributes.
        #[arg(long = "arg")]
        args: Vec<String>,
    },

    /// Record a counter value on a track.
    ///
    /// Example: tacho emit counter agent:1/tokens 42000 --unit tokens
    Counter {
        /// Track key (e.g. agent:1/tokens).
        track: String,
        /// Numeric counter value.
        value: i64,
        /// Optional measurement unit.
        #[arg(long)]
        unit: Option<String>,
    },

    /// Record a message sent between agents.
    ///
    /// Example: tacho emit message --from worker-1 --to coordinator --bytes 128
    Message {
        /// Sending agent name.
        #[arg(long)]
        from: String,
        /// Receiving agent name.
        #[arg(long)]
        to: String,
        /// Message size in bytes.
        #[arg(long)]
        bytes: Option<u64>,
        /// Optional message ID.
        #[arg(long)]
        id: Option<String>,
    },

    /// Record a review finding for a task.
    ///
    /// Example: tacho emit finding task-1 --severity major --path src/main.rs --line 42 --title "Bug"
    Finding {
        /// Target task slug.
        task: String,
        /// Severity: info, minor, major, critical.
        #[arg(long)]
        severity: String,
        /// File path where finding occurred.
        #[arg(long)]
        path: Option<String>,
        /// Line number in file.
        #[arg(long)]
        line: Option<u32>,
        /// Short title of finding.
        #[arg(long)]
        title: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SpanAction {
    /// Begin a span.
    ///
    /// Example: tacho emit span begin agent:1 build --id b1
    Begin {
        /// Track key (e.g. agent:1).
        track: String,
        /// Name of span.
        name: String,
        /// Unique span ID.
        #[arg(long)]
        id: Option<String>,
        /// Custom key=value attributes.
        #[arg(long = "arg")]
        args: Vec<String>,
    },

    /// End a span.
    ///
    /// Example: tacho emit span end agent:1 --id b1
    End {
        /// Track key (e.g. agent:1).
        track: String,
        /// Unique span ID matching begin.
        #[arg(long)]
        id: Option<String>,
        /// Custom key=value attributes.
        #[arg(long = "arg")]
        args: Vec<String>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Gemini,
    Jetski,
}

#[derive(Args, Debug)]
pub struct HookArgs {
    /// Agent provider whose hook payload is being piped to stdin.
    ///
    /// Example: claude
    pub provider: Provider,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Command and arguments to execute.
    ///
    /// Example: tacho run -- cargo test
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Args, Debug)]
pub struct TraceArgs {
    /// Output file path. Defaults to $TACHO_HOME/trace/<timestamp>.perfetto-trace.
    ///
    /// Example: -o snapshot.pftrace
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Only include events within the specified duration window (e.g. 30m, 2h).
    ///
    /// Example: --since 1h
    #[arg(long)]
    pub since: Option<String>,

    /// Merge unprivileged Perfetto system trace alongside agent trace.
    ///
    /// Example: --system
    #[arg(long)]
    pub system: bool,

    /// Stream binary trace packets directly to standard output.
    ///
    /// Example: --stdout
    #[arg(long)]
    pub stdout: bool,

    /// Rebuild trace from SQLite WAL event log.
    ///
    /// Example: --from-log 30m
    #[arg(long)]
    pub from_log: Option<Option<String>>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Output status as structured JSON.
    ///
    /// Example: --json
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// Agent name to report on.
    ///
    /// Example: worker-1
    pub agent: String,

    /// Filter events within the specified duration (e.g. 30m, 2h).
    ///
    /// Example: --since 30m
    #[arg(long)]
    pub since: Option<String>,

    /// Show claim verification summary (edits, test after last edit, commits, pushes, CLAIM flags, git worktree status).
    ///
    /// Example: --diff-claims
    #[arg(long)]
    pub diff_claims: bool,
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Output updates as NDJSON lines.
    ///
    /// Example: --json
    #[arg(long)]
    pub json: bool,

    /// Block until an agent reaches a target state: stalled, idle, or exit.
    ///
    /// Example: --until stalled
    #[arg(long)]
    pub until: Option<String>,
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    /// Shipped query name (e.g. agents-now, open-waits), query file path, or - for stdin.
    ///
    /// Example: agents-now
    pub query: String,

    /// Path to an existing trace file to query instead of live snapshot.
    ///
    /// Example: --trace /path/to/trace.pftrace
    #[arg(long)]
    pub trace: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Agent provider to install hooks for: claude, gemini, or jetski.
    ///
    /// Example: claude
    pub provider: Provider,

    /// Install into project directory (e.g. ./.claude/settings.json). Default.
    #[arg(long, default_value_t = true, conflicts_with = "user")]
    pub project: bool,

    /// Install into user home directory (e.g. ~/.claude/settings.json).
    #[arg(long)]
    pub user: bool,
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Agent provider to remove hooks for: claude, gemini, or jetski.
    ///
    /// Example: claude
    pub provider: Provider,

    /// Remove from project directory. Default.
    #[arg(long, default_value_t = true, conflicts_with = "user")]
    pub project: bool,

    /// Remove from user home directory.
    #[arg(long)]
    pub user: bool,
}

#[derive(Args, Debug)]
pub struct SystemArgs {
    #[command(subcommand)]
    pub command: SystemSubcommands,
}

#[derive(Subcommand, Debug)]
pub enum SystemSubcommands {
    /// Start a background Perfetto system tracing session.
    ///
    /// Example: tacho system start --profile default
    Start {
        /// Profile to use: default (unprivileged), full (root ftrace), or path to config file.
        #[arg(long, default_value = "default")]
        profile: String,
    },

    /// Stop the background Perfetto system tracing session.
    ///
    /// Example: tacho system stop
    Stop,

    /// Show status of the Perfetto system tracing session.
    ///
    /// Example: tacho system status
    Status,

    /// Print setup commands for traced and traced_probes.
    ///
    /// Example: tacho system setup --sudo
    Setup {
        /// Show root commands for full profile with ftrace.
        #[arg(long)]
        sudo: bool,
    },
}

#[derive(Args, Debug, Default)]
pub struct TailArgs {
    /// Path to a transcript .jsonl file or projects directory. Defaults to ~/.claude/projects.
    ///
    /// Example: ~/.claude/projects
    pub path: Option<PathBuf>,

    /// Scan and ingest new transcript lines once, then exit.
    ///
    /// Example: --once
    #[arg(long)]
    pub once: bool,

    /// Optional agent identifier to attribute transcript sessions to.
    ///
    /// Example: --agent worker-1
    #[arg(long)]
    pub agent: Option<String>,
}
