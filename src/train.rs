//! `sea_trained`'s code cache. `--build-sea` caches only the code V8 compiles eagerly, the top
//! level, so every function startup calls is compiled at launch. Here the bundle runs under plain
//! `node` (the binary the executable embeds) through `bench`'s runs in a throwaway home: RPC
//! startup, interactive startup in a pty, and a print turn writing a file. Each run compiles with
//! the cache the previous one wrote at exit (`bundle/train.cjs`), and the last one's holds every
//! function compiled by then. V8 output differs slightly between trainings, so the cache is kept
//! per bundle, Node and V8 flags: a rebuild reuses it and stays byte-identical.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::bench::Bench;
use crate::fake_provider::{self, FakeProvider};
use crate::harness::PiCommand;
use crate::stage::{self, Outcome};

/// Under the directory holding `pibake.toml`.
pub const TRAIN_ROOT: &str = "target/pibake/train";
pub const CACHE_FILE: &str = "cache.bin";
const TRAIN: &str = include_str!("bundle/train.cjs");
/// The print turn's file: small, since the runs are for coverage, not load.
const PRINT_TURN_BYTES: usize = 1024;

pub struct Trainer<'a> {
    /// Holds one directory per training.
    pub root: &'a Path,
    /// The `sea_trained` bundle directory, named by its hash.
    pub bundle: &'a Path,
    pub node_version: &'a str,
    /// The executable's V8 flags; V8 rejects a cache made under others.
    pub v8_flags: &'a [&'a str],
    /// The agent directory the runs' throwaway homes are made from.
    pub source: &'a Path,
    pub real_home: &'a Path,
}

impl Trainer<'_> {
    /// The trained cache file, made now or reused.
    pub fn run(&self, log: &mut dyn FnMut(String)) -> Result<Outcome> {
        let outcome = stage::publish(self.root, &self.key()?, log, |dir, log| {
            let script_dir = tempfile::tempdir()?;
            let script = script_dir.path().join("train.cjs");
            fs::write(&script, TRAIN)?;
            let cache = dir.join(CACHE_FILE);
            let path = |p: &Path| p.to_string_lossy().into_owned();
            let pi = PiCommand {
                label: "training".into(),
                program: "node".into(),
                args: self
                    .v8_flags
                    .iter()
                    .map(|f| f.to_string())
                    .chain([
                        path(&script),
                        path(&cache),
                        path(&self.bundle.join("main.cjs")),
                    ])
                    .collect(),
            };
            let bench = Bench {
                commands: &[],
                runs: 0,
                scenarios: &[],
                big_write_bytes: PRINT_TURN_BYTES,
                source: self.source,
                real_home: self.real_home,
                perf_stat: false,
            };
            let provider = FakeProvider::unrecorded(PRINT_TURN_BYTES)?;
            let content = fake_provider::big_write_content(PRINT_TURN_BYTES);
            let size = || -> Result<String> {
                let len = fs::metadata(&cache)
                    .context("the run wrote no code cache")?
                    .len();
                Ok(format!("{:.1} MiB cached", len as f64 / (1024.0 * 1024.0)))
            };
            bench.startup(&pi, &provider)?;
            log(format!("train: RPC startup, {}", size()?));
            bench.interactive(&pi, &provider)?;
            log(format!("train: interactive startup, {}", size()?));
            bench.big_write(&pi, &provider, &content)?;
            log(format!("train: print turn, {}", size()?));
            Ok(())
        })?;
        Ok(match outcome {
            Outcome::Cached(d) => Outcome::Cached(d.join(CACHE_FILE)),
            Outcome::Built(d) => Outcome::Built(d.join(CACHE_FILE)),
        })
    }

    fn key(&self) -> Result<String> {
        let bundle = self
            .bundle
            .file_name()
            .context("the bundle directory has no name")?;
        Ok(stage::content_hash([
            ("pibake", env!("CARGO_PKG_VERSION").as_bytes()),
            ("bundle", bundle.as_encoded_bytes()),
            ("node", self.node_version.as_bytes()),
            ("v8_flags", self.v8_flags.join("\n").as_bytes()),
            ("train.cjs", TRAIN.as_bytes()),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trainer<'a>(bundle: &'a Path, node: &'a str, flags: &'a [&'a str]) -> Trainer<'a> {
        Trainer {
            root: Path::new("/unused"),
            bundle,
            node_version: node,
            v8_flags: flags,
            source: Path::new("/unused"),
            real_home: Path::new("/unused"),
        }
    }

    #[test]
    fn a_training_is_kept_per_bundle_node_and_flags() {
        let key = |b: &str, n: &str, f: &[&str]| trainer(Path::new(b), n, f).key().unwrap();
        let base = key("/x/bundle/abc", "26.11.1", &[]);
        assert_eq!(base, key("/y/bundle/abc", "26.11.1", &[]));
        assert_ne!(base, key("/x/bundle/abd", "26.11.1", &[]));
        assert_ne!(base, key("/x/bundle/abc", "26.11.2", &[]));
        assert_ne!(
            base,
            key("/x/bundle/abc", "26.11.1", &["--perf-basic-prof"])
        );
    }
}
