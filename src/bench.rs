//! Startup and big-write measurements of Pi commands, interleaved, against the fake provider.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;

use crate::fake_provider::{self, BIG_WRITE_PATH, FakeProvider, PROVIDER, Script};
use crate::harness::{Harness, PiCommand};
use crate::process::{Supervised, Usage};

const TIMEOUT: Duration = Duration::from_secs(300);
const BIG_WRITE_PROMPT: &str = "Write the file.";

pub const METRICS: [&str; 6] = [
    "startup_ms",
    "startup_cpu_ms",
    "startup_peak_rss_mib",
    "big_write_wall_ms",
    "big_write_cpu_ms",
    "big_write_peak_rss_mib",
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
    pub source: &'a Path,
    pub real_home: &'a Path,
}

impl Bench<'_> {
    /// One discarded warm-up round, then `runs` rounds. Each round rotates which command goes first.
    pub fn run(&self, mut progress: impl FnMut(&str)) -> Result<Vec<CommandResult>> {
        let provider = FakeProvider::start()?;
        let content = fake_provider::big_write_content();
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
            let mut record = |i: usize, name: &'static str, value: f64| {
                if round > 0 {
                    samples[i].entry(name).or_default().push(value);
                }
            };
            for &i in &order {
                let (ready, usage) = self.startup(&self.commands[i], &provider)?;
                record(i, "startup_ms", ms(ready));
                record(i, "startup_cpu_ms", ms(usage.cpu()));
                record(i, "startup_peak_rss_mib", mib(usage.max_rss_kib));
            }
            for &i in &order {
                let usage = self.big_write(&self.commands[i], &provider, &content)?;
                record(i, "big_write_wall_ms", ms(usage.wall));
                record(i, "big_write_cpu_ms", ms(usage.cpu()));
                record(i, "big_write_peak_rss_mib", mib(usage.max_rss_kib));
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

    /// Time from spawn until the `get_state` response, plus the whole process's usage.
    fn startup(&self, pi: &PiCommand, provider: &FakeProvider) -> Result<(Duration, Usage)> {
        let harness = self.harness(provider)?;
        let log = "startup.log";
        let run = || -> Result<(Duration, Usage)> {
            let mut cmd = harness.command(pi, log)?;
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
            Ok((ready, usage))
        };
        run().with_context(|| {
            format!(
                "startup of `{}`, stderr:\n{}",
                pi.label,
                harness.log_tail(log)
            )
        })
    }

    /// `-p` with the `big_write` script; checks the written file.
    fn big_write(&self, pi: &PiCommand, provider: &FakeProvider, content: &str) -> Result<Usage> {
        let harness = self.harness(provider)?;
        let log = "big_write.log";
        let run = || -> Result<Usage> {
            let mut cmd = harness.command(pi, log)?;
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
            Ok(usage)
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

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
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
pub fn record(dir: &Path, label: &str, runs: usize, results: &[CommandResult]) -> Result<PathBuf> {
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
        "big_write_bytes": fake_provider::BIG_WRITE_BYTES,
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
            source: &fixture,
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
        let path = record(dir.path(), "baseline", 1, &results).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json["results"][0]["metrics"]["startup_ms"]["samples"],
            json!([1.0])
        );
        assert_eq!(json["label"], "baseline");
        assert!(record(dir.path(), "baseline", 1, &results).is_err());
        assert!(record(dir.path(), "../x", 1, &results).is_err());
    }
}
