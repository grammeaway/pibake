use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pibake::{config::Config, inventory, paths};

#[derive(Parser)]
#[command(version, about = "Build a Pi agent directory into one executable")]
struct Cli {
    /// Path to pibake.toml [default: ./pibake.toml if present]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List what the agent directory contains, and what is config versus state
    Inspect {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = paths::home_dir()?;
    let config = Config::load(cli.config.as_deref(), &std::env::current_dir()?, &home)?;
    match cli.command {
        Command::Inspect { json } => {
            let inventory = inventory::read(&config.agent_dir, &home)?;
            let mut out = std::io::stdout().lock();
            if json {
                serde_json::to_writer_pretty(&mut out, &inventory)?;
                writeln!(out)?;
            } else {
                write!(out, "{}", inventory::render_text(&inventory))?;
            }
        }
    }
    Ok(())
}
