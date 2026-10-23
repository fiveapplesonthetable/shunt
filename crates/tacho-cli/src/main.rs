//! `tacho`: Flight recorder CLI for coding agents.

pub mod cli;
pub mod client;
pub mod daemon;
pub mod emit;
pub mod fleet;
pub mod hook;
pub mod install;
pub mod kit;
pub mod paths;
pub mod query;
pub mod run;
pub mod status;
pub mod system;
pub mod tail;
pub mod trace;

use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args) => {
            if let Err(e) = daemon::run_daemon(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Emit(args) => {
            if let Err(e) = emit::execute_emit(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Hook(args) => {
            hook::execute_hook(args.provider);
            // Must always exit 0 and print nothing to stdout
            std::process::exit(0);
        }
        Commands::Run(args) => match run::execute_run(args.cmd) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        Commands::Trace(args) => {
            if let Err(e) = trace::execute_trace(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Status(args) => {
            if let Err(e) = status::execute_status(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Report(args) => {
            if let Err(e) = status::execute_report(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Watch(args) => {
            if let Err(e) = status::execute_watch(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Query(args) => {
            if let Err(e) = query::execute_query(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Install(args) => {
            if let Err(e) = install::install_hooks(args.provider, args.user) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Uninstall(args) => {
            if let Err(e) = install::uninstall_hooks(args.provider, args.user) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::System(args) => {
            if let Err(e) = system::execute_system(args.command) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tail(args) => {
            if let Err(e) = tail::execute_tail(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Start(args) => {
            if let Err(e) = kit::execute_start(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Prove(args) => {
            if let Err(e) = kit::execute_prove(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Job(args) => {
            if let Err(e) = kit::execute_job(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Wait(args) => match kit::execute_wait(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        Commands::Kill(args) => {
            if let Err(e) = kit::execute_kill(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Pr(args) => {
            if let Err(e) = kit::execute_pr(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Note(args) => {
            if let Err(e) = kit::execute_note(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Starve(args) => match kit::execute_starve(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        Commands::Todo(args) => {
            if let Err(e) = fleet::execute_todo(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Session(args) => {
            if let Err(e) = fleet::execute_session(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Hold(args) => {
            if let Err(e) = fleet::execute_hold(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Release(args) => {
            if let Err(e) = fleet::execute_release(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Retire(args) => {
            if let Err(e) = fleet::execute_retire(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Attach(args) => {
            if let Err(e) = fleet::execute_attach(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Stop(args) => {
            if let Err(e) = fleet::execute_stop(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tell(args) => {
            if let Err(e) = fleet::execute_tell(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Next(args) => {
            if let Err(e) = fleet::execute_next(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Progress(args) => {
            if let Err(e) = fleet::execute_progress(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Flakes(args) => {
            if let Err(e) = fleet::execute_flakes(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Batch(args) => {
            if let Err(e) = fleet::execute_batch(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Replay(args) => {
            if let Err(e) = fleet::execute_replay(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Train(args) => {
            if let Err(e) = fleet::execute_train(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::ReleaseTrain(args) => {
            if let Err(e) = fleet::execute_release_train(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Land(args) => {
            if let Err(e) = fleet::execute_land(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Queue(args) => {
            if let Err(e) = fleet::execute_queue(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tidy(args) => {
            if let Err(e) = fleet::execute_tidy(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tick(args) => {
            if let Err(e) = fleet::execute_tick(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Events(args) => {
            if let Err(e) = fleet::execute_events(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Top(args) => {
            if let Err(e) = fleet::execute_top(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Serve(args) => {
            if let Err(e) = fleet::execute_serve(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Watchdog(args) => {
            if let Err(e) = fleet::execute_watchdog(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
}
