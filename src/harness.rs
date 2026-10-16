//! A throwaway home for one Pi run: `$HOME/.pi/agent` built from the source agent directory
//! (state left out), and an empty working directory beside it.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{fake_provider, inventory};

/// Passed through from pibake's environment; everything else is cleared.
const ENV_ALLOWLIST: [&str; 6] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR"];

pub struct Harness {
    root: tempfile::TempDir,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    pub cwd: PathBuf,
}

impl Harness {
    /// Config is copied (files) or linked (directories), install roots are linked, state is skipped.
    /// `~/.agents` is linked from the real home, since Pi loads skills from there.
    pub fn new(source: &Path, real_home: &Path, provider_base_url: &str) -> Result<Self> {
        let inventory = inventory::read(source, real_home)?;
        let root = tempfile::Builder::new().prefix("pibake-").tempdir()?;
        let home = root.path().join("home");
        let agent_dir = home.join(".pi/agent");
        let cwd = root.path().join("work");
        fs::create_dir_all(&agent_dir)?;
        fs::create_dir(&cwd)?;
        let real_agents = real_home.join(".agents");
        if real_agents.exists() {
            symlink(&real_agents, home.join(".agents"))?;
        }
        for name in inventory.config.iter().chain(&inventory.install_roots) {
            let (from, to) = (source.join(name), agent_dir.join(name));
            if name == "models.json" {
                continue;
            }
            if from.is_dir() {
                symlink(&from, &to)
            } else {
                fs::copy(&from, &to).map(|_| ())
            }
            .with_context(|| format!("staging {}", from.display()))?;
        }
        let models = merged_models(&source.join("models.json"), provider_base_url)?;
        fs::write(
            agent_dir.join("models.json"),
            serde_json::to_string_pretty(&models)?,
        )?;
        Ok(Self {
            home,
            agent_dir,
            cwd,
            root,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// `pi` set up to run here: cleared environment, offline, harness home and agent dir.
    /// Stderr goes to `<root>/<log_name>`.
    pub fn command(&self, pi: &PiCommand, log_name: &str) -> Result<Command> {
        let mut cmd = Command::new(&pi.program);
        cmd.args(&pi.args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(
                ENV_ALLOWLIST
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .env("HOME", &self.home)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_TELEMETRY", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.root.path().join(log_name))?);
        Ok(cmd)
    }

    /// The last lines of a stderr log, for error messages.
    pub fn log_tail(&self, log_name: &str) -> String {
        let text = fs::read_to_string(self.root.path().join(log_name)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }
}

/// The source `models.json` with the fake provider added; the user's providers stay as they are.
fn merged_models(source: &Path, base_url: &str) -> Result<Value> {
    let mut models: Value = match fs::read_to_string(source) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", source.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", source.display())),
    };
    let Some(top) = models.as_object_mut() else {
        bail!("{} is not a JSON object", source.display());
    };
    let providers = top
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    let Some(providers) = providers.as_object_mut() else {
        bail!("{}: `providers` is not an object", source.display());
    };
    providers.insert(
        fake_provider::PROVIDER.to_string(),
        fake_provider::provider_entry(base_url),
    );
    Ok(models)
}

/// A Pi command line as given to `--pi`, e.g. `pi` or `pi -ne`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiCommand {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

impl PiCommand {
    // shortcut: split on whitespace, no shell quoting; use a shell-words parser if an argument ever needs spaces.
    pub fn parse(text: &str) -> Result<Self> {
        let mut words = text.split_whitespace().map(str::to_string);
        let program = words.next().context("empty Pi command")?;
        Ok(Self {
            label: text.trim().to_string(),
            program,
            args: words.collect(),
        })
    }

    /// The `@earendil-works/pi-coding-agent` package the program runs from, if it is a stock install.
    pub fn package_dir(&self) -> Option<PathBuf> {
        let program = Path::new(&self.program);
        let found = if program.components().count() > 1 {
            program.to_path_buf()
        } else {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join(program))
                .find(|p| p.is_file())?
        };
        fs::canonicalize(found)
            .ok()?
            .ancestors()
            .find(|dir| {
                fs::read_to_string(dir.join("package.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .is_some_and(|p| p["name"] == "@earendil-works/pi-coding-agent")
            })
            .map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_lines() {
        let cmd = PiCommand::parse("  pi  -ne --offline ").unwrap();
        assert_eq!(cmd.label, "pi  -ne --offline");
        assert_eq!(cmd.program, "pi");
        assert_eq!(cmd.args, ["-ne", "--offline"]);
        assert!(PiCommand::parse("   ").is_err());
    }

    #[test]
    fn finds_the_package_through_a_bin_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("lib/pi");
        fs::create_dir_all(pkg.join("dist")).unwrap();
        fs::write(
            pkg.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent"}"#,
        )
        .unwrap();
        fs::write(pkg.join("dist/cli.js"), "").unwrap();
        // A nested package.json of another name is skipped.
        fs::write(pkg.join("dist/package.json"), r#"{"name":"other"}"#).unwrap();
        symlink(pkg.join("dist/cli.js"), dir.path().join("pi")).unwrap();
        let cmd = PiCommand::parse(&dir.path().join("pi").to_string_lossy()).unwrap();
        assert_eq!(cmd.package_dir(), Some(fs::canonicalize(&pkg).unwrap()));
        let missing = PiCommand::parse("/nonexistent/pi").unwrap();
        assert_eq!(missing.package_dir(), None);
    }

    #[test]
    fn merges_the_fake_provider_into_user_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let merged = merged_models(&path, "http://x/v1").unwrap();
        assert_eq!(merged["providers"]["pibake_fake"]["baseUrl"], "http://x/v1");

        fs::write(
            &path,
            r#"{"providers":{"mine":{"api":"openai-completions"}},"modelOverrides":{}}"#,
        )
        .unwrap();
        let merged = merged_models(&path, "http://x/v1").unwrap();
        assert_eq!(
            merged["providers"]["mine"],
            serde_json::json!({"api": "openai-completions"})
        );
        assert!(merged["modelOverrides"].is_object());
        assert_eq!(
            merged["providers"]["pibake_fake"]["api"],
            "openai-completions"
        );

        fs::write(&path, "[]").unwrap();
        assert!(merged_models(&path, "u").is_err());
    }
}
