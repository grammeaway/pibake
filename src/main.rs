use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use pibake::bench::{self, Bench};
use pibake::fake_provider::FakeProvider;
use pibake::harness::PiCommand;
use pibake::{bundle, config::Config, inventory, lock, paths, sea, snapshot, stage};

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
    /// Pin Pi, Node, npm and git packages as installed, in pibake.lock beside pibake.toml
    Lock {
        /// Re-pin to what is installed now, instead of reporting drift
        #[arg(long)]
        update: bool,
    },
    /// Install the lock into target/pibake/stage/<hash>/ beside pibake.toml, or reuse it
    Stage,
    /// Stage, then bundle Pi and its extensions into target/pibake/bundle/<hash>/pi.mjs
    Bundle,
    /// Check the lock, stage, bundle, then build target/pibake/bin/<name>, a single executable
    Build,
    /// Build, then copy the executable to ~/.local/bin/<name>
    Install,
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

fn run_stage(config: &Config, inventory: &inventory::Inventory) -> Result<stage::Outcome> {
    let inputs = stage::Inputs::read(&config.dir, inventory)?;
    let stager = stage::Stager {
        root: &config.dir.join(stage::STAGE_ROOT),
        npm: &stage::run_npm,
    };
    stager.run(&inputs, &mut |step| eprintln!("stage: {step}"))
}

fn locker<'a>(config: &'a Config, host: &'a lock::Host) -> lock::Locker<'a> {
    lock::Locker {
        agent_dir: &config.agent_dir,
        dir: &config.dir,
        host,
        npm: &lock::npm_lock_only,
    }
}

fn print_drift(out: &mut dyn Write, items: Vec<String>) -> Result<()> {
    writeln!(out, "the setup has drifted from {}:", lock::LOCK_FILE)?;
    for item in items {
        writeln!(out, "  {item}")?;
    }
    writeln!(out, "run `pibake lock --update` to re-pin")?;
    Ok(())
}

fn run_bundle(
    config: &Config,
    inventory: &inventory::Inventory,
    out: &mut dyn Write,
) -> Result<(PathBuf, PathBuf)> {
    let (stage::Outcome::Cached(stage_dir) | stage::Outcome::Built(stage_dir)) =
        run_stage(config, inventory)?;
    let plan = bundle::Plan::new(inventory, &stage_dir)?;
    let bundler = bundle::Bundler {
        root: &config.dir.join(bundle::BUNDLE_ROOT),
        build: &bundle::run_node_build,
    };
    let (outcome, report) = bundler.run(&plan, &mut |step| eprintln!("bundle: {step}"))?;
    write!(out, "{}", report.summary())?;
    let dir = match outcome {
        stage::Outcome::Cached(dir) => {
            writeln!(
                out,
                "bundle up to date: {}",
                dir.join(bundle::LAUNCHER).display()
            )?;
            dir
        }
        stage::Outcome::Built(dir) => {
            writeln!(out, "bundled {}", dir.join(bundle::LAUNCHER).display())?;
            dir
        }
    };
    Ok((stage_dir, dir))
}

/// The built executable, or `None` when the setup drifted from the lock (reported to `out`).
fn run_build(config: &Config, home: &Path, out: &mut dyn Write) -> Result<Option<PathBuf>> {
    let inventory = inventory::read(&config.agent_dir, home)?;
    let host = lock::Host::find()?;
    if let lock::Outcome::Drift(items) =
        locker(config, &host).run(&inventory, false, &mut |w| eprintln!("warning: {w}"))?
    {
        print_drift(out, items)?;
        return Ok(None);
    }
    let (stage_dir, bundle_dir) = run_bundle(config, &inventory, out)?;
    let lock_text = std::fs::read(config.dir.join(lock::LOCK_FILE))?;
    let pinned = lock::Lock::parse(&String::from_utf8_lossy(&lock_text))?;
    let version = format!(
        "{} (pibake {}, lock {})",
        pinned.pi.version,
        env!("CARGO_PKG_VERSION"),
        &lock::sha256_hex(&lock_text)[..12]
    );
    let output = config.dir.join(sea::BIN_ROOT).join(&config.name);
    eprintln!("build: node --build-sea");
    sea::Builder {
        stage: &stage_dir,
        bundle: &bundle_dir,
        version: &version,
        output: &output,
        node: &sea::run_node,
    }
    .run()?;
    writeln!(out, "built {}", output.display())?;
    Ok(Some(output))
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
        Command::Lock { update } => {
            let inventory = inventory::read(&config.agent_dir, &home)?;
            let host = lock::Host::find()?;
            match locker(&config, &host)
                .run(&inventory, update, &mut |w| eprintln!("warning: {w}"))?
            {
                lock::Outcome::UpToDate => writeln!(out, "lock up to date")?,
                lock::Outcome::Written(files) if files.is_empty() => {
                    writeln!(out, "re-pinned; lock unchanged")?
                }
                lock::Outcome::Written(files) => {
                    for f in files {
                        writeln!(out, "wrote {}", f.display())?;
                    }
                }
                lock::Outcome::Drift(items) => {
                    print_drift(&mut out, items)?;
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Command::Stage => {
            let inventory = inventory::read(&config.agent_dir, &home)?;
            match run_stage(&config, &inventory)? {
                stage::Outcome::Cached(dir) => {
                    writeln!(out, "stage up to date: {}", dir.display())?
                }
                stage::Outcome::Built(dir) => writeln!(out, "staged {}", dir.display())?,
            }
        }
        Command::Bundle => {
            let inventory = inventory::read(&config.agent_dir, &home)?;
            run_bundle(&config, &inventory, &mut out)?;
        }
        Command::Build => {
            if run_build(&config, &home, &mut out)?.is_none() {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Install => {
            let Some(built) = run_build(&config, &home, &mut out)? else {
                return Ok(ExitCode::FAILURE);
            };
            let to = home.join(".local/bin").join(&config.name);
            sea::install(&built, &to)?;
            writeln!(out, "installed {}", to.display())?;
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
