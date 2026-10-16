//! `pibake lock`: pins the setup as installed now. `pibake.lock` holds the Pi, Node and git
//! pins; `pibake.package-lock.json` is npm's own lockfile for Pi plus every npm package.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::harness::PiCommand;
use crate::inventory::Inventory;
use crate::package::Source;

pub const LOCK_FILE: &str = "pibake.lock";
pub const NPM_LOCK_FILE: &str = "pibake.package-lock.json";
pub const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";
/// The bundler (D5), pinned by pibake: the npm lock's only dev dependency.
pub const ESBUILD_VERSION: &str = "0.28.2";
/// `--build-sea` (D4).
const MIN_NODE: (u64, u64) = (25, 5);
const HEADER: &str =
    "# Written by `pibake lock` from the setup as installed. Re-pin with `pibake lock --update`.\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub pi: PiPin,
    pub node: NodePin,
    pub npm_lock: NpmLockPin,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git: Vec<GitPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiPin {
    pub version: String,
    pub integrity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodePin {
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmLockPin {
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitPin {
    /// `<host>/<path>`: the install directory under `git/`, and Pi's package identity.
    pub name: String,
    pub url: String,
    pub commit: String,
}

impl Lock {
    /// npm packages are pinned by the npm lock whose hash is `npm_lock_sha256`.
    pub fn pin(setup: &Setup, pi_integrity: String, npm_lock_sha256: String) -> Self {
        Self {
            pi: PiPin {
                version: setup.pi_version.clone(),
                integrity: pi_integrity,
            },
            node: NodePin {
                version: setup.node_version.clone(),
            },
            npm_lock: NpmLockPin {
                sha256: npm_lock_sha256,
            },
            git: setup
                .git
                .iter()
                .map(|(name, (url, commit))| GitPin {
                    name: name.clone(),
                    url: url.clone(),
                    commit: commit.clone(),
                })
                .collect(),
        }
    }

    /// Canonical text: git pins sorted, so equal locks render to equal bytes.
    pub fn render(&self) -> String {
        let mut lock = self.clone();
        lock.git.sort();
        format!(
            "{HEADER}{}",
            toml::to_string(&lock).expect("lock serialises")
        )
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

/// The host tools a lock pins: the stock Pi on `PATH` and the host `node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub pi_dir: PathBuf,
    pub pi_version: String,
    pub node_version: String,
}

impl Host {
    pub fn find() -> Result<Self> {
        let pi_dir = PiCommand::parse("pi")?
            .package_dir()
            .with_context(|| format!("no stock Pi (`{PI_PACKAGE}`) behind `pi` on PATH"))?;
        let pi_version = package_version(&pi_dir)?;
        let out = Command::new("node")
            .arg("--version")
            .output()
            .context("running `node --version`")?;
        if !out.status.success() {
            bail!("`node --version` failed");
        }
        let node_version = node_version(&String::from_utf8_lossy(&out.stdout))?;
        Ok(Self {
            pi_dir,
            pi_version,
            node_version,
        })
    }
}

/// `v26.11.1\n` → `26.11.1`, refusing a Node older than `MIN_NODE`.
fn node_version(output: &str) -> Result<String> {
    let version = output.trim().trim_start_matches('v');
    let mut parts = version.split('.').map(str::parse::<u64>);
    let (Some(Ok(major)), Some(Ok(minor))) = (parts.next(), parts.next()) else {
        bail!("unrecognised `node --version` output: {output:?}");
    };
    if (major, minor) < MIN_NODE {
        bail!(
            "node {version} is too old: pibake needs >= {}.{} for `--build-sea`",
            MIN_NODE.0,
            MIN_NODE.1
        );
    }
    Ok(version.to_owned())
}

fn package_version(dir: &Path) -> Result<String> {
    let path = dir.join("package.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let json: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    json["version"]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("{} has no version", path.display()))
}

/// What a lock pins, read from the setup and host as they are now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub pi_version: String,
    pub node_version: String,
    /// npm package name → installed version.
    pub npm: BTreeMap<String, String>,
    /// `<host>/<path>` → (url, HEAD commit).
    pub git: BTreeMap<String, (String, String)>,
}

impl Setup {
    /// Local packages are part of the setup itself and are not pinned.
    pub fn read(inventory: &Inventory, host: &Host, warn: &mut dyn FnMut(String)) -> Result<Self> {
        let mut npm = BTreeMap::new();
        let mut git = BTreeMap::new();
        for package in &inventory.packages {
            let not_installed = || {
                format!(
                    "{} is in settings.json but not installed at {}",
                    package.source,
                    package.install_path.display()
                )
            };
            match &package.parsed {
                Source::Npm { name, .. } => {
                    let installed = package.installed.as_ref().with_context(not_installed)?;
                    let version = installed.version.clone().with_context(|| {
                        format!("{}: installed package.json has no version", package.source)
                    })?;
                    npm.insert(name.clone(), version);
                }
                Source::Git {
                    repo, host, path, ..
                } => {
                    if package.installed.is_none() {
                        bail!(not_installed());
                    }
                    let commit = git_head(&package.install_path, &mut *warn)
                        .with_context(|| package.source.clone())?;
                    git.insert(format!("{host}/{path}"), (repo.clone(), commit));
                }
                Source::Local { .. } => {}
            }
        }
        Ok(Self {
            pi_version: host.pi_version.clone(),
            node_version: host.node_version.clone(),
            npm,
            git,
        })
    }
}

/// `HEAD` of the checkout at `dir`. Warns when tracked files differ from it.
fn git_head(dir: &Path, warn: &mut dyn FnMut(String)) -> Result<String> {
    // Without its own `.git`, git would report an enclosing repository's HEAD.
    if !dir.join(".git").exists() {
        bail!("{} is not a git checkout", dir.display());
    }
    // `--no-optional-locks`: `status` must not refresh the index, a write into the agent dir.
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {} in {}: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        warn(format!(
            "{} has local changes to tracked files; the lock pins its HEAD commit only",
            dir.display()
        ));
    }
    Ok(commit)
}

/// What differs between a lock and the setup now; empty when the lock still holds.
/// `locked_npm` is the npm packages pinned in `pibake.package-lock.json`, Pi excluded.
pub fn drift(lock: &Lock, locked_npm: &BTreeMap<String, String>, setup: &Setup) -> Vec<String> {
    let mut out = vec![];
    if lock.pi.version != setup.pi_version {
        out.push(format!(
            "pi: locked {}, installed {}",
            lock.pi.version, setup.pi_version
        ));
    }
    if lock.node.version != setup.node_version {
        out.push(format!(
            "node: locked {}, host has {}",
            lock.node.version, setup.node_version
        ));
    }
    diff_maps("npm", locked_npm, &setup.npm, &mut out);
    let pin = |url: &str, commit: &str| format!("{commit} ({url})");
    let locked_git = lock
        .git
        .iter()
        .map(|g| (g.name.clone(), pin(&g.url, &g.commit)))
        .collect();
    let setup_git = setup
        .git
        .iter()
        .map(|(name, (url, commit))| (name.clone(), pin(url, commit)))
        .collect();
    diff_maps("git", &locked_git, &setup_git, &mut out);
    out
}

fn diff_maps(
    kind: &str,
    locked: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    let names: BTreeSet<&String> = locked.keys().chain(now.keys()).collect();
    for name in names {
        match (locked.get(name), now.get(name)) {
            (Some(l), Some(n)) if l != n => {
                out.push(format!("{kind} {name}: locked {l}, installed {n}"));
            }
            (Some(l), None) => out.push(format!(
                "{kind} {name}: locked at {l}, no longer in settings.json"
            )),
            (None, Some(n)) => out.push(format!("{kind} {name}: installed {n}, not locked")),
            _ => {}
        }
    }
}

pub enum Outcome {
    /// The lock still holds; nothing was written.
    UpToDate,
    Drift(Vec<String>),
    /// Lock files whose bytes changed; empty when re-pinning gave the same lock.
    Written(Vec<PathBuf>),
}

/// Runs npm in `dir` to turn its `package.json` into a `package-lock.json`.
pub type NpmResolver<'a> = &'a dyn Fn(&Path) -> Result<()>;

pub struct Locker<'a> {
    pub agent_dir: &'a Path,
    /// Where the lock files live.
    pub dir: &'a Path,
    pub host: &'a Host,
    pub npm: NpmResolver<'a>,
}

impl Locker<'_> {
    pub fn run(
        &self,
        inventory: &Inventory,
        update: bool,
        warn: &mut dyn FnMut(String),
    ) -> Result<Outcome> {
        let setup = Setup::read(inventory, self.host, warn)?;
        if !update && let Some(drift) = self.check(&setup)? {
            return Ok(if drift.is_empty() {
                Outcome::UpToDate
            } else {
                Outcome::Drift(drift)
            });
        }
        let (lock, npm_lock) = self.generate(&setup, warn)?;
        let mut written = vec![];
        // The npm lock first: `pibake.lock` carries its hash, so it is the commit point.
        for (name, bytes) in [
            (NPM_LOCK_FILE, npm_lock),
            (LOCK_FILE, lock.render().into_bytes()),
        ] {
            let path = self.dir.join(name);
            if write_if_changed(&path, &bytes)? {
                written.push(path);
            }
        }
        Ok(Outcome::Written(written))
    }

    /// `None` without lock files; else what drifted (empty when the lock holds).
    fn check(&self, setup: &Setup) -> Result<Option<Vec<String>>> {
        let lock_path = self.dir.join(LOCK_FILE);
        let npm_path = self.dir.join(NPM_LOCK_FILE);
        let read = |path: &Path| match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let (lock_text, npm_bytes) = match (read(&lock_path)?, read(&npm_path)?) {
            (None, None) => return Ok(None),
            (Some(lock), Some(npm)) => (lock, npm),
            (Some(_), None) => return Ok(Some(vec![format!("{NPM_LOCK_FILE} is missing")])),
            (None, Some(_)) => return Ok(Some(vec![format!("{LOCK_FILE} is missing")])),
        };
        let lock = Lock::parse(&String::from_utf8_lossy(&lock_text))
            .with_context(|| format!("parsing {}", lock_path.display()))?;
        let sha = sha256_hex(&npm_bytes);
        if sha != lock.npm_lock.sha256 {
            return Ok(Some(vec![format!(
                "{NPM_LOCK_FILE} changed since it was locked (sha256 {sha}, locked {})",
                lock.npm_lock.sha256
            )]));
        }
        let in_npm_lock = || format!("in {}", npm_path.display());
        let npm_json: Value = serde_json::from_slice(&npm_bytes).with_context(in_npm_lock)?;
        let mut locked_npm = root_deps(&npm_json, "dependencies").with_context(in_npm_lock)?;
        locked_npm.remove(PI_PACKAGE);
        let mut out = drift(&lock, &locked_npm, setup);
        let locked_dev = root_deps(&npm_json, "devDependencies").with_context(in_npm_lock)?;
        if locked_dev != dev_dependencies() {
            out.push(format!(
                "esbuild: locked {}, pibake uses {ESBUILD_VERSION}",
                locked_dev.get("esbuild").map_or("none", String::as_str)
            ));
        }
        Ok(Some(out))
    }

    fn generate(&self, setup: &Setup, warn: &mut dyn FnMut(String)) -> Result<(Lock, Vec<u8>)> {
        let work = tempfile::tempdir()?;
        let mut dependencies = setup.npm.clone();
        dependencies.insert(PI_PACKAGE.to_owned(), setup.pi_version.clone());
        let dev_dependencies = dev_dependencies();
        let package_json = json!({
            "name": "pibake-stage",
            "private": true,
            "dependencies": dependencies,
            "devDependencies": dev_dependencies,
        });
        fs::write(
            work.path().join("package.json"),
            format!("{}\n", serde_json::to_string_pretty(&package_json)?),
        )?;
        // npm keeps already-locked versions that still satisfy, so seeding with the lock of the
        // installed extensions pins their transitive dependencies to what is installed.
        let seed = self.agent_dir.join("npm/package-lock.json");
        if seed.is_file() {
            fs::copy(&seed, work.path().join("package-lock.json"))?;
        }
        (self.npm)(work.path())?;
        let npm_lock = fs::read(work.path().join("package-lock.json"))
            .context("npm wrote no package-lock.json")?;
        let lock_json: Value = serde_json::from_slice(&npm_lock)?;

        if root_deps(&lock_json, "dependencies")? != dependencies
            || root_deps(&lock_json, "devDependencies")? != dev_dependencies
        {
            bail!("npm's package-lock.json root does not list the generated dependencies");
        }
        for (name, version) in dependencies.iter().chain(&dev_dependencies) {
            let entry = &lock_json["packages"][format!("node_modules/{name}")];
            if entry["version"].as_str() != Some(version) {
                bail!("npm locked {name} at {}, not {version}", entry["version"]);
            }
        }
        let integrity = lock_json["packages"][format!("node_modules/{PI_PACKAGE}")]["integrity"]
            .as_str()
            .context("npm's lock has no integrity for Pi")?
            .to_owned();

        let installed_roots = [
            self.host.pi_dir.clone(),
            self.agent_dir.join("npm/node_modules"),
        ];
        let missing = not_installed(&lock_json, &installed_roots);
        if !missing.is_empty() {
            warn(format!(
                "{} locked packages are not installed at that version, so the build will \
                 differ from stock Pi in them: {}",
                missing.len(),
                missing.join(", ")
            ));
        }

        let lock = Lock::pin(setup, integrity, sha256_hex(&npm_lock));
        Ok((lock, npm_lock))
    }
}

/// `npm install --package-lock-only` with Pi's own peer handling (`--legacy-peer-deps`,
/// `getNpmInstallArgs` in `dist/core/package-manager.js`).
pub fn npm_lock_only(dir: &Path) -> Result<()> {
    let out = Command::new("npm")
        .args([
            "install",
            "--package-lock-only",
            "--ignore-scripts",
            "--legacy-peer-deps",
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(dir)
        .output()
        .context("running npm")?;
    if !out.status.success() {
        bail!(
            "npm install --package-lock-only failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn dev_dependencies() -> BTreeMap<String, String> {
    BTreeMap::from([("esbuild".to_owned(), ESBUILD_VERSION.to_owned())])
}

/// `field` (`dependencies`, `devDependencies`) of the npm lock's root package; absent is empty.
pub(crate) fn root_deps(npm_lock: &Value, field: &str) -> Result<BTreeMap<String, String>> {
    let root = &npm_lock["packages"][""];
    if !root.is_object() {
        bail!("no root package");
    }
    let Some(deps) = root.get(field) else {
        return Ok(BTreeMap::new());
    };
    deps.as_object()
        .with_context(|| format!("root {field}: not an object"))?
        .iter()
        .map(|(name, v)| {
            let v = v
                .as_str()
                .with_context(|| format!("{name}: not a string"))?;
            Ok((name.clone(), v.to_owned()))
        })
        .collect()
}

/// `name@version` of each package in the npm lock that no installed tree under `roots` has.
fn not_installed(npm_lock: &Value, roots: &[PathBuf]) -> Vec<String> {
    let mut installed = BTreeSet::new();
    for root in roots {
        if root.join("package.json").is_file() {
            collect_package(root, &mut installed);
        } else {
            collect_node_modules(root, &mut installed);
        }
    }
    let Some(packages) = npm_lock["packages"].as_object() else {
        return vec![];
    };
    let locked: BTreeSet<String> = packages
        .iter()
        // Dev entries are pibake's own (esbuild), never part of the installed setup.
        .filter(|(path, entry)| !path.is_empty() && entry["link"] != true && entry["dev"] != true)
        .filter_map(|(path, entry)| {
            let name = entry["name"].as_str().unwrap_or_else(|| {
                path.rsplit_once("node_modules/")
                    .map_or(path.as_str(), |(_, n)| n)
            });
            Some(format!("{name}@{}", entry["version"].as_str()?))
        })
        .collect();
    locked.difference(&installed).cloned().collect()
}

fn collect_package(dir: &Path, out: &mut BTreeSet<String>) {
    if let Ok(text) = fs::read_to_string(dir.join("package.json"))
        && let Ok(json) = serde_json::from_str::<Value>(&text)
        && let (Some(name), Some(version)) = (json["name"].as_str(), json["version"].as_str())
    {
        out.insert(format!("{name}@{version}"));
    }
    collect_node_modules(&dir.join("node_modules"), out);
}

fn collect_node_modules(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Real directories only: symlinked packages could loop.
        if name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if name.starts_with('@') {
            collect_node_modules(&entry.path(), out);
        } else {
            collect_package(&entry.path(), out);
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Atomic, and a no-op when `path` already holds `bytes`, so its mtime survives.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    let dir = path.parent().context("lock path has no parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    // `NamedTempFile` creates 0600; lock files are ordinary project files.
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Lock {
        Lock {
            pi: PiPin {
                version: "1.3.0".into(),
                integrity: "sha512-abc==".into(),
            },
            node: NodePin {
                version: "26.11.1".into(),
            },
            npm_lock: NpmLockPin {
                sha256: "00ff".into(),
            },
            git: vec![
                GitPin {
                    name: "z.org/o/r".into(),
                    url: "https://z.org/o/r".into(),
                    commit: "2222".into(),
                },
                GitPin {
                    name: "a.org/o/r".into(),
                    url: "git@a.org:o/r".into(),
                    commit: "1111".into(),
                },
            ],
        }
    }

    fn setup() -> Setup {
        Setup {
            pi_version: "1.3.0".into(),
            node_version: "26.11.1".into(),
            npm: BTreeMap::from([("pi-btw".into(), "0.8.0".into())]),
            git: BTreeMap::from([("a.org/o/r".into(), ("git@a.org:o/r".into(), "1111".into()))]),
        }
    }

    #[test]
    fn renders_canonical_toml() {
        let text = sample().render();
        assert_eq!(
            text,
            format!(
                "{HEADER}[pi]\nversion = \"1.3.0\"\nintegrity = \"sha512-abc==\"\n\n[node]\nversion = \"26.11.1\"\n\n\
                 [npm_lock]\nsha256 = \"00ff\"\n\n[[git]]\nname = \"a.org/o/r\"\nurl = \"git@a.org:o/r\"\ncommit = \"1111\"\n\n\
                 [[git]]\nname = \"z.org/o/r\"\nurl = \"https://z.org/o/r\"\ncommit = \"2222\"\n"
            )
        );
        let mut sorted = sample();
        sorted.git.sort();
        assert_eq!(Lock::parse(&text).unwrap(), sorted);
        let mut no_git = sample();
        no_git.git.clear();
        assert!(!no_git.render().contains("git"));
        assert_eq!(Lock::parse(&no_git.render()).unwrap(), no_git);
    }

    #[test]
    fn parse_rejects_unknown_and_missing_keys() {
        let text = sample().render();
        assert!(Lock::parse(&text.replace("[node]", "[nodee]")).is_err());
        assert!(Lock::parse(&text.replace("commit = \"1111\"", "sha = \"1111\"")).is_err());
        assert!(Lock::parse(&text.replace("integrity = \"sha512-abc==\"\n", "")).is_err());
    }

    #[test]
    fn node_version_needs_build_sea() {
        assert_eq!(node_version("v26.11.1\n").unwrap(), "26.11.1");
        assert_eq!(node_version("v25.5.0").unwrap(), "25.5.0");
        assert!(node_version("v25.4.9").is_err());
        assert!(node_version("v24.99.0").is_err());
        assert!(node_version("not node").is_err());
    }

    #[test]
    fn a_fresh_lock_has_no_drift() {
        let s = setup();
        let lock = Lock::pin(&s, "i".into(), "h".into());
        assert!(drift(&lock, &s.npm, &s).is_empty());
    }

    #[test]
    fn drift_names_each_change() {
        let s = setup();
        let lock = Lock::pin(&s, "i".into(), "h".into());
        let mut now = s.clone();
        now.pi_version = "1.4.0".into();
        now.node_version = "26.12.0".into();
        now.npm.insert("pi-btw".into(), "0.8.1".into());
        now.npm.insert("new-pkg".into(), "1.0.0".into());
        now.git.clear();
        now.git.insert(
            "b.org/o/r".into(),
            ("https://b.org/o/r".into(), "3333".into()),
        );
        assert_eq!(
            drift(&lock, &s.npm, &now),
            [
                "pi: locked 1.3.0, installed 1.4.0",
                "node: locked 26.11.1, host has 26.12.0",
                "npm new-pkg: installed 1.0.0, not locked",
                "npm pi-btw: locked 0.8.0, installed 0.8.1",
                "git a.org/o/r: locked at 1111 (git@a.org:o/r), no longer in settings.json",
                "git b.org/o/r: installed 3333 (https://b.org/o/r), not locked",
            ]
        );
        let mut moved = s.clone();
        moved.git.get_mut("a.org/o/r").unwrap().1 = "4444".into();
        assert_eq!(
            drift(&lock, &s.npm, &moved),
            ["git a.org/o/r: locked 1111 (git@a.org:o/r), installed 4444 (git@a.org:o/r)"]
        );
    }

    #[test]
    fn not_installed_compares_name_and_version_across_trees() {
        let dir = tempfile::tempdir().unwrap();
        let write = |rel: &str, name: &str, version: &str| {
            let p = dir.path().join(rel);
            fs::create_dir_all(&p).unwrap();
            fs::write(
                p.join("package.json"),
                json!({"name": name, "version": version}).to_string(),
            )
            .unwrap();
        };
        write("pi", "pi", "1.0.0");
        write("pi/node_modules/a", "a", "1.0.0");
        write("pi/node_modules/@s/b", "@s/b", "2.0.0");
        write("pi/node_modules/a/node_modules/c", "c", "3.0.0");
        write("ext/node_modules/d", "d", "4.0.0");
        write("ext/node_modules/.hidden", "h", "0.0.0");
        let lock = json!({"packages": {
            "": {"name": "root"},
            "node_modules/pi": {"version": "1.0.0"},
            "node_modules/a": {"version": "1.0.0"},
            "node_modules/@s/b": {"version": "2.0.1"},
            "node_modules/x/node_modules/c": {"version": "3.0.0"},
            "node_modules/d": {"version": "4.0.0"},
            "node_modules/aliased": {"name": "e", "version": "5.0.0"},
            "node_modules/linked": {"link": true, "resolved": "../x"},
            "node_modules/esbuild": {"version": "0.28.2", "dev": true},
        }});
        let roots = [dir.path().join("pi"), dir.path().join("ext/node_modules")];
        assert_eq!(not_installed(&lock, &roots), ["@s/b@2.0.1", "e@5.0.0"]);
    }

    #[test]
    fn write_if_changed_keeps_mtime_and_writes_0644() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        assert!(write_if_changed(&path, b"one").unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!write_if_changed(&path, b"one").unwrap());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert!(write_if_changed(&path, b"two").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"two");
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
