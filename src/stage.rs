//! `pibake stage`: installs the lock into `target/pibake/stage/<hash>/`, keyed by the content
//! hash of its inputs and reused while they are unchanged.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::inventory::{self, Inventory};
use crate::lock::{self, ESBUILD_VERSION, LOCK_FILE, Lock, NPM_LOCK_FILE};
use crate::package::Source;

/// Under the directory holding `pibake.toml`.
pub const STAGE_ROOT: &str = "target/pibake/stage";
/// Written last; a stage or bundle directory without it is not used.
pub const MARKER: &str = ".pibake-staged";

/// What a stage is built from, read once: the hash covers exactly the bytes that get staged.
pub struct Inputs {
    lock: Lock,
    npm_lock: Vec<u8>,
    /// Path relative to the agent directory's `extensions/`, `/`-separated → content.
    extensions: BTreeMap<String, Vec<u8>>,
    hash: String,
}

impl Inputs {
    pub fn read(lock_dir: &Path, inventory: &Inventory) -> Result<Self> {
        refuse_unstaged_local_code(inventory)?;
        let read = |name: &str| {
            fs::read(lock_dir.join(name)).with_context(|| {
                format!(
                    "reading {name} in {}; run `pibake lock`",
                    lock_dir.display()
                )
            })
        };
        let lock_text = read(LOCK_FILE)?;
        let npm_lock = read(NPM_LOCK_FILE)?;
        let lock = Lock::parse(&String::from_utf8_lossy(&lock_text))
            .with_context(|| format!("parsing {LOCK_FILE}"))?;
        if lock::sha256_hex(&npm_lock) != lock.npm_lock.sha256 {
            bail!("{NPM_LOCK_FILE} does not match {LOCK_FILE}; run `pibake lock --update`");
        }
        let npm_json: Value = serde_json::from_slice(&npm_lock)?;
        let dev = lock::root_deps(&npm_json, "devDependencies")?;
        if dev.get("esbuild").map(String::as_str) != Some(ESBUILD_VERSION) {
            bail!(
                "{NPM_LOCK_FILE} does not pin esbuild {ESBUILD_VERSION}; run `pibake lock --update`"
            );
        }
        for pin in &lock.git {
            check_git_pin(pin)?;
        }
        let mut extensions = BTreeMap::new();
        let dir = inventory.agent_dir.join("extensions");
        if dir.exists() {
            walk_tree(&dir, "", &mut |rel, path, _| {
                let bytes =
                    fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                extensions.insert(rel, bytes);
                Ok(())
            })?;
        }

        let version = env!("CARGO_PKG_VERSION").as_bytes();
        let ext_keys: Vec<String> = extensions
            .keys()
            .map(|k| format!("extensions/{k}"))
            .collect();
        let hash = content_hash(
            [
                ("pibake", version),
                (LOCK_FILE, &lock_text[..]),
                (NPM_LOCK_FILE, &npm_lock[..]),
            ]
            .into_iter()
            .chain(
                ext_keys
                    .iter()
                    .map(String::as_str)
                    .zip(extensions.values().map(Vec::as_slice)),
            ),
        );
        Ok(Self {
            lock,
            npm_lock,
            extensions,
            hash,
        })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// Code the stage does not carry yet must not be silently left out of the build.
// shortcut: local packages and settings.json extension paths are refused, not staged; stage
// them (and hash them) when a setup uses one.
fn refuse_unstaged_local_code(inventory: &Inventory) -> Result<()> {
    let mut found: Vec<&str> = inventory
        .packages
        .iter()
        .filter(|p| matches!(p.parsed, Source::Local { .. }))
        .map(|p| p.source.as_str())
        .collect();
    found.extend(
        inventory
            .settings_resources
            .get("extensions")
            .into_iter()
            .flatten()
            .filter(|e| inventory::is_path_entry(e))
            .map(String::as_str),
    );
    if !found.is_empty() {
        bail!(
            "pibake does not stage local packages or settings.json extension paths yet: {}",
            found.join(", ")
        );
    }
    Ok(())
}

/// The lock is a file anyone can edit; its git pins become paths and git arguments.
fn check_git_pin(pin: &lock::GitPin) -> Result<()> {
    if pin
        .name
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        bail!(
            "{LOCK_FILE}: git package name {:?} is not a relative path",
            pin.name
        );
    }
    let hex = pin.commit.chars().all(|c| c.is_ascii_hexdigit());
    if !hex || !matches!(pin.commit.len(), 40 | 64) {
        bail!(
            "{LOCK_FILE}: {} has commit {:?}, not a full hash",
            pin.name,
            pin.commit
        );
    }
    if pin.url.starts_with('-') {
        bail!("{LOCK_FILE}: {} has url {:?}", pin.name, pin.url);
    }
    Ok(())
}

/// Calls `file` with the `/`-separated relative path, path and (followed) metadata of each file
/// under `dir`, following file symlinks. Directory symlinks are refused (loops).
pub(crate) fn walk_tree(
    dir: &Path,
    prefix: &str,
    file: &mut dyn FnMut(String, &Path, &fs::Metadata) -> Result<()>,
) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .with_context(|| format!("{}: name is not UTF-8", entry.path().display()))?;
        let rel = format!("{prefix}{name}");
        let path = entry.path();
        let meta = fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.is_dir() {
            if entry.file_type()?.is_symlink() {
                bail!(
                    "{} is a symlinked directory; pibake does not follow those",
                    path.display()
                );
            }
            walk_tree(&path, &format!("{rel}/"), file)?;
        } else if meta.is_file() {
            file(rel, &path, &meta)?;
        } else {
            bail!("{} is neither a file nor a directory", path.display());
        }
    }
    Ok(())
}

/// SHA-256 over `(name, content)` pairs in name order, each part length-prefixed so no two
/// different sets encode alike. 16 hex digits: a local cache key, not a signature.
pub fn content_hash<'a>(inputs: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut inputs: Vec<_> = inputs.into_iter().collect();
    inputs.sort();
    let mut hasher = Sha256::new();
    for (name, content) in inputs {
        for part in [name.as_bytes(), content] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
    }
    hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Runs npm with `args` in a directory.
pub type Npm<'a> = &'a dyn Fn(&Path, &[&str]) -> Result<()>;

/// Root install: the lock exactly, dev dependencies (esbuild) included, Pi's peer handling.
pub const NPM_CI: [&str; 6] = [
    "ci",
    "--ignore-scripts",
    "--legacy-peer-deps",
    "--include=dev",
    "--no-audit",
    "--no-fund",
];
/// Git package dependencies, as Pi installs them (`getGitDependencyInstallArgs`), scripts off.
const GIT_DEP_FLAGS: [&str; 5] = [
    "--ignore-scripts",
    "--omit=dev",
    "--legacy-peer-deps",
    "--no-audit",
    "--no-fund",
];

pub fn run_npm(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("npm")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running npm")?;
    if !out.status.success() {
        bail!(
            "npm {} in {} failed:\n{}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Debug)]
pub enum Outcome {
    Cached(PathBuf),
    Built(PathBuf),
}

pub struct Stager<'a> {
    /// Holds one directory per hash.
    pub root: &'a Path,
    pub npm: Npm<'a>,
}

/// `<root>/<hash>`, reused if complete, else filled by `fill` beside it and renamed into place,
/// so an interrupted build never sits at `<hash>`.
pub fn publish(
    root: &Path,
    hash: &str,
    log: &mut dyn FnMut(String),
    fill: impl FnOnce(&Path, &mut dyn FnMut(String)) -> Result<()>,
) -> Result<Outcome> {
    let target = root.join(hash);
    if target.join(MARKER).is_file() {
        return Ok(Outcome::Cached(target));
    }
    if target.exists() {
        log(format!("{} is incomplete; rebuilding it", target.display()));
        fs::remove_dir_all(&target)?;
    }
    fs::create_dir_all(root)?;
    // Dropped (removed) on error.
    let tmp = tempfile::Builder::new().prefix(".tmp-").tempdir_in(root)?;
    fill(tmp.path(), log)?;
    fs::write(tmp.path().join(MARKER), format!("{hash}\n"))?;
    let tmp = tmp.keep();
    if let Err(e) = fs::rename(&tmp, &target) {
        fs::remove_dir_all(&tmp)?;
        // A concurrent build of the same inputs got there first.
        if target.join(MARKER).is_file() {
            return Ok(Outcome::Cached(target));
        }
        return Err(e).with_context(|| format!("renaming into {}", target.display()));
    }
    Ok(Outcome::Built(target))
}

impl Stager<'_> {
    pub fn run(&self, inputs: &Inputs, log: &mut dyn FnMut(String)) -> Result<Outcome> {
        publish(self.root, &inputs.hash, log, |dir, log| {
            self.fill(dir, inputs, log)
        })
    }

    fn fill(&self, dir: &Path, inputs: &Inputs, log: &mut dyn FnMut(String)) -> Result<()> {
        let npm_lock: Value = serde_json::from_slice(&inputs.npm_lock)?;
        // The lock's root entry is the package.json `pibake lock` generated.
        let mut package = npm_lock["packages"][""].clone();
        package["private"] = json!(true);
        fs::write(
            dir.join("package.json"),
            format!("{}\n", serde_json::to_string_pretty(&package)?),
        )?;
        fs::write(dir.join("package-lock.json"), &inputs.npm_lock)?;
        log("npm ci".into());
        (self.npm)(dir, &NPM_CI)?;
        check_installed(dir, &npm_lock)?;

        for pin in &inputs.lock.git {
            let checkout = dir.join("git").join(&pin.name);
            log(format!("git clone {} at {}", pin.url, pin.commit));
            clone_at(&pin.url, &pin.commit, &checkout)?;
            self.install_git_dependencies(&checkout, &pin.name, log)?;
        }

        let local = dir.join("local/extensions");
        for (rel, bytes) in &inputs.extensions {
            let path = local.join(rel);
            fs::create_dir_all(path.parent().expect("under local/extensions"))?;
            fs::write(&path, bytes)?;
        }
        Ok(())
    }

    /// Pi installs a git package's dependencies whenever it has a `package.json`; npm with
    /// nothing to install only writes a lockfile, so packages without dependencies are skipped.
    fn install_git_dependencies(
        &self,
        checkout: &Path,
        name: &str,
        log: &mut dyn FnMut(String),
    ) -> Result<()> {
        let Ok(text) = fs::read_to_string(checkout.join("package.json")) else {
            return Ok(());
        };
        let manifest: Value =
            serde_json::from_str(&text).with_context(|| format!("{name}: parsing package.json"))?;
        if manifest["dependencies"]
            .as_object()
            .is_none_or(|d| d.is_empty())
        {
            return Ok(());
        }
        let command = if checkout.join("package-lock.json").is_file() {
            log(format!("{name}: npm ci from its package-lock.json"));
            "ci"
        } else {
            log(format!(
                "{name}: no package-lock.json; npm install resolves its dependencies fresh"
            ));
            "install"
        };
        let args: Vec<&str> = [command].into_iter().chain(GIT_DEP_FLAGS).collect();
        (self.npm)(checkout, &args)
    }
}

fn clone_at(url: &str, commit: &str, dir: &Path) -> Result<()> {
    let git = |args: &[&str], cwd: &Path| -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let parent = dir.parent().expect("checkout is under the stage");
    fs::create_dir_all(parent)?;
    let leaf = dir
        .file_name()
        .expect("checked pin name")
        .to_str()
        .expect("UTF-8 pin name");
    git(
        &["clone", "--quiet", "--no-checkout", "--", url, leaf],
        parent,
    )?;
    git(&["checkout", "--quiet", "--detach", commit], dir)?;
    let head = git(&["rev-parse", "HEAD"], dir)?;
    if head != commit {
        bail!("{url}: checked out {head}, locked {commit}");
    }
    Ok(())
}

/// Every package npm locked is installed at its locked version. Optional packages may be
/// absent: npm skips those built for other platforms (esbuild's binaries).
fn check_installed(dir: &Path, npm_lock: &Value) -> Result<()> {
    let packages = npm_lock["packages"]
        .as_object()
        .context("npm lock has no packages")?;
    let mut wrong = vec![];
    for (path, entry) in packages {
        if path.is_empty() || entry["link"] == true {
            continue;
        }
        let Some(locked) = entry["version"].as_str() else {
            continue;
        };
        let installed = fs::read_to_string(dir.join(path).join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|j| j["version"].as_str().map(str::to_owned));
        match installed {
            Some(v) if v == locked => {}
            None if entry["optional"] == true => {}
            Some(v) => wrong.push(format!("{path}: {v}, locked {locked}")),
            None => wrong.push(format!("{path}: missing, locked {locked}")),
        }
    }
    if !wrong.is_empty() {
        bail!(
            "npm ci did not install what the lock says:\n  {}",
            wrong.join("\n  ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(pairs: &[(&str, &str)]) -> String {
        content_hash(pairs.iter().map(|(n, c)| (*n, c.as_bytes())))
    }

    #[test]
    fn hash_is_order_free_and_unambiguous() {
        assert_eq!(
            hash(&[("a", "1"), ("b", "2")]),
            hash(&[("b", "2"), ("a", "1")])
        );
        assert_eq!(hash(&[]).len(), 16);
        // Moving bytes between name and content, or between entries, changes the hash.
        assert_ne!(hash(&[("ab", "c")]), hash(&[("a", "bc")]));
        assert_ne!(
            hash(&[("a", "1b"), ("", "2")]),
            hash(&[("a", "1"), ("b", "2")])
        );
        assert_ne!(hash(&[("a", "")]), hash(&[]));
    }

    fn pin(name: &str, url: &str, commit: &str) -> lock::GitPin {
        lock::GitPin {
            name: name.into(),
            url: url.into(),
            commit: commit.into(),
        }
    }

    #[test]
    fn git_pins_are_checked_before_use() {
        let sha = "a".repeat(40);
        assert!(check_git_pin(&pin("github.com/o/r", "https://github.com/o/r", &sha)).is_ok());
        assert!(check_git_pin(&pin("h/o/r", "u", &"0f".repeat(32))).is_ok());
        for bad in ["", "../x", "/abs", "h/./r", "h/../../r", "h//r", "h/r/"] {
            assert!(check_git_pin(&pin(bad, "u", &sha)).is_err(), "{bad:?}");
        }
        for bad in ["main", "--upload-pack=x", &"a".repeat(39), &"g".repeat(40)] {
            assert!(check_git_pin(&pin("h/o/r", "u", bad)).is_err(), "{bad:?}");
        }
        assert!(check_git_pin(&pin("h/o/r", "--upload-pack=x", &sha)).is_err());
    }

    #[test]
    fn check_installed_names_wrong_and_missing_packages() {
        let dir = tempfile::tempdir().unwrap();
        let install = |path: &str, version: &str| {
            fs::create_dir_all(dir.path().join(path)).unwrap();
            fs::write(
                dir.path().join(path).join("package.json"),
                json!({"version": version}).to_string(),
            )
            .unwrap();
        };
        install("node_modules/a", "1.0.0");
        install("node_modules/a/node_modules/b", "2.0.0");
        let lock = |extra: Value| {
            let mut packages = json!({
                "": {"name": "root"},
                "node_modules/a": {"version": "1.0.0"},
                "node_modules/a/node_modules/b": {"version": "2.0.0"},
                "node_modules/@esbuild/aix-ppc64": {"version": "0.28.2", "optional": true},
                "node_modules/l": {"link": true, "resolved": "x"},
            });
            packages
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"packages": packages})
        };
        check_installed(dir.path(), &lock(json!({}))).unwrap();
        let err = check_installed(
            dir.path(),
            &lock(json!({
                "node_modules/a": {"version": "1.0.1"},
                "node_modules/c": {"version": "3.0.0"},
            })),
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("node_modules/a: 1.0.0, locked 1.0.1"),
            "{text}"
        );
        assert!(
            text.contains("node_modules/c: missing, locked 3.0.0"),
            "{text}"
        );
    }
}
