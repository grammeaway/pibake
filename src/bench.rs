//! Startup (RPC and interactive) and big-write measurements of Pi commands, interleaved, against
//! the fake provider.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::json;

use crate::fake_provider::{
    self, BIG_WRITE_PATH, DONE_TEXT, FakeProvider, LONG_ANSWER_END, PROVIDER, SESSION_FILE,
    SESSION_FILE_BYTES, SESSION_TURN_END, Script, UNEXPECTED_TEXT,
};
use crate::harness::{Harness, PiCommand};
use crate::process::{self, Supervised, Usage};

const TIMEOUT: Duration = Duration::from_secs(300);
const BIG_WRITE_PROMPT: &str = "Write the file.";
const LONG_ANSWER_PROMPT: &str = "Answer at length.";
const BIG_OUTPUT_PROMPT: &str = "Print the numbers.";
const SESSION_PROMPT: &str = "Read the file, then answer.";
const SESSION_TURNS: usize = 40;
/// Exits Pi from an empty editor.
const CTRL_D: &[u8] = b"\x04";
/// A VT220's answer to DA1, as a real terminal gives it within a millisecond; without it, Pi
/// waits 100 ms for its colour query.
const DA1_REPLY: &[u8] = b"\x1b[?62c";

/// `--perf-stat` writes its counters here, in the run's harness.
const PERF_STAT_FILE: &str = "perf_stat.csv";
const PERF_STAT_EVENT: &str = "instructions:u";

/// What `bench` measures; each run checks its own end state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum Scenario {
    /// RPC startup, until the `get_state` response.
    Startup,
    /// Interactive startup in a pty, run to its end by `PI_STARTUP_BENCHMARK=1`.
    Interactive,
    /// `-p`: a streamed write of `--big-write-kib`.
    BigWrite,
    /// pty: a 20 KiB Markdown answer, paced.
    LongAnswer,
    /// pty: the `big_write` turn in the TUI.
    TuiWrite,
    /// pty: a `bash` call printing 8 MiB.
    BigOutput,
    /// RPC, session kept: 40 turns, each reading a 16 KiB file and answering 2 KiB.
    LongSession,
}

impl Scenario {
    pub const ALL: [Scenario; 7] = [
        Scenario::Startup,
        Scenario::Interactive,
        Scenario::BigWrite,
        Scenario::LongAnswer,
        Scenario::TuiWrite,
        Scenario::BigOutput,
        Scenario::LongSession,
    ];

    /// `*_minstr`: millions of user-space instructions, with `--perf-stat`.
    pub fn metrics(self) -> &'static [&'static str] {
        match self {
            Scenario::Startup => &[
                "startup_ms",
                "startup_cpu_ms",
                "startup_peak_rss_mib",
                "startup_minstr",
            ],
            Scenario::Interactive => &[
                "first_paint_ms",
                "ready_ms",
                "interactive_cpu_ms",
                "interactive_peak_rss_mib",
                "interactive_minstr",
            ],
            Scenario::BigWrite => &[
                "big_write_wall_ms",
                "big_write_cpu_ms",
                "big_write_peak_rss_mib",
                "big_write_minstr",
            ],
            Scenario::LongAnswer => &[
                "long_answer_cpu_ms",
                "long_answer_peak_rss_mib",
                "long_answer_minstr",
            ],
            Scenario::TuiWrite => &[
                "tui_write_cpu_ms",
                "tui_write_peak_rss_mib",
                "tui_write_minstr",
            ],
            Scenario::BigOutput => &[
                "big_output_cpu_ms",
                "big_output_peak_rss_mib",
                "big_output_minstr",
            ],
            Scenario::LongSession => &[
                "session_early_turn_cpu_ms",
                "session_late_turn_cpu_ms",
                "session_end_rss_mib",
                "session_peak_rss_mib",
                "session_minstr",
            ],
        }
    }

    fn writes(self) -> bool {
        matches!(self, Scenario::BigWrite | Scenario::TuiWrite)
    }
}

/// Every metric, in table order.
pub fn metrics() -> impl Iterator<Item = &'static str> {
    Scenario::ALL
        .into_iter()
        .flat_map(|s| s.metrics().iter().copied())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub n: usize,
    pub median: f64,
    pub p25: f64,
    pub p75: f64,
    pub min: f64,
    pub max: f64,
}

impl Summary {
    /// Quantiles by linear interpolation between closest ranks. `None` for no samples.
    pub fn of(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut s = samples.to_vec();
        s.sort_by(f64::total_cmp);
        let q = |p: f64| {
            let rank = p * (s.len() - 1) as f64;
            let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
            s[lo] + (s[hi] - s[lo]) * (rank - lo as f64)
        };
        Some(Self {
            n: s.len(),
            median: q(0.5),
            p25: q(0.25),
            p75: q(0.75),
            min: s[0],
            max: s[s.len() - 1],
        })
    }
}

#[derive(Serialize)]
pub struct CommandResult {
    pub command: String,
    pub pi_version: Option<String>,
    pub metrics: BTreeMap<&'static str, Series>,
}

#[derive(Serialize)]
pub struct Series {
    pub summary: Option<Summary>,
    pub samples: Vec<f64>,
}

pub struct Bench<'a> {
    pub commands: &'a [PiCommand],
    pub runs: usize,
    pub scenarios: &'a [Scenario],
    /// 0 skips both writes.
    pub big_write_bytes: usize,
    pub source: &'a Path,
    pub real_home: &'a Path,
    /// Runs each command under `perf stat`, for the `*_minstr` metrics.
    pub perf_stat: bool,
}

impl Bench<'_> {
    /// One discarded warm-up round, then `runs` rounds. Each round runs the scenarios in turn, and
    /// rotates which command goes first.
    pub fn run(&self, mut progress: impl FnMut(&str)) -> Result<Vec<CommandResult>> {
        let provider = FakeProvider::unrecorded(self.big_write_bytes)?;
        let content = fake_provider::big_write_content(self.big_write_bytes);
        let mut samples: Vec<BTreeMap<&'static str, Vec<f64>>> =
            vec![BTreeMap::new(); self.commands.len()];
        for round in 0..=self.runs {
            progress(&if round == 0 {
                "warm-up".to_string()
            } else {
                format!("round {round}/{}", self.runs)
            });
            let order: Vec<usize> = (0..self.commands.len())
                .map(|i| (i + round) % self.commands.len())
                .collect();
            for &scenario in self.scenarios {
                if scenario.writes() && self.big_write_bytes == 0 {
                    continue;
                }
                for &i in &order {
                    let values = self.measure(scenario, &self.commands[i], &provider, &content)?;
                    for (name, value) in scenario.metrics().iter().zip(values) {
                        if let (true, Some(value)) = (round > 0, value) {
                            samples[i].entry(name).or_default().push(value);
                        }
                    }
                }
            }
        }
        self.commands
            .iter()
            .zip(samples)
            .map(|(pi, mut by_metric)| {
                let metrics = metrics()
                    .map(|m| {
                        let samples = by_metric.remove(m).unwrap_or_default();
                        (
                            m,
                            Series {
                                summary: Summary::of(&samples),
                                samples,
                            },
                        )
                    })
                    .collect();
                Ok(CommandResult {
                    command: pi.label.clone(),
                    pi_version: self.pi_version(pi, &provider)?,
                    metrics,
                })
            })
            .collect()
    }

    /// One run of `scenario`: a value per `scenario.metrics()`, in order.
    fn measure(
        &self,
        scenario: Scenario,
        pi: &PiCommand,
        provider: &FakeProvider,
        content: &str,
    ) -> Result<Vec<Option<f64>>> {
        Ok(match scenario {
            Scenario::Startup => {
                let (ready, usage, minstr) = self.startup(pi, provider)?;
                vec![ms(ready), ms(usage.cpu()), mib(usage.max_rss_kib), minstr]
            }
            Scenario::Interactive => {
                let (first_paint, ready, usage, minstr) = self.interactive(pi, provider)?;
                vec![
                    ms(first_paint),
                    ms(ready),
                    ms(usage.cpu()),
                    mib(usage.max_rss_kib),
                    minstr,
                ]
            }
            Scenario::BigWrite => {
                let (usage, minstr) = self.big_write(pi, provider, content)?;
                vec![
                    ms(usage.wall),
                    ms(usage.cpu()),
                    mib(usage.max_rss_kib),
                    minstr,
                ]
            }
            Scenario::LongAnswer | Scenario::TuiWrite | Scenario::BigOutput => {
                let (usage, minstr) =
                    self.tui_turn(pi, provider, scenario, |harness| match scenario {
                        Scenario::TuiWrite => check_written(harness, content),
                        _ => Ok(()),
                    })?;
                vec![ms(usage.cpu()), mib(usage.max_rss_kib), minstr]
            }
            Scenario::LongSession => {
                let session = self.long_session(pi, provider)?;
                let (early, late) = turn_medians(&session.turn_cpu);
                vec![
                    early,
                    late,
                    mib(session.end_rss_kib),
                    mib(session.peak_rss_kib),
                    session.minstr,
                ]
            }
        })
    }

    fn harness(&self, provider: &FakeProvider) -> Result<Harness> {
        Harness::new(self.source, self.real_home, &provider.base_url())
    }

    /// `harness.command`, under `perf stat` with `--perf-stat`.
    fn command(&self, harness: &Harness, pi: &PiCommand, log: &str) -> Result<Command> {
        if !self.perf_stat {
            return harness.command(pi, log);
        }
        let out = harness.root().join(PERF_STAT_FILE);
        let args = ["stat", "-x,", "-e", PERF_STAT_EVENT, "-o"].map(String::from);
        let perf = PiCommand {
            label: pi.label.clone(),
            program: "perf".into(),
            args: args
                .into_iter()
                .chain([
                    out.to_string_lossy().into(),
                    "--".into(),
                    pi.program.clone(),
                ])
                .chain(pi.args.iter().cloned())
                .collect(),
        };
        harness.command(&perf, log)
    }

    /// The counter `perf stat` wrote for the harness's last run, in millions.
    fn minstr(&self, harness: &Harness) -> Result<Option<f64>> {
        if !self.perf_stat {
            return Ok(None);
        }
        let path = harness.root().join(PERF_STAT_FILE);
        let csv = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(perf_stat_count(&csv, PERF_STAT_EVENT)? / 1e6))
    }

    /// Time from spawn until the `get_state` response, plus the whole process's usage.
    pub(crate) fn startup(
        &self,
        pi: &PiCommand,
        provider: &FakeProvider,
    ) -> Result<(Duration, Usage, Option<f64>)> {
        let harness = self.harness(provider)?;
        let log = "startup.log";
        let run = || -> Result<(Duration, Usage, Option<f64>)> {
            let mut cmd = self.command(&harness, pi, log)?;
            cmd.args(["--mode", "rpc", "--no-session", "--provider", PROVIDER])
                .args(["--model", Script::Hello.model_id()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut run = Supervised::spawn(cmd, TIMEOUT)?;
            let mut stdin = run.child.stdin.take().context("no stdin")?;
            writeln!(
                stdin,
                "{}",
                json!({ "id": "pibake_state", "type": "get_state" })
            )?;
            let mut lines = BufReader::new(run.child.stdout.take().context("no stdout")?).lines();
            let ready = loop {
                let line = lines
                    .next()
                    .context("Pi exited before answering get_state")??;
                if serde_json::from_str::<serde_json::Value>(&line)
                    .is_ok_and(|e| e["id"] == "pibake_state")
                {
                    break run.started.elapsed();
                }
            };
            drop(stdin);
            for line in lines {
                line?;
            }
            let (status, usage) = run.wait()?;
            if !status.success() {
                bail!("exited with {status}");
            }
            Ok((ready, usage, self.minstr(&harness)?))
        };
        run().with_context(|| {
            format!(
                "startup of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    /// `pi` in a 120x40 pty with `args`. With `until`, the reader quits Pi with Ctrl+D once the
    /// screen shows that text or the fake provider's unexpected reply. Fails unless Pi exits 0.
    fn pty_run(
        &self,
        harness: &Harness,
        pi: &PiCommand,
        log: &str,
        args: &[&str],
        until: Option<&'static str>,
    ) -> Result<(Screen, Usage, Option<f64>)> {
        let (master, tty) = process::openpty(120, 40)?;
        let mut cmd = self.command(harness, pi, log)?;
        cmd.args(args).env("TERM", "xterm-256color");
        if until.is_none() {
            cmd.env("PI_STARTUP_BENCHMARK", "1");
        }
        let run = Supervised::spawn_in_pty(cmd, tty, TIMEOUT)?;
        let started = run.started;
        let reader = std::thread::spawn(move || read_screen(master, started, until));
        let (status, usage) = run.wait()?;
        let screen = reader
            .join()
            .map_err(|_| anyhow!("the pty reader panicked"))??;
        if !status.success() {
            bail!("exited with {status}");
        }
        Ok((screen, usage, self.minstr(harness)?))
    }

    /// Interactive startup in a pty, run to its end by `PI_STARTUP_BENCHMARK=1`: time from spawn to
    /// the first frame and to the last one before teardown (`init()`'s closing `renderNow()`), plus
    /// the whole process's usage. Fails unless a frame came and the screen showed the fake model.
    pub(crate) fn interactive(
        &self,
        pi: &PiCommand,
        provider: &FakeProvider,
    ) -> Result<(Duration, Duration, Usage, Option<f64>)> {
        let harness = self.harness(provider)?;
        let log = "interactive.log";
        let model = Script::Hello.model_id();
        let run = || -> Result<(Duration, Duration, Usage, Option<f64>)> {
            let args = ["--no-session", "--provider", PROVIDER, "--model", model];
            let (screen, usage, minstr) = self.pty_run(&harness, pi, log, &args, None)?;
            let (Some(&first_paint), Some(&ready)) = (screen.frames.first(), screen.frames.last())
            else {
                bail!("no frame drawn");
            };
            if screen.find(model).is_none() {
                bail!("the screen never showed the model `{model}`");
            }
            Ok((first_paint, ready, usage, minstr))
        };
        run().with_context(|| {
            format!(
                "interactive startup of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    /// One prompt in the TUI, quit as a user would once its closing text is on screen; the
    /// whole process's usage. `check` then looks at what the turn left in the harness.
    pub(crate) fn tui_turn(
        &self,
        pi: &PiCommand,
        provider: &FakeProvider,
        scenario: Scenario,
        check: impl FnOnce(&Harness) -> Result<()>,
    ) -> Result<(Usage, Option<f64>)> {
        let (script, prompt, end) = match scenario {
            Scenario::LongAnswer => (Script::LongAnswer, LONG_ANSWER_PROMPT, LONG_ANSWER_END),
            Scenario::TuiWrite => (Script::BigWrite, BIG_WRITE_PROMPT, DONE_TEXT),
            Scenario::BigOutput => (Script::BigOutput, BIG_OUTPUT_PROMPT, DONE_TEXT),
            _ => bail!("{scenario:?} is not a TUI turn"),
        };
        let harness = self.harness(provider)?;
        let log = format!("{}.log", script.model_id());
        let run = || -> Result<(Usage, Option<f64>)> {
            let args = [
                "--no-session",
                "--provider",
                PROVIDER,
                "--model",
                script.model_id(),
                prompt,
            ];
            let (screen, usage, minstr) = self.pty_run(&harness, pi, &log, &args, Some(end))?;
            if let Some(at) = screen.find(UNEXPECTED_TEXT) {
                let reply = &screen.text[at..screen.text.len().min(at + 80)];
                bail!(
                    "the fake provider got an unexpected request: {}",
                    String::from_utf8_lossy(reply)
                );
            }
            if screen.find(end).is_none() {
                bail!("the screen never showed `{end}`");
            }
            check(&harness)?;
            Ok((usage, minstr))
        };
        run().with_context(|| {
            format!(
                "{scenario:?} of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(&log)
            )
        })
    }

    /// `SESSION_TURNS` prompts over RPC, the session kept in the harness. Each turn's CPU is the
    /// Pi process's, from sending the prompt to `agent_settled`; each must end in the scripted
    /// answer.
    pub(crate) fn long_session(&self, pi: &PiCommand, provider: &FakeProvider) -> Result<Session> {
        let harness = self.harness(provider)?;
        let log = "long_session.log";
        let file = harness.cwd.join(SESSION_FILE);
        std::fs::write(
            &file,
            fake_provider::session_file_content(SESSION_FILE_BYTES),
        )
        .with_context(|| format!("writing {}", file.display()))?;
        let run = || -> Result<Session> {
            let mut cmd = self.command(&harness, pi, log)?;
            cmd.args(["--mode", "rpc", "--provider", PROVIDER])
                .args(["--model", Script::LongSession.model_id()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut run = Supervised::spawn(cmd, TIMEOUT)?;
            let mut stdin = run.child.stdin.take().context("no stdin")?;
            let mut lines = BufReader::new(run.child.stdout.take().context("no stdout")?).lines();
            let mut next = || -> Result<serde_json::Value> {
                let line = lines.next().context("Pi exited mid-session")??;
                serde_json::from_str(&line).with_context(|| format!("not JSON: {line}"))
            };
            writeln!(
                stdin,
                "{}",
                json!({ "id": "pibake_state", "type": "get_state" })
            )?;
            while next()?["id"] != "pibake_state" {}
            let pid = process::busiest_in_tree(run.child.id())?;
            let mut turn_cpu = Vec::with_capacity(SESSION_TURNS);
            // An extension may compact between turns (pi-observational-memory does, after
            // `agent_settled`); Pi rejects a prompt meanwhile, so it is sent again afterwards.
            let mut compacting = false;
            for turn in 1..=SESSION_TURNS {
                let id = format!("turn_{turn}");
                let prompt = json!({ "id": id, "type": "prompt", "message": SESSION_PROMPT });
                let before = process::cpu_time(pid)?;
                writeln!(stdin, "{prompt}")?;
                let (mut answered, mut settled, mut rejected) = (false, false, false);
                while !settled || compacting || rejected {
                    let event = next()?;
                    match event["type"].as_str() {
                        Some("compaction_start") => compacting = true,
                        Some("compaction_end") => {
                            compacting = false;
                            if rejected {
                                writeln!(stdin, "{prompt}")?;
                                rejected = false;
                            }
                        }
                        Some("response") if event["id"] == id && event["success"] != true => {
                            if !compacting {
                                bail!("turn {turn}: {event}");
                            }
                            rejected = true;
                        }
                        Some("message_end") if event["message"]["role"] == "assistant" => {
                            answered |= fake_provider::content_text(&event["message"])
                                .contains(SESSION_TURN_END);
                        }
                        Some("agent_settled") => settled = true,
                        _ => {}
                    }
                }
                turn_cpu.push(process::cpu_time(pid)? - before);
                if !answered {
                    bail!("turn {turn} settled without the scripted answer");
                }
            }
            let (end_rss_kib, peak_rss_kib) = process::memory_kib(pid)?;
            drop(stdin);
            while next().is_ok() {}
            let (status, _) = run.wait()?;
            if !status.success() {
                bail!("exited with {status}");
            }
            Ok(Session {
                turn_cpu,
                end_rss_kib,
                peak_rss_kib,
                minstr: self.minstr(&harness)?,
            })
        };
        run().with_context(|| {
            format!(
                "long session of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    /// `-p` with the `big_write` script; checks the written file.
    pub(crate) fn big_write(
        &self,
        pi: &PiCommand,
        provider: &FakeProvider,
        content: &str,
    ) -> Result<(Usage, Option<f64>)> {
        let harness = self.harness(provider)?;
        let log = "big_write.log";
        let run = || -> Result<(Usage, Option<f64>)> {
            let mut cmd = self.command(&harness, pi, log)?;
            cmd.args(["-p", "--no-session", "--provider", PROVIDER])
                .args(["--model", Script::BigWrite.model_id(), BIG_WRITE_PROMPT]);
            let (status, usage) = Supervised::spawn(cmd, TIMEOUT)?.wait()?;
            if !status.success() {
                bail!("exited with {status}");
            }
            check_written(&harness, content)?;
            Ok((usage, self.minstr(&harness)?))
        };
        run().with_context(|| {
            format!(
                "big write of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    fn pi_version(&self, pi: &PiCommand, provider: &FakeProvider) -> Result<Option<String>> {
        let harness = self.harness(provider)?;
        let out = harness
            .command(pi, "version.log")?
            .arg("--version")
            .stdout(Stdio::piped())
            .output()?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.status.success() && !version.is_empty()).then_some(version))
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    turn_cpu: Vec<Duration>,
    /// `VmRSS` and `VmHWM` after the last turn.
    end_rss_kib: u64,
    peak_rss_kib: u64,
    minstr: Option<f64>,
}

fn check_written(harness: &Harness, content: &str) -> Result<()> {
    let path = harness.cwd.join(BIG_WRITE_PATH);
    let written = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if written != content.as_bytes() {
        bail!(
            "{} differs from the scripted content ({} bytes vs {})",
            path.display(),
            written.len(),
            content.len()
        );
    }
    Ok(())
}

/// The medians of turns 2-6 and 36-40 (1-based), in ms: early against late is the growth.
fn turn_medians(turn_cpu: &[Duration]) -> (Option<f64>, Option<f64>) {
    let median = |turns: std::ops::RangeInclusive<usize>| {
        let ms: Vec<f64> = turns
            .filter_map(|t| turn_cpu.get(t - 1))
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        Summary::of(&ms).map(|s| s.median)
    };
    (median(2..=6), median(36..=40))
}

/// Drains the pty until nothing holds its terminal side any more, answering DA1 queries. With
/// `until`, sends Ctrl+D once, when the drawn text first shows it or `UNEXPECTED_TEXT`.
fn read_screen(
    mut master: File,
    started: Instant,
    until: Option<&'static str>,
) -> std::io::Result<Screen> {
    let mut reply = master.try_clone()?;
    let mut screen = Screen::default();
    let mut buf = [0; 64 * 1024];
    let mut watch = until.map(|u| [u, UNEXPECTED_TEXT]);
    let mut scanned: usize = 0;
    loop {
        let n = match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // Linux reports a hung-up pty as EIO.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for _ in 0..screen.feed(&buf[..n], started.elapsed()) {
            reply.write_all(DA1_REPLY)?;
        }
        if let Some(texts) = watch {
            // Only the new text, plus enough before it for a marker split across reads.
            let longest = texts.iter().map(|t| t.len()).max().unwrap_or(0);
            let from = scanned.saturating_sub(longest);
            if texts.iter().any(|t| screen.find_from(from, t).is_some()) {
                reply.write_all(CTRL_D)?;
                watch = None;
            }
            scanned = screen.text.len();
        }
    }
    Ok(screen)
}

/// Pi's terminal output, parsed byte by byte, so sequences split across reads are still found.
/// Frames are synchronized-output blocks (`CSI ? 2026 h` … `CSI ? 2026 l`); only frames that draw
/// text count, which leaves out the fullscreen renderer's no-change frames (e.g. the re-render
/// after the highlight grammars load) and everything from the alt-screen exit (teardown) on.
#[derive(Debug, Default)]
pub struct Screen {
    state: State,
    params: Vec<u8>,
    in_frame: bool,
    drew: bool,
    torn_down: bool,
    /// When each frame that drew text ended.
    pub frames: Vec<Duration>,
    /// The text those frames drew, escape sequences left out.
    pub text: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC, DCS, APC, PM or SOS, up to BEL or ST.
    Str,
    StrEsc,
}

impl Screen {
    /// Where `needle` first starts in the drawn text.
    pub fn find(&self, needle: &str) -> Option<usize> {
        self.find_from(0, needle)
    }

    fn find_from(&self, from: usize, needle: &str) -> Option<usize> {
        self.text
            .get(from..)?
            .windows(needle.len())
            .position(|w| w == needle.as_bytes())
            .map(|i| from + i)
    }

    /// Feeds bytes read at `at`; returns how many DA1 queries (`CSI c`) they completed.
    pub fn feed(&mut self, bytes: &[u8], at: Duration) -> usize {
        let mut queries = 0;
        for &b in bytes {
            self.state = match self.state {
                State::Ground => match b {
                    0x1b => State::Esc,
                    0x20..=0x7e | 0x80.. => {
                        if self.in_frame && !self.torn_down {
                            self.drew = true;
                            self.text.push(b);
                        }
                        State::Ground
                    }
                    _ => State::Ground,
                },
                State::StrEsc if b == b'\\' => State::Ground,
                State::Esc | State::StrEsc => match b {
                    b'[' => {
                        self.params.clear();
                        State::Csi
                    }
                    b']' | b'P' | b'_' | b'^' | b'X' => State::Str,
                    // Intermediates, e.g. `ESC ( B`.
                    0x1b | 0x20..=0x2f => State::Esc,
                    _ => State::Ground,
                },
                State::Csi => match b {
                    0x40..=0x7e => {
                        queries += usize::from(self.end_csi(b, at));
                        State::Ground
                    }
                    0x1b => State::Esc,
                    _ => {
                        // Longer than any sequence matched below, so a truncated one never matches.
                        if self.params.len() < 16 {
                            self.params.push(b);
                        }
                        State::Csi
                    }
                },
                State::Str => match b {
                    0x07 => State::Ground,
                    0x1b => State::StrEsc,
                    _ => State::Str,
                },
            };
        }
        queries
    }

    /// Whether the sequence was a DA1 query.
    fn end_csi(&mut self, final_byte: u8, at: Duration) -> bool {
        match (self.params.as_slice(), final_byte) {
            (b"" | b"0", b'c') => return true,
            (b"?2026", b'h') => (self.in_frame, self.drew) = (true, false),
            (b"?2026", b'l') => {
                if self.in_frame && self.drew && !self.torn_down {
                    self.frames.push(at);
                }
                self.in_frame = false;
            }
            (b"?1049", b'l') => self.torn_down = true,
            _ => {}
        }
        false
    }
}

/// The count of `event` in `perf stat -x,` output.
fn perf_stat_count(csv: &str, event: &str) -> Result<f64> {
    let fields: Vec<&str> = csv
        .lines()
        .map(|l| l.split(',').collect::<Vec<_>>())
        .find(|f| f.get(2) == Some(&event))
        .with_context(|| format!("perf stat reported no {event}:\n{csv}"))?;
    fields[0]
        .parse()
        .map_err(|_| anyhow!("perf stat could not count {event}: {}", fields[0]))
}

fn ms(d: Duration) -> Option<f64> {
    Some(d.as_secs_f64() * 1000.0)
}

fn mib(kib: u64) -> Option<f64> {
    Some(kib as f64 / 1024.0)
}

/// Medians with the interquartile range, one column per command; metrics no command has are
/// left out.
pub fn render_table(results: &[CommandResult]) -> String {
    let mut rows = vec![
        std::iter::once("metric".to_string())
            .chain(results.iter().map(|r| r.command.clone()))
            .collect::<Vec<_>>(),
    ];
    for metric in metrics() {
        if results.iter().all(|r| r.metrics[metric].summary.is_none()) {
            continue;
        }
        let cells = results.iter().map(|r| match &r.metrics[metric].summary {
            Some(s) => format!("{:.1} [{:.1}–{:.1}]", s.median, s.p25, s.p75),
            None => "-".to_string(),
        });
        rows.push(std::iter::once(metric.to_string()).chain(cells).collect());
    }
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}", w = *w))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// Writes `<dir>/<date>_<label>.json`; refuses to overwrite an earlier record.
pub fn record(
    dir: &Path,
    label: &str,
    bench: &Bench,
    results: &[CommandResult],
) -> Result<PathBuf> {
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("--record label must be non-empty [A-Za-z0-9_-]");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let date = utc_date(now);
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let report = json!({
        "label": label,
        "date": date,
        "unix_time": now,
        "commit": git(&["rev-parse", "HEAD"]),
        "dirty": git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()),
        "node_version": Command::new("node").arg("--version").output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
        "runs": bench.runs,
        "scenarios": bench.scenarios,
        "big_write_bytes": bench.big_write_bytes,
        "delta_bytes": fake_provider::DELTA_BYTES,
        "results": results,
    });
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{date}_{label}.json"));
    let mut file =
        std::fs::File::create_new(&path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(path)
}

/// `YYYY-MM-DD` for a Unix time (Howard Hinnant's civil-from-days).
pub fn utc_date(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_792_108_800), "2026-10-16");
        assert_eq!(utc_date(4_107_542_399), "2100-02-28");
    }

    #[test]
    fn reads_the_version_through_the_harness() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_pi");
        std::fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = --version ] && echo \" 9.9.9\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir");
        let bench = Bench {
            commands: &[],
            runs: 1,
            scenarios: &[],
            big_write_bytes: fake_provider::BIG_WRITE_BYTES,
            source: &fixture,
            perf_stat: false,
            real_home: dir.path(),
        };
        let provider = FakeProvider::start().unwrap();
        let pi = PiCommand::parse(&script.to_string_lossy()).unwrap();
        assert_eq!(
            bench.pi_version(&pi, &provider).unwrap().as_deref(),
            Some("9.9.9")
        );
        let failing = PiCommand::parse("false").unwrap();
        assert_eq!(bench.pi_version(&failing, &provider).unwrap(), None);
    }

    /// A fullscreen startup as Pi 1.3.0 writes it, shortened: one read per entry.
    const CAPTURE: [&[u8]; 10] = [
        b"\x1b[?1049h\x1b[?1000h\x1b[2J\x1b[H\x1b[?25l",
        // Keyboard protocol and program status queries, closed by DA1.
        b"\x1b[>7u\x1b[?u\x1b]7501;?\x1b\\\x1b[c",
        // The colour query, closed by DA1.
        b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;0;?\x07\x1b[c",
        // First paint: editor and footer.
        b"\x1b[?2026h\x1b[2J\x1b[1;1H\x1b[2K\x1b]8;;\x07\x1b[40;1H\x1b[38;5;116m\xe2\x9c\xa6 hello\x1b[39m\x1b[37;1H\x1b[?25l\x1b[?2026l",
        b"\x1b]0;\xcf\x80 - work\x07",
        // `renderNow()`: the header.
        b"\x1b[?2026h\x1b[3;1H\x1b[2K\x1b[1m\xe2\x96\x88 pi\x1b[22m\x1b[38;1H\x1b[?25l\x1b[?2026l",
        // The re-render once the highlight grammars load: nothing changed.
        b"\x1b[?2026h\x1b[38;1H\x1b[?25l\x1b[?2026l",
        // Teardown, then the transcript on the main screen.
        b"\x1b[?2026h\x1b[?1006l\x1b[?1000l\x1b[?7h\x1b[?2026l\x1b[?25h",
        b"\x1b[?2026h\x1b[?1049l\x1b[?25h\x1b[?2026l",
        b"\x1b[?2026h\x1b[0m\r\r\n\xe2\x96\x88 pi\r\r\nhello\x1b[?2026l",
    ];

    #[test]
    fn screen_finds_drawing_frames_and_da1_queries() {
        let mut screen = Screen::default();
        let queries: Vec<usize> = (0..CAPTURE.len())
            .map(|i| screen.feed(CAPTURE[i], Duration::from_millis(i as u64)))
            .collect();
        assert_eq!(queries, [0, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            screen.frames,
            [Duration::from_millis(3), Duration::from_millis(5)]
        );
        assert_eq!(
            String::from_utf8_lossy(&screen.text),
            "\u{2726} hello\u{2588} pi"
        );
    }

    #[test]
    fn screen_finds_markers_split_across_reads() {
        let bytes = CAPTURE.concat();
        let mut screen = Screen::default();
        let mut queries = vec![];
        for (i, b) in bytes.iter().enumerate() {
            if screen.feed(&[*b], Duration::from_nanos(i as u64)) > 0 {
                queries.push(i);
            }
        }
        let ends = |pattern: &[u8]| -> Vec<usize> {
            (0..bytes.len())
                .filter(|&i| bytes[i..].starts_with(pattern))
                .map(|i| i + pattern.len() - 1)
                .collect()
        };
        assert_eq!(queries, ends(b"\x1b[c"));
        let frame_ends = ends(b"\x1b[?2026l");
        let frames: Vec<u64> = screen.frames.iter().map(|d| d.as_nanos() as u64).collect();
        assert_eq!(frames, [frame_ends[0] as u64, frame_ends[1] as u64]);
    }

    #[test]
    fn interactive_runs_need_a_frame_showing_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir");
        let bench = test_bench(&fixture, dir.path());
        let provider = FakeProvider::start().unwrap();
        let run =
            |name: &str, body: &str| bench.interactive(&fake_pi(dir.path(), name, body), &provider);
        let (first_paint, ready, _, _) = run(
            "ok",
            &format!("{}; sleep 0.1; {}", frame("hi"), frame("hello")),
        )
        .unwrap();
        assert!(first_paint + Duration::from_millis(50) < ready);
        let err = |name: &str, body: &str| format!("{:#}", run(name, body).unwrap_err());
        assert!(err("blank", "true").contains("no frame drawn"));
        assert!(err("other", &frame("hi")).contains("never showed the model"));
        assert!(err("failing", &format!("{}; exit 3", frame("hello"))).contains("exit status: 3"));
    }

    fn test_bench<'a>(fixture: &'a Path, home: &'a Path) -> Bench<'a> {
        Bench {
            commands: &[],
            runs: 1,
            scenarios: &[],
            big_write_bytes: 0,
            source: fixture,
            perf_stat: false,
            real_home: home,
        }
    }

    /// A shell script standing in for Pi.
    fn fake_pi(dir: &Path, name: &str, body: &str) -> PiCommand {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join(name);
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        PiCommand::parse(&script.to_string_lossy()).unwrap()
    }

    fn frame(text: &str) -> String {
        format!("printf '\\033[?2026h{text}\\033[?2026l'")
    }

    #[test]
    fn tui_turns_quit_with_ctrl_d_once_the_closing_text_shows() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir");
        let bench = test_bench(&fixture, dir.path());
        let provider = FakeProvider::start().unwrap();
        let run = |name: &str, body: &str| {
            let pi = fake_pi(dir.path(), name, body);
            bench.tui_turn(&pi, &provider, Scenario::BigOutput, |_| Ok(()))
        };
        // The canonical-mode pty turns Ctrl+D on an empty line into EOF; `read` then fails.
        let wait_for_eof = "while read -r line; do :; done";
        run(
            "ok",
            &format!("{}; {wait_for_eof}", frame("pibake-fake-done")),
        )
        .unwrap();
        let split = "printf '\\033[?2026hpibake-fa'; sleep 0.05; printf 'ke-done\\033[?2026l'";
        run("split", &format!("{split}; {wait_for_eof}")).unwrap();
        let err = |name: &str, body: &str| format!("{:#}", run(name, body).unwrap_err());
        assert!(err("early_exit", &frame("pibake")).contains("never showed `pibake-fake-done`"));
        assert!(
            err(
                "unexpected",
                &format!(
                    "{}; {wait_for_eof}",
                    frame("pibake-fake-unexpected: the output lacks")
                )
            )
            .contains("unexpected request: pibake-fake-unexpected: the output lacks")
        );
        assert!(
            err("failing", &format!("{}; exit 3", frame("pibake-fake-done")))
                .contains("exit status: 3")
        );
        let checked = bench.tui_turn(
            &fake_pi(
                dir.path(),
                "checked",
                &format!("{}; {wait_for_eof}", frame("pibake-fake-done")),
            ),
            &provider,
            Scenario::TuiWrite,
            |harness| check_written(harness, "x"),
        );
        assert!(format!("{:#}", checked.unwrap_err()).contains("big_write.txt"));
    }

    /// An RPC session in sh: answers `get_state`, then each prompt with `answer`; prompt 3 is
    /// rejected once during a compaction and sent again after it.
    fn rpc_session(answer: &str) -> String {
        format!(
            r#"read -r line
echo '{{"id":"pibake_state","type":"response","success":true}}'
i=0
while read -r line; do
  i=$((i+1))
  if [ $i = 3 ]; then
    echo '{{"type":"compaction_start"}}'
    echo '{{"id":"turn_3","type":"response","success":false}}'
    echo '{{"type":"compaction_end"}}'
    read -r line
  fi
  echo '{{"id":"turn_'$i'","type":"response","success":true}}'
  echo '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"{answer}"}}]}}}}'
  echo '{{"type":"agent_settled"}}'
done"#
        )
    }

    #[test]
    fn a_long_session_runs_every_turn_through_compactions() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir");
        let bench = test_bench(&fixture, dir.path());
        let provider = FakeProvider::start().unwrap();
        let ok = fake_pi(dir.path(), "ok", &rpc_session("text pibake-turn-end"));
        let session = bench.long_session(&ok, &provider).unwrap();
        assert_eq!(session.turn_cpu.len(), SESSION_TURNS);
        assert!(session.peak_rss_kib >= session.end_rss_kib && session.end_rss_kib > 0);
        let wrong = fake_pi(dir.path(), "wrong", &rpc_session("pibake-fake-unexpected"));
        let err = format!("{:#}", bench.long_session(&wrong, &provider).unwrap_err());
        assert!(
            err.contains("turn 1 settled without the scripted answer"),
            "{err}"
        );
        let short = fake_pi(
            dir.path(),
            "short",
            "read -r line; echo '{\"id\":\"pibake_state\"}'; read -r line",
        );
        let err = format!("{:#}", bench.long_session(&short, &provider).unwrap_err());
        assert!(err.contains("Pi exited mid-session"), "{err}");
    }

    #[test]
    fn turn_medians_take_turns_2_to_6_and_36_to_40() {
        let turns: Vec<Duration> = (1..=40).map(|t| Duration::from_millis(t * 10)).collect();
        assert_eq!(turn_medians(&turns), (Some(40.0), Some(380.0)));
        assert_eq!(turn_medians(&turns[..3]), (Some(25.0), None));
        assert_eq!(turn_medians(&[]), (None, None));
    }

    #[test]
    fn scenario_metrics_are_unique_and_named_for_the_cli() {
        let all: Vec<&str> = metrics().collect();
        let unique: std::collections::BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len());
        use clap::ValueEnum;
        let names: Vec<String> = Scenario::value_variants()
            .iter()
            .map(|s| s.to_possible_value().unwrap().get_name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "startup",
                "interactive",
                "big_write",
                "long_answer",
                "tui_write",
                "big_output",
                "long_session"
            ]
        );
        assert_eq!(serde_json::to_value(Scenario::ALL).unwrap(), json!(names));
    }

    #[test]
    fn summary_quantiles() {
        assert_eq!(Summary::of(&[]), None);
        let s = Summary::of(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!((s.n, s.min, s.max, s.median), (4, 1.0, 4.0, 2.5));
        assert_eq!((s.p25, s.p75), (1.75, 3.25));
        assert_eq!(Summary::of(&[7.0]).unwrap().p75, 7.0);
    }

    fn result(command: &str, startup: &[f64]) -> CommandResult {
        let metrics = metrics()
            .map(|m| {
                let samples = if m == "startup_ms" {
                    startup.to_vec()
                } else {
                    vec![]
                };
                (
                    m,
                    Series {
                        summary: Summary::of(&samples),
                        samples,
                    },
                )
            })
            .collect();
        CommandResult {
            command: command.into(),
            pi_version: Some("1.3.0".into()),
            metrics,
        }
    }

    #[test]
    fn reads_a_perf_stat_count() {
        let csv = "# started on Sat Oct 17 19:25:42 2026\n\n2898297306,,instructions:u,545170000,100.00,,\n";
        assert_eq!(
            perf_stat_count(csv, "instructions:u").unwrap(),
            2898297306.0
        );
        let err = perf_stat_count(csv, "cycles:u").unwrap_err();
        assert!(err.to_string().contains("no cycles:u"), "{err}");
        let unsupported = "<not supported>,,instructions:u,0,100.00,,\n";
        let err = perf_stat_count(unsupported, "instructions:u").unwrap_err();
        assert!(err.to_string().contains("<not supported>"), "{err}");
    }

    #[test]
    fn table_has_a_column_per_command_and_a_row_per_measured_metric() {
        let table = render_table(&[
            result("pi", &[100.0, 200.0, 300.0]),
            result("pi -ne", &[50.0]),
            result("none", &[]),
        ]);
        let lines: Vec<&str> = table.lines().collect();
        // Only `startup_ms` has samples.
        assert_eq!(lines.len(), 2, "{table}");
        assert!(
            lines[0].starts_with("metric") && lines[0].ends_with("none"),
            "{table}"
        );
        assert!(
            lines[1].contains("200.0 [150.0–250.0]")
                && lines[1].contains("50.0 [50.0–50.0]")
                && lines[1].ends_with('-'),
            "{table}"
        );
    }

    #[test]
    fn record_writes_once_per_label_and_day() {
        let dir = tempfile::tempdir().unwrap();
        let results = [result("pi", &[1.0])];
        let home = tempfile::tempdir().unwrap();
        let bench = Bench {
            big_write_bytes: 100,
            scenarios: &[Scenario::Startup, Scenario::LongSession],
            ..test_bench(Path::new("/unused"), home.path())
        };
        let path = record(dir.path(), "baseline", &bench, &results).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json["results"][0]["metrics"]["startup_ms"]["samples"],
            json!([1.0])
        );
        assert_eq!(json["label"], "baseline");
        assert_eq!(json["big_write_bytes"], 100);
        assert_eq!(json["scenarios"], json!(["startup", "long_session"]));
        assert!(record(dir.path(), "baseline", &bench, &results).is_err());
        assert!(record(dir.path(), "../x", &bench, &results).is_err());
    }
}
