use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::passes::{self, Pass};
use crate::paths;

pub const FILE_NAME: &str = "pibake.toml";
const DEFAULT_AGENT_DIR: &str = "~/.pi/agent";
const DEFAULT_NAME: &str = "pi-baked";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub agent_dir: PathBuf,
    /// Where `pibake.toml` is, or the working directory without one; lock files go here.
    pub dir: PathBuf,
    /// The executable's file name, in `target/pibake/bin/` and when installed.
    pub name: String,
    pub passes: passes::Options,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    source: RawSource,
    #[serde(default)]
    build: RawBuild,
    #[serde(default)]
    trim: RawTrim,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    name: Option<String>,
    passes: Option<Vec<Pass>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTrim {
    highlight_languages: Option<Vec<String>>,
    apis: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSource {
    agent_dir: Option<String>,
}

impl Config {
    /// `explicit` must exist; otherwise `<cwd>/pibake.toml` is used if present, else defaults.
    pub fn load(explicit: Option<&Path>, cwd: &Path, home: &Path) -> Result<Self> {
        let path = match explicit {
            Some(p) => Some(cwd.join(p)),
            None => Some(cwd.join(FILE_NAME)).filter(|p| p.is_file()),
        };
        let Some(path) = path else {
            return Ok(Self::parse("", cwd, home).expect("empty config parses"));
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let dir = path.parent().unwrap_or(cwd);
        Self::parse(&text, dir, home).with_context(|| format!("in {}", path.display()))
    }

    /// Relative `agent_dir` resolves from `config_dir`, the directory holding `pibake.toml`.
    pub fn parse(text: &str, config_dir: &Path, home: &Path) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;
        let agent_dir = raw.source.agent_dir.as_deref().unwrap_or(DEFAULT_AGENT_DIR);
        let name = raw.build.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            bail!("[build] name must be a file name, not {name:?}");
        }
        Ok(Self {
            agent_dir: paths::normalize(&config_dir.join(paths::expand_tilde(agent_dir, home))),
            dir: config_dir.to_path_buf(),
            name,
            passes: passes::Options {
                passes: raw.build.passes.unwrap_or(passes::DEFAULT_PASSES.to_vec()),
                highlight_languages: raw
                    .trim
                    .highlight_languages
                    .unwrap_or_else(|| passes::DEFAULT_LANGUAGES.map(str::to_owned).to_vec()),
                apis: raw.trim.apis,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, Path::new("/proj"), Path::new(HOME))
    }

    #[test]
    fn defaults_without_keys() {
        let expected = PathBuf::from("/home/u/.pi/agent");
        assert_eq!(parse("").unwrap().agent_dir, expected);
        assert_eq!(parse("[source]\n").unwrap().agent_dir, expected);
    }

    #[test]
    fn tilde_and_relative_paths() {
        assert_eq!(
            parse("[source]\nagent_dir = \"~/setup\"")
                .unwrap()
                .agent_dir,
            PathBuf::from("/home/u/setup")
        );
        assert_eq!(
            parse("[source]\nagent_dir = \".\"").unwrap().agent_dir,
            PathBuf::from("/proj")
        );
        assert_eq!(
            parse("[source]\nagent_dir = \"/abs/dir\"")
                .unwrap()
                .agent_dir,
            PathBuf::from("/abs/dir")
        );
    }

    #[test]
    fn build_name_defaults_and_must_be_a_file_name() {
        assert_eq!(parse("").unwrap().name, "pi-baked");
        assert_eq!(parse("[build]\nname = \"pi\"").unwrap().name, "pi");
        for bad in ["", ".", "..", "a/b", "/abs"] {
            let err = parse(&format!("[build]\nname = {bad:?}")).unwrap_err();
            assert!(format!("{err:#}").contains("file name"), "{bad}: {err:#}");
        }
    }

    #[test]
    fn passes_default_on_and_switch_off() {
        let default = parse("").unwrap().passes;
        assert_eq!(default.passes, [Pass::TrimHighlight, Pass::TrimApis]);
        assert!(default.highlight_languages.iter().any(|l| l == "rust"));
        assert_eq!(default.apis, None);
        let apis = parse("[trim]\napis = [\"anthropic-messages\"]")
            .unwrap()
            .passes;
        assert_eq!(apis.apis.unwrap(), ["anthropic-messages"]);
        let off = parse("[build]\npasses = []").unwrap().passes;
        assert!(off.passes.is_empty());
        let langs = parse("[trim]\nhighlight_languages = [\"rust\"]")
            .unwrap()
            .passes;
        assert_eq!(langs.highlight_languages, ["rust"]);
        let err = parse("[build]\npasses = [\"trim_everything\"]").unwrap_err();
        assert!(format!("{err:#}").contains("trim_highlight"), "{err:#}");
        assert!(parse("[trim]\nlanguages = []").is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        let top = parse("[sourec]\nagent_dir = \"x\"").unwrap_err();
        assert!(format!("{top:#}").contains("sourec"), "{top:#}");
        let nested = parse("[source]\nagent_dri = \"x\"").unwrap_err();
        assert!(format!("{nested:#}").contains("agent_dri"), "{nested:#}");
    }

    #[test]
    fn load_prefers_explicit_then_cwd_then_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let home = Path::new(HOME);
        assert_eq!(
            Config::load(None, dir.path(), home).unwrap().agent_dir,
            PathBuf::from("/home/u/.pi/agent")
        );

        std::fs::write(dir.path().join(FILE_NAME), "[source]\nagent_dir = \"cwd\"").unwrap();
        assert_eq!(
            Config::load(None, dir.path(), home).unwrap().agent_dir,
            dir.path().join("cwd")
        );

        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("x.toml"), "[source]\nagent_dir = \"rel\"").unwrap();
        assert_eq!(
            Config::load(Some(Path::new("other/x.toml")), dir.path(), home)
                .unwrap()
                .agent_dir,
            other.join("rel")
        );

        assert_eq!(
            Config::load(Some(Path::new("other/x.toml")), dir.path(), home)
                .unwrap()
                .dir,
            other
        );
        assert_eq!(
            Config::load(None, dir.path(), home).unwrap().dir,
            dir.path()
        );

        assert!(Config::load(Some(Path::new("missing.toml")), dir.path(), home).is_err());
    }
}
