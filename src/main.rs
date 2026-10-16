use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use pibake::bench::{self, Bench};
use pibake::fake_provider::FakeProvider;
use pibake::harness::PiCommand;
use pibake::{config::Config, inventory, paths, snapshot};

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
    /// Compare tools, system prompt and commands of two Pi commands on the same setup
    Verify {
        /// The Pi command under test, e.g. `pi` or `pi -ne`
        #[arg(long)]
        pi: String,
        /// The reference Pi command
        #[arg(long, default_value = "pi")]
        against: String,
    },
    /// Measure startup and a streamed 200 KB write, interleaved across Pi commands
    Bench {
        /// A Pi command to measure; repeat to compare
        #[arg(long = "pi", required = true)]
        pi: Vec<String>,
        #[arg(long, default_value_t = 10)]
        runs: usize,
        /// Write the results to bench_results/<date>_<label>.json
        #[arg(long)]
        record: Option<String>,
    },
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let home = paths::home_dir()?;
    let config = Config::load(cli.config.as_deref(), &std::env::current_dir()?, &home)?;
    let mut out = std::io::stdout().lock();
    match cli.command {
        Command::Inspect { json } => {
            let inventory = inventory::read(&config.agent_dir, &home)?;
            if json {
                serde_json::to_writer_pretty(&mut out, &inventory)?;
                writeln!(out)?;
            } else {
                write!(out, "{}", inventory::render_text(&inventory))?;
            }
        }
        Command::Verify { pi, against } => {
            let (pi, against) = (PiCommand::parse(&pi)?, PiCommand::parse(&against)?);
            let provider = FakeProvider::start()?;
            let ours = snapshot::capture(&pi, &config.agent_dir, &home, &provider)?;
            let reference = snapshot::capture(&against, &config.agent_dir, &home, &provider)?;
            let report = snapshot::compare(&ours, &pi.label, &reference, &against.label);
            if !report.is_empty() {
                write!(out, "--- {}\n+++ {}\n{report}", against.label, pi.label)?;
                return Ok(ExitCode::FAILURE);
            }
            writeln!(
                out,
                "same: {} tools, {} commands, {}-line system prompt",
                ours.tools.len(),
                ours.commands.len(),
                ours.system_prompt.lines().count()
            )?;
        }
        Command::Bench { pi, runs, record } => {
            if runs == 0 {
                bail!("--runs must be at least 1");
            }
            let commands = pi
                .iter()
                .map(|p| PiCommand::parse(p))
                .collect::<Result<Vec<_>>>()?;
            let bench = Bench {
                commands: &commands,
                runs,
                source: &config.agent_dir,
                real_home: &home,
            };
            let results = bench.run(|step| eprintln!("bench: {step}"))?;
            write!(out, "{}", bench::render_table(&results))?;
            if let Some(label) = record {
                let path = bench::record(Path::new("bench_results"), &label, runs, &results)?;
                writeln!(out, "recorded: {}", path.display())?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
