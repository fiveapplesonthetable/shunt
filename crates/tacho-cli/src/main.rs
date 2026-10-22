//! `tacho`: Flight recorder CLI for coding agents.

pub mod cli;
pub mod client;
pub mod daemon;
pub mod emit;
pub mod hook;
pub mod install;
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
    }
}
