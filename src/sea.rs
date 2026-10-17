//! `pibake build`: the bundle as a Node single executable application (SEA). The stage files Pi
//! reads at runtime ride along as assets, extracted once into the user's cache.
//!
//! `sea_codecache`: `main.mjs` is the SEA's main, with V8's code cache; its prologue
//! (`bundle/prologue.mjs`) extracts the files at launch.
//!
//! `sea_snapshot`: `main.cjs` runs at build time and the SEA carries the resulting V8 startup
//! snapshot. Module code reads its location while the snapshot is built, so the files are
//! extracted at build time to the path the executable then always uses; a launch re-extracts them
//! only if they are gone. Before the build, `bundle/trace.cjs` runs the same evaluation and
//! `passes::snapshot_safety::check` judges what it saw.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::bundle::{self, PI_CHUNK_FILES};
use crate::config::Backend;
use crate::lock::PI_PACKAGE;
use crate::passes::snapshot_safety;
use crate::stage;

const TRACE: &str = include_str!("bundle/trace.cjs");
/// Marks a complete extraction; the prologues use the same name.
const EXTRACTED: &str = ".pibake-extracted";

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

/// Where executables extract their files: `$XDG_CACHE_HOME/pibake` if absolute, else
/// `~/.cache/pibake`, as the prologues decide.
pub fn cache_dir(home: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    base.join("pibake")
}

/// Runs `node` with the given arguments, and with only `env` when given (`PATH` is kept); the
/// stand-in point for tests.
pub type Node<'a> = &'a dyn Fn(&[&std::ffi::OsStr], Env) -> Result<()>;
/// The only environment variables a `node` run gets, besides `PATH`; `None` inherits pibake's.
pub type Env<'a> = Option<&'a [(&'a str, &'a std::ffi::OsStr)]>;

pub fn run_node(args: &[&std::ffi::OsStr], env: Env) -> Result<()> {
    let mut cmd = Command::new("node");
    cmd.args(args);
    if let Some(env) = env {
        cmd.env_clear().envs(env.iter().copied());
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
    }
    let out = cmd.output().context("running node")?;
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
    /// Holds the bundle's main file.
    pub bundle: &'a Path,
    pub backend: Backend,
    /// `cache_dir`, where a snapshot executable's files are extracted at build time.
    pub cache: &'a Path,
    /// What the executable prints for a lone `--version`.
    pub version: &'a str,
    /// The setup it is built from (`staleness::Fingerprint`), checked at startup.
    pub setup: &'a serde_json::Value,
    pub output: &'a Path,
    pub node: Node<'a>,
}

impl Builder<'_> {
    /// Writes the executable at `output`, replacing what is there only once it is complete.
    /// Returns what a snapshot keeps from its build, as `snapshot_safety::check` accepted it.
    pub fn run(&self) -> Result<Vec<String>> {
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
        let main = self.bundle.join(bundle::main_file(self.backend));
        let mut config = json!({
            "main": main,
            "output": &*tmp,
            "disableExperimentalSEAWarning": true,
            "assets": assets,
        });
        let config_path = work.path().join("sea.json");
        let mut kept = vec![];
        match self.backend {
            Backend::SeaCodecache => {
                config["mainFormat"] = json!("module");
                config["useCodeCache"] = json!(true);
                fs::File::create(&config_path)?.write_all(&serde_json::to_vec_pretty(&config)?)?;
                // A fixed seed makes the code cache, and so the executable, the same bytes every
                // build; `--hash-seed` would too, but V8 then rejects the cache at runtime.
                (self.node)(
                    &[
                        "--random-seed=1".as_ref(),
                        "--build-sea".as_ref(),
                        config_path.as_os_str(),
                    ],
                    None,
                )?;
            }
            Backend::SeaSnapshot => {
                let root = self.cache.join(&hash);
                extract(&contents, &root, &hash)?;
                kept = self.gate(&main, &root, work.path())?;
                config["useSnapshot"] = json!(true);
                fs::File::create(&config_path)?.write_all(&serde_json::to_vec_pretty(&config)?)?;
                // The build's environment is what the snapshot could keep: only the root.
                (self.node)(
                    &["--build-sea".as_ref(), config_path.as_os_str()],
                    Some(&[("PIBAKE_SNAPSHOT_ROOT", root.as_os_str())]),
                )?;
            }
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        tmp.persist(self.output)
            .with_context(|| format!("writing {}", self.output.display()))?;
        Ok(kept)
    }

    /// Evaluates the snapshot bundle's modules under `bundle/trace.cjs`, as the snapshot build
    /// will, and fails on what `snapshot_safety` does not accept.
    fn gate(&self, main: &Path, root: &Path, work: &Path) -> Result<Vec<String>> {
        let trace = work.join("trace.cjs");
        let out = work.join("trace.json");
        fs::write(&trace, TRACE)?;
        (self.node)(
            &["--require".as_ref(), trace.as_os_str(), main.as_os_str()],
            Some(&[
                ("PIBAKE_SNAPSHOT_ROOT", root.as_os_str()),
                ("PIBAKE_TRACE_OUT", out.as_os_str()),
            ]),
        )
        .context("evaluating the bundle's modules as the snapshot build would")?;
        let findings: Vec<snapshot_safety::Finding> =
            serde_json::from_slice(&fs::read(&out).context("the trace wrote no findings")?)?;
        snapshot_safety::check(&findings)
    }
}

/// The executable's files at `root`, written as the prologues extract them (`bundle/extract.js`):
/// into a temporary directory beside it, renamed into place, marked complete.
fn extract(contents: &[(&str, Vec<u8>)], root: &Path, hash: &str) -> Result<()> {
    if root.join(EXTRACTED).is_file() {
        return Ok(());
    }
    if root.exists() {
        bail!("{} exists without {EXTRACTED}; remove it", root.display());
    }
    let base = root.parent().context("extraction root has no directory")?;
    fs::create_dir_all(base)?;
    let tmp = tempfile::Builder::new()
        .prefix(&format!(".tmp-{hash}-"))
        .tempdir_in(base)?;
    for (rel, bytes) in contents {
        let path = tmp.path().join(rel);
        fs::create_dir_all(path.parent().context("a file has a directory")?)?;
        fs::write(&path, bytes)?;
    }
    fs::write(tmp.path().join(EXTRACTED), format!("{hash}\n"))?;
    let tmp = tmp.keep();
    if let Err(e) = fs::rename(&tmp, root) {
        let _ = fs::remove_dir_all(&tmp);
        if !root.join(EXTRACTED).is_file() {
            return Err(e).with_context(|| format!("extracting into {}", root.display()));
        }
    }
    Ok(())
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
        let node = |args: &[&std::ffi::OsStr], env: Env| -> Result<()> {
            assert!(env.is_none());
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
            backend: Backend::SeaCodecache,
            cache: Path::new("/unused"),
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
            backend: Backend::SeaCodecache,
            cache: Path::new("/unused"),
            version: "v",
            setup: &json!(null),
            output: &output,
            node: &|_, _| bail!("node broke"),
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
            backend: Backend::SeaCodecache,
            cache: Path::new("/unused"),
            version: "v",
            setup: &json!(null),
            output: &output,
            node: &|_, _| bail!("node broke"),
        }
        .run()
        .unwrap_err();
        assert!(format!("{err:#}").contains("node broke"), "{err:#}");
        assert_eq!(fs::read_to_string(&output).unwrap(), "old");
        assert_eq!(fs::read_dir(out_dir.path()).unwrap().count(), 1);
    }

    fn staged_with_workers() -> tempfile::TempDir {
        let dir = stage();
        for f in PI_CHUNK_FILES {
            touch(
                dir.path(),
                &format!("node_modules/{PI_PACKAGE}/dist/bundle/chunks/{f}"),
            );
        }
        dir
    }

    type Calls = std::cell::RefCell<Vec<(Vec<String>, Option<Vec<(String, String)>>)>>;

    /// A node that answers the gate with `findings` and builds by writing "new".
    fn snapshot_node<'a>(
        findings: &'a serde_json::Value,
        calls: &'a Calls,
    ) -> impl Fn(&[&std::ffi::OsStr], Env) -> Result<()> + 'a {
        move |args, env| {
            let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
            let env: Option<Vec<(String, String)>> = env.map(|e| {
                e.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string_lossy().into()))
                    .collect()
            });
            let var = |k: &str| {
                env.as_ref()
                    .and_then(|e| e.iter().find(|(n, _)| n == k))
                    .map(|(_, v)| v.clone())
            };
            if args[0] == "--require" {
                fs::write(var("PIBAKE_TRACE_OUT").unwrap(), findings.to_string())?;
            } else {
                let config: serde_json::Value = serde_json::from_slice(&fs::read(&args[1])?)?;
                fs::write(config["output"].as_str().unwrap(), "new")?;
            }
            calls.borrow_mut().push((args, env));
            Ok(())
        }
    }

    fn snapshot_builder<'a>(
        dir: &'a Path,
        cache: &'a Path,
        output: &'a Path,
        node: Node<'a>,
    ) -> Builder<'a> {
        Builder {
            stage: dir,
            bundle: Path::new("/b"),
            backend: Backend::SeaSnapshot,
            cache,
            version: "v",
            setup: &serde_json::Value::Null,
            output,
            node,
        }
    }

    #[test]
    fn a_snapshot_is_built_from_files_extracted_to_its_root_after_the_gate() {
        let dir = staged_with_workers();
        let out = tempfile::tempdir().unwrap();
        let (cache, output) = (out.path().join("cache"), out.path().join("bin/pi"));
        let findings = json!([{ "module": "node_modules/which/which.js", "kind": "env", "detail": "OSTYPE", "line": 9 }]);
        let calls = Calls::default();
        let node = snapshot_node(&findings, &calls);
        let kept = snapshot_builder(dir.path(), &cache, &output, &node)
            .run()
            .unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(
            kept[0].starts_with("which/which.js: env OSTYPE"),
            "{kept:?}"
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), "new");

        let roots: Vec<_> = fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(roots.len(), 1, "{roots:?}");
        let root = &roots[0];
        let hash = root.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            fs::read_to_string(root.join(EXTRACTED)).unwrap(),
            format!("{hash}\n")
        );
        for rel in shipped_files(dir.path()).unwrap().keys() {
            assert_eq!(fs::read_to_string(root.join(rel)).unwrap(), *rel);
        }

        drop(node);
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 2);
        let (gate, gate_env) = &calls[0];
        assert_eq!(gate[0], "--require");
        assert_eq!(gate[2], "/b/main.cjs");
        let root_var = (
            "PIBAKE_SNAPSHOT_ROOT".to_owned(),
            root.to_string_lossy().into_owned(),
        );
        assert!(gate_env.as_ref().unwrap().contains(&root_var));
        let (build, build_env) = &calls[1];
        assert_eq!(build[0], "--build-sea");
        assert_eq!(build_env.as_deref(), Some(&[root_var][..]));

        // A second build reuses the extraction.
        let calls = Calls::default();
        let node = snapshot_node(&findings, &calls);
        snapshot_builder(dir.path(), &cache, &output, &node)
            .run()
            .unwrap();
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[test]
    fn a_snapshot_config_asks_for_the_snapshot_of_main_cjs() {
        let dir = staged_with_workers();
        let out = tempfile::tempdir().unwrap();
        let (cache, output) = (out.path().join("cache"), out.path().join("pi"));
        let seen = std::cell::RefCell::new(None);
        let node = |args: &[&std::ffi::OsStr], env: Env| -> Result<()> {
            if args[0] == "--require" {
                let out = env
                    .unwrap()
                    .iter()
                    .find(|(k, _)| *k == "PIBAKE_TRACE_OUT")
                    .unwrap()
                    .1;
                return Ok(fs::write(out, "[]")?);
            }
            let config: serde_json::Value = serde_json::from_slice(&fs::read(args[1])?)?;
            fs::write(config["output"].as_str().unwrap(), "new")?;
            *seen.borrow_mut() = Some(config);
            Ok(())
        };
        snapshot_builder(dir.path(), &cache, &output, &node)
            .run()
            .unwrap();
        let config = seen.into_inner().unwrap();
        assert_eq!(config["main"], "/b/main.cjs");
        assert_eq!(config["useSnapshot"], true);
        assert!(config.get("mainFormat").is_none() && config.get("useCodeCache").is_none());
    }

    #[test]
    fn an_unhandled_finding_fails_before_the_snapshot_is_built() {
        let dir = staged_with_workers();
        let out = tempfile::tempdir().unwrap();
        let (cache, output) = (out.path().join("cache"), out.path().join("pi"));
        fs::write(&output, "old").unwrap();
        let findings = json!([{ "module": "node_modules/x/start.js", "kind": "time", "detail": "Date.now()", "line": 42 }]);
        let calls = Calls::default();
        let node = snapshot_node(&findings, &calls);
        let err = snapshot_builder(dir.path(), &cache, &output, &node)
            .run()
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("node_modules/x/start.js (main.cjs:42): time Date.now()"),
            "{err:#}"
        );
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(fs::read_to_string(&output).unwrap(), "old");
    }

    #[test]
    fn a_root_without_its_marker_is_not_reused() {
        let out = tempfile::tempdir().unwrap();
        let contents = vec![("a/b.js", b"x".to_vec())];
        let root = out.path().join("cache/abc");
        fs::create_dir_all(&root).unwrap();
        let err = extract(&contents, &root, "abc").unwrap_err();
        assert!(
            format!("{err:#}").contains("without .pibake-extracted"),
            "{err:#}"
        );
        fs::remove_dir(&root).unwrap();
        extract(&contents, &root, "abc").unwrap();
        assert_eq!(fs::read_to_string(root.join("a/b.js")).unwrap(), "x");
        // Nothing is left beside it.
        assert_eq!(fs::read_dir(out.path().join("cache")).unwrap().count(), 1);
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
