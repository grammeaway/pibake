//! `pibake build`: the bundle as a Node single executable application (SEA). `main.mjs` is the
//! SEA's main, with V8's code cache; the stage files Pi reads at runtime ride along as assets,
//! which the bundle's prologue (`bundle/prologue.mjs`) extracts once into the user's cache.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::bundle::PI_CHUNK_FILES;
use crate::lock::PI_PACKAGE;
use crate::stage;

/// Under the directory holding `pibake.toml`.
pub const BIN_ROOT: &str = "target/pibake/bin";

/// Packages under the stage's `node_modules` whose files Pi reads from disk at runtime: Pi's
/// docs, themes, templates and workers; the TUI's native clipboard helpers; the wasm of
/// quickjs (code mode) and photon (the image-resize worker, which imports it).
const SHIPPED: [&str; 4] = [
    PI_PACKAGE,
    "@earendil-works/pi-tui",
    "quickjs-wasi",
    "@silvia-odwyer/photon-node",
];

/// Stage-relative (`/`-separated) path → file, for every file the executable carries.
pub fn shipped_files(stage_dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for package in SHIPPED {
        let rel = format!("node_modules/{package}");
        let dir = stage_dir.join(&rel);
        if !dir.join("package.json").is_file() {
            bail!("{package} is not in the stage {}", stage_dir.display());
        }
        walk(&dir, &rel, &mut out)?;
    }
    Ok(out)
}

fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| anyhow::anyhow!("{}: non-UTF-8 file name {n:?}", dir.display()))?;
        let path = entry.path();
        let child = format!("{rel}/{name}");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !skipped_dir(&child) {
                walk(&path, &child, out)?;
            }
        } else if kind.is_file() {
            if shipped_file(&child) {
                out.insert(child, path);
            }
        } else {
            bail!("{}: neither a file nor a directory", path.display());
        }
    }
    Ok(())
}

/// Nested dependencies are bundled; Pi's release bundle is replaced by ours, but for its workers.
fn skipped_dir(rel: &str) -> bool {
    rel.rsplit('/').next() == Some("node_modules")
        || rel == format!("node_modules/{PI_PACKAGE}/dist/bundle")
}

fn shipped_file(rel: &str) -> bool {
    const SKIPPED: [&str; 4] = [".map", ".d.ts", ".d.mts", ".d.cts"];
    !SKIPPED.iter().any(|s| rel.ends_with(s))
}

/// Pi's release workers, which `skipped_dir` leaves out with the rest of Pi's release bundle.
fn pi_workers(stage_dir: &Path) -> Vec<(String, PathBuf)> {
    PI_CHUNK_FILES
        .iter()
        .map(|f| {
            let rel = format!("node_modules/{PI_PACKAGE}/dist/bundle/chunks/{f}");
            let path = stage_dir.join(&rel);
            (rel, path)
        })
        .collect()
}

/// Runs `node` with the given arguments; the stand-in point for tests.
pub type Node<'a> = &'a dyn Fn(&[&std::ffi::OsStr]) -> Result<()>;

pub fn run_node(args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = Command::new("node")
        .args(args)
        .output()
        .context("running node")?;
    if !out.status.success() {
        bail!(
            "node {:?} failed:\n{}{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub struct Builder<'a> {
    pub stage: &'a Path,
    /// Holds `main.mjs`.
    pub bundle: &'a Path,
    /// What the executable prints for a lone `--version`.
    pub version: &'a str,
    /// The setup it is built from (`staleness::Fingerprint`), checked at startup.
    pub setup: &'a serde_json::Value,
    pub output: &'a Path,
    pub node: Node<'a>,
}

impl Builder<'_> {
    /// Writes the executable at `output`, replacing what is there only once it is complete.
    pub fn run(&self) -> Result<()> {
        let mut files = shipped_files(self.stage)?;
        for (rel, path) in pi_workers(self.stage) {
            if !path.is_file() {
                bail!("{rel} is not in the stage {}", self.stage.display());
            }
            files.insert(rel, path);
        }
        let contents = files
            .iter()
            .map(|(rel, path)| Ok((rel.as_str(), fs::read(path)?)))
            .collect::<Result<Vec<_>>>()?;
        let hash = stage::content_hash(contents.iter().map(|(r, b)| (*r, &b[..])));

        let work = tempfile::tempdir()?;
        let info = work.path().join("pibake.json");
        fs::write(
            &info,
            serde_json::to_vec(
                &json!({ "version": self.version, "hash": hash, "setup": self.setup }),
            )?,
        )?;
        let parent = self.output.parent().context("output has no directory")?;
        fs::create_dir_all(parent)?;
        let tmp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(parent)?
            .into_temp_path();
        let mut assets = serde_json::Map::new();
        assets.insert("pibake.json".into(), json!(info));
        for (rel, path) in &files {
            assets.insert(format!("files/{rel}"), json!(path));
        }
        let config = json!({
            "main": self.bundle.join("main.mjs"),
            "mainFormat": "module",
            "output": &*tmp,
            "useCodeCache": true,
            "disableExperimentalSEAWarning": true,
            "assets": assets,
        });
        let config_path = work.path().join("sea.json");
        fs::File::create(&config_path)?.write_all(&serde_json::to_vec_pretty(&config)?)?;
        // A fixed seed makes the code cache, and so the executable, the same bytes every build;
        // `--hash-seed` would too, but V8 then rejects the cache at runtime.
        (self.node)(&[
            "--random-seed=1".as_ref(),
            "--build-sea".as_ref(),
            config_path.as_os_str(),
        ])?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        tmp.persist(self.output)
            .with_context(|| format!("writing {}", self.output.display()))?;
        Ok(())
    }
}

/// Copies `from` to `to` through a temporary file beside `to`, so `to` is never half-written.
pub fn install(from: &Path, to: &Path) -> Result<()> {
    let parent = to.parent().context("install path has no directory")?;
    fs::create_dir_all(parent)?;
    let tmp = tempfile::Builder::new()
        .prefix(".tmp-pibake-")
        .tempfile_in(parent)?
        .into_temp_path();
    fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    tmp.persist(to)
        .with_context(|| format!("writing {}", to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, rel).unwrap();
    }

    fn stage() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for package in SHIPPED {
            touch(dir.path(), &format!("node_modules/{package}/package.json"));
        }
        dir
    }

    #[test]
    fn ships_package_files_but_maps_types_nested_deps_and_pi_release_bundle() {
        let dir = stage();
        let pi = format!("node_modules/{PI_PACKAGE}");
        for rel in [
            "docs/README.md",
            "dist/main.js",
            "dist/main.js.map",
            "dist/main.d.ts",
            "dist/modes/theme/dark.json",
            "node_modules/undici/index.js",
            "dist/bundle/cli.js",
            "dist/bundle/chunks/codemode-worker.js",
        ] {
            touch(dir.path(), &format!("{pi}/{rel}"));
        }
        touch(dir.path(), "node_modules/quickjs-wasi/quickjs.wasm");
        touch(dir.path(), "node_modules/unshipped/index.js");

        let files = shipped_files(dir.path()).unwrap();
        let mut expected: Vec<String> = SHIPPED
            .iter()
            .map(|p| format!("node_modules/{p}/package.json"))
            .chain(
                [
                    "docs/README.md",
                    "dist/main.js",
                    "dist/modes/theme/dark.json",
                ]
                .iter()
                .map(|r| format!("{pi}/{r}")),
            )
            .chain(["node_modules/quickjs-wasi/quickjs.wasm".to_owned()])
            .collect();
        expected.sort();
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), expected);
        for (rel, path) in &files {
            assert_eq!(path, &dir.path().join(rel));
        }
    }

    #[test]
    fn a_missing_package_is_an_error() {
        let dir = stage();
        fs::remove_file(dir.path().join("node_modules/quickjs-wasi/package.json")).unwrap();
        let err = shipped_files(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("quickjs-wasi"), "{err:#}");
    }

    #[test]
    fn builds_from_the_bundle_with_every_file_and_replaces_the_output() {
        let dir = stage();
        for f in PI_CHUNK_FILES {
            touch(
                dir.path(),
                &format!("node_modules/{PI_PACKAGE}/dist/bundle/chunks/{f}"),
            );
        }
        let out_dir = tempfile::tempdir().unwrap();
        let output = out_dir.path().join("bin/pi-baked");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, "old").unwrap();
        let seen = std::cell::RefCell::new(None);
        let node = |args: &[&std::ffi::OsStr]| -> Result<()> {
            let config: serde_json::Value =
                serde_json::from_slice(&fs::read(args.last().unwrap())?)?;
            fs::write(config["output"].as_str().unwrap(), "new")?;
            let info: serde_json::Value = serde_json::from_slice(&fs::read(
                config["assets"]["pibake.json"].as_str().unwrap(),
            )?)?;
            *seen.borrow_mut() = Some((config, info));
            Ok(())
        };
        Builder {
            stage: dir.path(),
            bundle: Path::new("/b"),
            version: "1.3.0 (pibake x)",
            setup: &json!({ "pi": "fingerprint" }),
            output: &output,
            node: &node,
        }
        .run()
        .unwrap();

        assert_eq!(fs::read_to_string(&output).unwrap(), "new");
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 1);
        let (config, info) = seen.into_inner().unwrap();
        assert_eq!(config["main"], "/b/main.mjs");
        assert_eq!(config["mainFormat"], "module");
        assert_eq!(config["useCodeCache"], true);
        assert_eq!(info["version"], "1.3.0 (pibake x)");
        assert_eq!(info["setup"], json!({ "pi": "fingerprint" }));
        assert_eq!(info["hash"].as_str().unwrap().len(), 16);
        let assets = config["assets"].as_object().unwrap();
        for f in PI_CHUNK_FILES {
            let key = format!("files/node_modules/{PI_PACKAGE}/dist/bundle/chunks/{f}");
            assert!(assets.contains_key(&key), "{key}");
        }
        assert_eq!(assets.len(), 1 + SHIPPED.len() + PI_CHUNK_FILES.len());
    }

    #[test]
    fn a_failed_build_keeps_the_old_output() {
        let dir = stage();
        let out_dir = tempfile::tempdir().unwrap();
        let output = out_dir.path().join("pi-baked");
        fs::write(&output, "old").unwrap();
        let err = Builder {
            stage: dir.path(),
            bundle: Path::new("/b"),
            version: "v",
            setup: &json!(null),
            output: &output,
            node: &|_| bail!("node broke"),
        }
        .run()
        .unwrap_err();
        // The workers are missing from this stage.
        assert!(format!("{err:#}").contains("codemode-worker.js"), "{err:#}");
        assert_eq!(fs::read_to_string(&output).unwrap(), "old");

        for f in PI_CHUNK_FILES {
            touch(
                dir.path(),
                &format!("node_modules/{PI_PACKAGE}/dist/bundle/chunks/{f}"),
            );
        }
        let err = Builder {
            stage: dir.path(),
            bundle: Path::new("/b"),
            version: "v",
            setup: &json!(null),
            output: &output,
            node: &|_| bail!("node broke"),
        }
        .run()
        .unwrap_err();
        assert!(format!("{err:#}").contains("node broke"), "{err:#}");
        assert_eq!(fs::read_to_string(&output).unwrap(), "old");
        assert_eq!(fs::read_dir(out_dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn install_replaces_the_target_whole() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("built");
        fs::write(&from, "binary").unwrap();
        let to = dir.path().join("bin/pi-baked");
        install(&from, &to).unwrap();
        install(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "binary");
        assert_eq!(
            fs::metadata(&to).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(to.parent().unwrap()).unwrap().count(), 1);
    }
}
