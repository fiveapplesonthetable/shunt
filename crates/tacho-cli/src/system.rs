//! System tracing commands powered by `tacho-system`.

use std::path::PathBuf;
use tacho_system::{Perfetto, Profile};

use crate::cli::SystemSubcommands;

/// Dispatches `tacho system <subcommand>`.
pub fn execute_system(cmd: SystemSubcommands) -> Result<(), String> {
    match cmd {
        SystemSubcommands::Start { profile } => {
            let perfetto = Perfetto::locate().map_err(|e| e.to_string())?;
            let prof = match profile.as_str() {
                "default" => Profile::Default,
                "full" => Profile::Full,
                path => Profile::Path(PathBuf::from(path)),
            };

            perfetto.start(prof).map_err(|e| e.to_string())?;
            println!("Started system tracing session 'tacho-system'");
        }
        SystemSubcommands::Stop => {
            let perfetto = Perfetto::locate().map_err(|e| e.to_string())?;
            perfetto.stop().map_err(|e| e.to_string())?;
            println!("Stopped system tracing session 'tacho-system'");
        }
        SystemSubcommands::Status => {
            let perfetto = Perfetto::locate().map_err(|e| e.to_string())?;
            match perfetto.status().map_err(|e| e.to_string())? {
                Some(sess) => {
                    println!("Session ID: {}", sess.id);
                    println!("Name:       {}", sess.name);
                    println!("State:      {}", sess.state);
                    if let Some(buf_kb) = sess.buffer_size_kb {
                        println!("Buffer:     {buf_kb} KiB");
                    }
                    if let Some(ds) = sess.num_data_sources {
                        println!("DataSources:{ds}");
                    }
                }
                None => {
                    println!("No active 'tacho-system' tracing session.");
                }
            }
        }
        SystemSubcommands::Setup { sudo } => {
            let instructions = tacho_system::setup_instructions(sudo);
            print!("{instructions}");
        }
    }
    Ok(())
}
