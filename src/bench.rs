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

use crate::fake_provider::{self, BIG_WRITE_PATH, FakeProvider, PROVIDER, Script};
use crate::harness::{Harness, PiCommand};
use crate::process::{self, Supervised, Usage};

const TIMEOUT: Duration = Duration::from_secs(300);
const BIG_WRITE_PROMPT: &str = "Write the file.";
/// A VT220's answer to DA1, as a real terminal gives it within a millisecond; without it, Pi
/// waits 100 ms for its colour query.
const DA1_REPLY: &[u8] = b"\x1b[?62c";

/// `--perf-stat` writes its counters here, in the run's harness.
const PERF_STAT_FILE: &str = "perf_stat.csv";
const PERF_STAT_EVENT: &str = "instructions:u";

/// `*_minstr`: millions of user-space instructions, with `--perf-stat`.
pub const METRICS: [&str; 13] = [
    "startup_ms",
    "startup_cpu_ms",
    "startup_peak_rss_mib",
    "startup_minstr",
    "first_paint_ms",
    "ready_ms",
    "interactive_cpu_ms",
    "interactive_peak_rss_mib",
    "interactive_minstr",
    "big_write_wall_ms",
    "big_write_cpu_ms",
    "big_write_peak_rss_mib",
    "big_write_minstr",
];

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
    pub big_write_bytes: usize,
    pub source: &'a Path,
    pub real_home: &'a Path,
    /// Runs each command under `perf stat`, for the `*_minstr` metrics.
    pub perf_stat: bool,
}

impl Bench<'_> {
    /// One discarded warm-up round, then `runs` rounds. Each round rotates which command goes first.
    pub fn run(&self, mut progress: impl FnMut(&str)) -> Result<Vec<CommandResult>> {
        let provider = FakeProvider::with_big_write(self.big_write_bytes)?;
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
            let mut record = |i: usize, name: &'static str, value: Option<f64>| {
                if let (true, Some(value)) = (round > 0, value) {
                    samples[i].entry(name).or_default().push(value);
                }
            };
            for &i in &order {
                let (ready, usage, minstr) = self.startup(&self.commands[i], &provider)?;
                record(i, "startup_ms", ms(ready));
                record(i, "startup_cpu_ms", ms(usage.cpu()));
                record(i, "startup_peak_rss_mib", mib(usage.max_rss_kib));
                record(i, "startup_minstr", minstr);
            }
            for &i in &order {
                let (first_paint, ready, usage, minstr) =
                    self.interactive(&self.commands[i], &provider)?;
                record(i, "first_paint_ms", ms(first_paint));
                record(i, "ready_ms", ms(ready));
                record(i, "interactive_cpu_ms", ms(usage.cpu()));
                record(i, "interactive_peak_rss_mib", mib(usage.max_rss_kib));
                record(i, "interactive_minstr", minstr);
            }
            for &i in order.iter().filter(|_| self.big_write_bytes > 0) {
                let (usage, minstr) = self.big_write(&self.commands[i], &provider, &content)?;
                record(i, "big_write_wall_ms", ms(usage.wall));
                record(i, "big_write_cpu_ms", ms(usage.cpu()));
                record(i, "big_write_peak_rss_mib", mib(usage.max_rss_kib));
                record(i, "big_write_minstr", minstr);
            }
        }
        self.commands
            .iter()
            .zip(samples)
            .map(|(pi, mut by_metric)| {
                let metrics = METRICS
                    .into_iter()
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
    fn startup(
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

    /// Interactive startup in a pty, run to its end by `PI_STARTUP_BENCHMARK=1`: time from spawn to
    /// the first frame and to the last one before teardown (`init()`'s closing `renderNow()`), plus
    /// the whole process's usage. Fails unless a frame came and the screen showed the fake model.
    fn interactive(
        &self,
        pi: &PiCommand,
        provider: &FakeProvider,
    ) -> Result<(Duration, Duration, Usage, Option<f64>)> {
        let harness = self.harness(provider)?;
        let log = "interactive.log";
        let model = Script::Hello.model_id();
        let run = || -> Result<(Duration, Duration, Usage, Option<f64>)> {
            let (master, tty) = process::openpty(120, 40)?;
            let mut cmd = self.command(&harness, pi, log)?;
            cmd.args(["--no-session", "--provider", PROVIDER, "--model", model])
                .env("PI_STARTUP_BENCHMARK", "1")
                .env("TERM", "xterm-256color");
            let run = Supervised::spawn_in_pty(cmd, tty, TIMEOUT)?;
            let started = run.started;
            let reader = std::thread::spawn(move || read_screen(master, started));
            let (status, usage) = run.wait()?;
            let screen = reader
                .join()
                .map_err(|_| anyhow!("the pty reader panicked"))??;
            if !status.success() {
                bail!("exited with {status}");
            }
            let (Some(&first_paint), Some(&ready)) = (screen.frames.first(), screen.frames.last())
            else {
                bail!("no frame drawn");
            };
            if !screen
                .text
                .windows(model.len())
                .any(|w| w == model.as_bytes())
            {
                bail!("the screen never showed the model `{model}`");
            }
            Ok((first_paint, ready, usage, self.minstr(&harness)?))
        };
        run().with_context(|| {
            format!(
                "interactive startup of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    /// `-p` with the `big_write` script; checks the written file.
    fn big_write(
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
            let path = harness.cwd.join(BIG_WRITE_PATH);
            let written =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if written != content.as_bytes() {
                bail!(
                    "{} differs from the scripted content ({} bytes vs {})",
                    path.display(),
                    written.len(),
                    content.len()
                );
            }
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

/// Drains the pty until nothing holds its terminal side any more, answering DA1 queries.
fn read_screen(mut master: File, started: Instant) -> std::io::Result<Screen> {
    let mut reply = master.try_clone()?;
    let mut screen = Screen::default();
    let mut buf = [0; 64 * 1024];
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

/// Medians with the interquartile range, one column per command.
pub fn render_table(results: &[CommandResult]) -> String {
    let mut rows = vec![
        std::iter::once("metric".to_string())
            .chain(results.iter().map(|r| r.command.clone()))
            .collect::<Vec<_>>(),
    ];
    for metric in METRICS {
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
    runs: usize,
    big_write_bytes: usize,
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
        "runs": runs,
        "big_write_bytes": big_write_bytes,
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
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir");
        let bench = Bench {
            commands: &[],
            runs: 1,
            big_write_bytes: 0,
            source: &fixture,
            perf_stat: false,
            real_home: dir.path(),
        };
        let provider = FakeProvider::start().unwrap();
        let run = |name: &str, body: &str| {
            let script = dir.path().join(name);
            std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let pi = PiCommand::parse(&script.to_string_lossy()).unwrap();
            bench.interactive(&pi, &provider)
        };
        let frame = |text: &str| format!("printf '\\033[?2026h{text}\\033[?2026l'");
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

    #[test]
    fn summary_quantiles() {
        assert_eq!(Summary::of(&[]), None);
        let s = Summary::of(&[4.0, 1.0, 3.0, 2.0]).unwrap();
        assert_eq!((s.n, s.min, s.max, s.median), (4, 1.0, 4.0, 2.5));
        assert_eq!((s.p25, s.p75), (1.75, 3.25));
        assert_eq!(Summary::of(&[7.0]).unwrap().p75, 7.0);
    }

    fn result(command: &str, startup: &[f64]) -> CommandResult {
        let metrics = METRICS
            .into_iter()
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
    fn table_has_a_column_per_command() {
        let table = render_table(&[
            result("pi", &[100.0, 200.0, 300.0]),
            result("pi -ne", &[50.0]),
        ]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 1 + METRICS.len());
        assert!(
            lines[0].starts_with("metric") && lines[0].ends_with("pi -ne"),
            "{table}"
        );
        assert!(
            lines[1].contains("200.0 [150.0–250.0]") && lines[1].ends_with("50.0 [50.0–50.0]"),
            "{table}"
        );
        assert!(lines[2].ends_with('-'), "{table}");
    }

    #[test]
    fn record_writes_once_per_label_and_day() {
        let dir = tempfile::tempdir().unwrap();
        let results = [result("pi", &[1.0])];
        let path = record(dir.path(), "baseline", 1, 100, &results).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json["results"][0]["metrics"]["startup_ms"]["samples"],
            json!([1.0])
        );
        assert_eq!(json["label"], "baseline");
        assert_eq!(json["big_write_bytes"], 100);
        assert!(record(dir.path(), "baseline", 1, 100, &results).is_err());
        assert!(record(dir.path(), "../x", 1, 100, &results).is_err());
    }
}
