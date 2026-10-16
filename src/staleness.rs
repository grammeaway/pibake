//! The setup an executable is built from, recorded in it (`sea::Builder`) so that at startup it
//! can tell the live setup has moved on (`bundle/staleness.mjs`). Kept as components, not one
//! hash, so the check can name what changed.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inventory::Inventory;
use crate::lock::{self, Host};
use crate::package::Source;
use crate::stage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub pi: PiFile,
    /// `packages` of `settings.json` as written; `null` when absent.
    pub packages: Value,
    pub npm: Vec<NpmPackage>,
    pub git: Vec<GitPackage>,
    pub extensions: Vec<ExtensionFile>,
}

/// The global Pi the lock pins, by its `package.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiFile {
    pub path: String,
    pub version: String,
}

/// `path` is the install directory relative to the agent directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NpmPackage {
    pub source: String,
    pub path: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitPackage {
    pub source: String,
    pub path: String,
    pub commit: String,
}

/// A file under the agent directory's `extensions/`, as the stage reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionFile {
    pub path: String,
    pub size: u64,
    /// Nanoseconds since the epoch, as Node's `stat(…, { bigint: true }).mtimeNs`; a string,
    /// since JSON numbers cannot hold it exactly.
    pub mtime_ns: String,
    pub sha256: String,
}

/// Read before the lock is checked and the stage reads `extensions/`: a change made during the
/// build then shows as stale at runtime instead of going unnoticed.
pub fn read(inventory: &Inventory, host: &Host) -> Result<Fingerprint> {
    let agent_dir = &inventory.agent_dir;
    let rel = |path: &Path| utf8(path.strip_prefix(agent_dir).unwrap_or(path));
    let (mut npm, mut git) = (vec![], vec![]);
    for package in &inventory.packages {
        match &package.parsed {
            Source::Npm { .. } => npm.push(NpmPackage {
                source: package.source.clone(),
                path: rel(&package.install_path)?,
                version: package.installed.as_ref().and_then(|i| i.version.clone()),
            }),
            Source::Git { .. } => git.push(GitPackage {
                source: package.source.clone(),
                path: rel(&package.install_path)?,
                commit: lock::git_head(&package.install_path, &mut |_| {})
                    .with_context(|| package.source.clone())?,
            }),
            // Refused by the stage.
            Source::Local { .. } => {}
        }
    }
    let mut extensions = vec![];
    let dir = agent_dir.join("extensions");
    if dir.exists() {
        stage::walk_tree(&dir, "", &mut |path, file, meta| {
            let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            extensions.push(ExtensionFile {
                path,
                size: meta.len(),
                mtime_ns: mtime_ns(meta),
                sha256: lock::sha256_hex(&bytes),
            });
            Ok(())
        })?;
    }
    extensions.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Fingerprint {
        pi: PiFile {
            path: utf8(&host.pi_dir.join("package.json"))?,
            version: host.pi_version.clone(),
        },
        packages: inventory.settings_packages.clone(),
        npm,
        git,
        extensions,
    })
}

fn mtime_ns(meta: &fs::Metadata) -> String {
    (i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())).to_string()
}

fn utf8(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{}: path is not UTF-8", path.display()))
}
