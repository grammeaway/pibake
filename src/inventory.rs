//! What a Pi agent directory holds, split into config, installed packages and state.
//! State is listed by name only: its contents (credentials, sessions) are never read.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::package::{Filter, PackageEntry, RESOURCE_TYPES, Source, string_array};
use crate::paths;

/// Top-level entries Pi documents as user configuration (`docs/configuration.md`),
/// plus `agents/`, which `@gotgenes/pi-subagents` reads.
const CONFIG_NAMES: [&str; 16] = [
    "settings.json",
    "keybindings.json",
    "mcp.json",
    "models.json",
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "agents",
];

/// Where Pi installs npm and git packages; read only for package manifests.
const INSTALL_ROOTS: [&str; 2] = ["npm", "git"];

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub agent_dir: PathBuf,
    pub packages: Vec<Package>,
    /// The `extensions`, `skills`, `prompts` and `themes` arrays of `settings.json`, raw.
    pub settings_resources: BTreeMap<&'static str, Vec<String>>,
    pub local: LocalResources,
    pub config: Vec<String>,
    pub install_roots: Vec<String>,
    pub state: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Package {
    pub source: String,
    pub filter: Option<Filter>,
    pub parsed: Source,
    pub identity: String,
    pub install_path: PathBuf,
    /// `None` when nothing is at `install_path`.
    pub installed: Option<Installed>,
}

/// Paths are relative to the package root, `/`-separated.
#[derive(Debug, Serialize)]
pub struct Installed {
    pub version: Option<String>,
    pub manifest: ManifestKind,
    /// Extension entry points by Pi's discovery rules, before filters and `!`/`+`/`-` patterns.
    pub extensions: Vec<String>,
    /// Declared roots: the manifest's entries, or the conventional directory.
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
    /// Extension entries pibake does not resolve itself; they are listed, never dropped.
    pub unresolved: Vec<Unresolved>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestKind {
    /// `package.json` has a `pi` object.
    Pi,
    /// No `pi` object: Pi's conventional directories apply.
    Conventional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unresolved {
    pub entry: String,
    pub reason: UnresolvedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    Glob,
    Pattern,
    Missing,
}

/// Paths are relative to the agent directory.
#[derive(Debug, Default, Serialize)]
pub struct LocalResources {
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
    pub agents: Vec<String>,
}

pub fn read(agent_dir: &Path, home: &Path) -> Result<Inventory> {
    if !agent_dir.is_dir() {
        bail!("agent directory {} does not exist", agent_dir.display());
    }
    let settings = read_settings(&agent_dir.join("settings.json"))?;
    let packages: Vec<Package> = package_entries(&settings)?
        .into_iter()
        .map(|entry| read_package(entry, agent_dir, home))
        .collect::<Result<_>>()?;
    let settings_resources: BTreeMap<_, _> = RESOURCE_TYPES
        .into_iter()
        .filter_map(|t| Some((t, string_array(settings.get(t)?)?)))
        .collect();

    // Top-level entries that settings.json points at (local packages, resource paths) are config too.
    let local_package_paths = packages
        .iter()
        .filter(|p| matches!(p.parsed, Source::Local { .. }))
        .map(|p| p.install_path.clone());
    let resource_paths = settings_resources
        .values()
        .flatten()
        .filter(|e| !e.starts_with(['!', '+', '-']) && !e.starts_with("builtin:"))
        .map(|e| paths::resolve(agent_dir, e, home));
    let referenced: Vec<String> = local_package_paths
        .chain(resource_paths)
        .filter_map(|p| {
            let first = p.strip_prefix(agent_dir).ok()?.components().next()?;
            Some(first.as_os_str().to_string_lossy().into_owned())
        })
        .collect();

    let (mut config, mut install_roots, mut state) = (vec![], vec![], vec![]);
    for name in sorted_names(agent_dir)? {
        if CONFIG_NAMES.contains(&name.as_str()) || referenced.contains(&name) {
            config.push(name);
        } else if INSTALL_ROOTS.contains(&name.as_str()) {
            install_roots.push(name);
        } else {
            state.push(name);
        }
    }

    let rel = |p: &Path| paths::display_rel(agent_dir, p);
    let names = |dir: &str| visible_names(&agent_dir.join(dir));
    let local = LocalResources {
        extensions: collect_auto_extension_entries(&agent_dir.join("extensions"))
            .iter()
            .map(|p| rel(p))
            .collect(),
        skills: names("skills")?,
        prompts: names("prompts")?,
        themes: names("themes")?,
        agents: names("agents")?,
    };

    Ok(Inventory {
        agent_dir: agent_dir.to_path_buf(),
        packages,
        settings_resources,
        local,
        config,
        install_roots,
        state,
    })
}

pub fn render_text(inv: &Inventory) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let list = |items: &[String]| {
        if items.is_empty() {
            "-".to_owned()
        } else {
            items.join(", ")
        }
    };
    let _ = writeln!(s, "agent dir: {}", inv.agent_dir.display());
    let _ = writeln!(s, "packages ({}):", inv.packages.len());
    for p in &inv.packages {
        let Some(installed) = &p.installed else {
            let _ = writeln!(
                s,
                "  {}  NOT INSTALLED at {}",
                p.source,
                p.install_path.display()
            );
            continue;
        };
        let version = installed.version.as_deref().unwrap_or("no version");
        let _ = writeln!(s, "  {}  {version}", p.source);
        if let Some(filter) = &p.filter {
            let _ = writeln!(
                s,
                "    filter: {}",
                serde_json::to_string(filter).unwrap_or_default()
            );
        }
        for (label, items) in [
            ("extensions", &installed.extensions),
            ("skills", &installed.skills),
            ("prompts", &installed.prompts),
            ("themes", &installed.themes),
        ] {
            if !items.is_empty() {
                let _ = writeln!(s, "    {label}: {}", list(items));
            }
        }
        for u in &installed.unresolved {
            let _ = writeln!(s, "    unresolved: {} ({:?})", u.entry, u.reason);
        }
    }
    for (key, items) in &inv.settings_resources {
        let _ = writeln!(s, "settings.json {key}: {}", list(items));
    }
    let local = &inv.local;
    let _ = writeln!(s, "local extensions: {}", list(&local.extensions));
    for (label, items) in [
        ("skills", &local.skills),
        ("prompts", &local.prompts),
        ("themes", &local.themes),
        ("agents", &local.agents),
    ] {
        let _ = writeln!(s, "local {label}: {}", items.len());
    }
    let _ = writeln!(s, "config: {}", list(&inv.config));
    let _ = writeln!(s, "install roots: {}", list(&inv.install_roots));
    let _ = writeln!(s, "state (names only): {}", list(&inv.state));
    s
}

fn read_settings(path: &Path) -> Result<Value> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Object(Default::default()));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("parsing {}", path.display()))?;
    if !value.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    Ok(value)
}

fn package_entries(settings: &Value) -> Result<Vec<PackageEntry>> {
    let Some(packages) = settings.get("packages") else {
        return Ok(vec![]);
    };
    let Some(packages) = packages.as_array() else {
        bail!("settings.json `packages` is not an array");
    };
    packages
        .iter()
        .enumerate()
        .map(|(i, v)| {
            PackageEntry::from_json(v).with_context(|| format!("settings.json packages[{i}]"))
        })
        .collect()
}

fn read_package(entry: PackageEntry, agent_dir: &Path, home: &Path) -> Result<Package> {
    let parsed = Source::parse(&entry.source)?;
    let install_path = parsed.install_path(agent_dir, home);
    let installed = install_path
        .is_dir()
        .then(|| read_installed(&install_path, entry.filter.as_ref()));
    Ok(Package {
        identity: parsed.identity(agent_dir, home),
        source: entry.source,
        filter: entry.filter,
        parsed,
        install_path,
        installed,
    })
}

struct PackageJson {
    version: Option<String>,
    /// `None` without a `pi` object. Fields that are not string arrays are dropped, as Pi's `readPiManifest` does.
    pi: Option<BTreeMap<&'static str, Vec<String>>>,
}

/// Unreadable or invalid `package.json` reads as absent, as in Pi.
fn read_package_json(path: &Path) -> PackageJson {
    let value: Option<Value> = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());
    let Some(Value::Object(object)) = value else {
        return PackageJson {
            version: None,
            pi: None,
        };
    };
    let pi = object.get("pi").and_then(Value::as_object).map(|pi| {
        RESOURCE_TYPES
            .into_iter()
            .filter_map(|t| Some((t, string_array(pi.get(t)?)?)))
            .collect()
    });
    PackageJson {
        version: object
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        pi,
    }
}

/// Where a resource type's candidates come from, per Pi's `collectPackageResources`.
enum Origin<'a> {
    Manifest(&'a [String]),
    Conventional,
    Nothing,
}

fn origin<'a>(
    manifest: Option<&'a BTreeMap<&'static str, Vec<String>>>,
    filter: Option<&Filter>,
    resource_type: &str,
) -> Origin<'a> {
    let entries = manifest
        .and_then(|m| m.get(resource_type))
        .map(Vec::as_slice);
    match filter {
        // String form: a `pi` object is authoritative, even for types it omits.
        None if manifest.is_some() => entries.map_or(Origin::Nothing, Origin::Manifest),
        None => Origin::Conventional,
        // Object form with patterns (or a delta): `collectManifestFiles` falls back on an empty list.
        Some(f) if f.autoload == Some(false) || f.patterns(resource_type).is_some() => {
            match entries {
                Some(e) if !e.is_empty() => Origin::Manifest(e),
                _ => Origin::Conventional,
            }
        }
        // Object form without patterns: `collectDefaultResources` keeps even an empty list.
        Some(_) => entries.map_or(Origin::Conventional, Origin::Manifest),
    }
}

fn read_installed(root: &Path, filter: Option<&Filter>) -> Installed {
    let package_json = read_package_json(&root.join("package.json"));
    let manifest = package_json.pi.as_ref();
    let rel = |p: &Path| paths::display_rel(root, p);

    let mut unresolved = vec![];
    let extensions = match origin(manifest, filter, "extensions") {
        Origin::Manifest(entries) => {
            let mut found = vec![];
            for entry in entries {
                let reason = if entry.starts_with(['!', '+', '-']) {
                    Some(UnresolvedReason::Pattern)
                } else if entry.contains(['*', '?']) {
                    Some(UnresolvedReason::Glob)
                } else {
                    let path = paths::normalize(&root.join(entry));
                    if path.is_file() {
                        found.push(path);
                        None
                    } else if path.is_dir() {
                        found.extend(collect_auto_extension_entries(&path));
                        None
                    } else {
                        Some(UnresolvedReason::Missing)
                    }
                };
                if let Some(reason) = reason {
                    unresolved.push(Unresolved {
                        entry: entry.clone(),
                        reason,
                    });
                }
            }
            found
        }
        Origin::Conventional => collect_auto_extension_entries(&root.join("extensions")),
        Origin::Nothing => vec![],
    };

    let declared = |resource_type: &str| match origin(manifest, filter, resource_type) {
        Origin::Manifest(entries) => entries.to_vec(),
        Origin::Conventional if root.join(resource_type).is_dir() => vec![resource_type.to_owned()],
        _ => vec![],
    };

    Installed {
        version: package_json.version,
        manifest: if manifest.is_some() {
            ManifestKind::Pi
        } else {
            ManifestKind::Conventional
        },
        extensions: extensions.iter().map(|p| rel(p)).collect(),
        skills: declared("skills"),
        prompts: declared("prompts"),
        themes: declared("themes"),
        unresolved,
    }
}

/// Pi's `collectAutoExtensionEntries`: the directory's own entry points if it has any,
/// else its `.ts`/`.js` files and the entry points of its subdirectories.
// shortcut: `.gitignore`/`.ignore`/`.fdignore` files are not honoured, and entries are sorted where Pi keeps readdir order; port both if a setup relies on them.
fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return vec![];
    }
    if let Some(entries) = resolve_extension_entries(dir) {
        return entries;
    }
    let mut entries = vec![];
    for name in visible_names(dir).unwrap_or_default() {
        let path = dir.join(&name);
        // `metadata` follows symlinks, as Pi's `statSync` fallback does.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_file() && (name.ends_with(".ts") || name.ends_with(".js")) {
            entries.push(path);
        } else if meta.is_dir() {
            entries.extend(resolve_extension_entries(&path).unwrap_or_default());
        }
    }
    entries
}

/// Pi's `resolveExtensionEntries`: existing `pi.extensions` paths, else `index.ts`, else `index.js`.
fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json = dir.join("package.json");
    if package_json.exists()
        && let Some(declared) = read_package_json(&package_json)
            .pi
            .and_then(|mut pi| pi.remove("extensions"))
    {
        let existing: Vec<_> = declared
            .iter()
            .map(|e| paths::normalize(&dir.join(e)))
            .filter(|p| p.exists())
            .collect();
        if !existing.is_empty() {
            return Some(existing);
        }
    }
    ["index.ts", "index.js"]
        .into_iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists())
        .map(|p| vec![p])
}

fn sorted_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Sorted names without dot entries and `node_modules`, as Pi's discovery skips both.
fn visible_names(dir: &Path) -> Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    Ok(sorted_names(dir)?
        .into_iter()
        .filter(|n| !n.starts_with('.') && n != "node_modules")
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn filter(value: Value) -> Filter {
        PackageEntry::from_json(&value).unwrap().filter.unwrap()
    }

    #[test]
    fn auto_extension_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for f in [
            "a.ts",
            "b.js",
            "c.mjs",
            "notes.md",
            ".hidden.ts",
            "node_modules/x/index.ts",
        ] {
            write(d, f, "");
        }
        write(d, "sub/index.js", "");
        write(d, "sub/index.ts", "");
        write(
            d,
            "pkg/package.json",
            r#"{"pi":{"extensions":["./main.ts","./gone.ts"]}}"#,
        );
        write(d, "pkg/main.ts", "");
        write(d, "pkg/index.ts", "");
        write(d, "bad/package.json", "not json");
        write(d, "bad/index.js", "");
        write(d, "empty/readme.md", "");
        let found: Vec<_> = collect_auto_extension_entries(d)
            .iter()
            .map(|p| paths::display_rel(d, p))
            .collect();
        assert_eq!(
            found,
            [
                "a.ts",
                "b.js",
                "bad/index.js",
                "pkg/main.ts",
                "sub/index.ts"
            ]
        );
    }

    #[test]
    fn a_directory_with_its_own_entry_point_is_one_extension() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "index.ts", "");
        write(dir.path(), "other.ts", "");
        assert_eq!(
            collect_auto_extension_entries(dir.path()),
            [dir.path().join("index.ts")]
        );
    }

    #[test]
    fn origin_follows_pi_per_entry_form() {
        let with = |e: Vec<String>| BTreeMap::from([("extensions", e)]);
        let full = with(vec!["./x.ts".into()]);
        let empty = with(vec![]);
        let omitted = BTreeMap::new();
        let kind =
            |m: Option<&BTreeMap<&'static str, Vec<String>>>, f: Option<&Filter>| match origin(
                m,
                f,
                "extensions",
            ) {
                Origin::Manifest(e) => format!("manifest{}", e.len()),
                Origin::Conventional => "conventional".into(),
                Origin::Nothing => "nothing".into(),
            };
        let bare = filter(json!({"source": "x"}));
        let patterned = filter(json!({"source": "x", "extensions": ["a"]}));
        let delta = filter(json!({"source": "x", "autoload": false}));

        assert_eq!(kind(Some(&full), None), "manifest1");
        assert_eq!(kind(Some(&omitted), None), "nothing");
        assert_eq!(kind(None, None), "conventional");
        assert_eq!(kind(Some(&empty), Some(&bare)), "manifest0");
        assert_eq!(kind(Some(&omitted), Some(&bare)), "conventional");
        assert_eq!(kind(Some(&empty), Some(&patterned)), "conventional");
        assert_eq!(kind(Some(&full), Some(&patterned)), "manifest1");
        assert_eq!(kind(Some(&empty), Some(&delta)), "conventional");
    }

    #[test]
    fn manifest_entries_resolve_files_directories_and_report_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "package.json",
            r#"{"version":"1.2.3","pi":{"extensions":["./src/index.ts","./dist","./ext/*.ts","!ext/skip.ts","./nope.ts"],"skills":["./skills"],"themes":"bad"}}"#,
        );
        write(d, "src/index.ts", "");
        write(d, "dist/index.js", "");
        write(d, "dist/helper.js", "");
        write(d, "prompts/p.md", "");
        let installed = read_installed(d, None);
        assert_eq!(installed.version.as_deref(), Some("1.2.3"));
        assert_eq!(installed.manifest, ManifestKind::Pi);
        assert_eq!(installed.extensions, ["src/index.ts", "dist/index.js"]);
        assert_eq!(installed.skills, ["./skills"]);
        // A `pi` object without `prompts` means none, even with a `prompts/` directory.
        assert!(installed.prompts.is_empty());
        assert!(installed.themes.is_empty());
        let reasons: Vec<_> = installed
            .unresolved
            .iter()
            .map(|u| (u.entry.as_str(), u.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                ("./ext/*.ts", UnresolvedReason::Glob),
                ("!ext/skip.ts", UnresolvedReason::Pattern),
                ("./nope.ts", UnresolvedReason::Missing),
            ]
        );
    }
}
