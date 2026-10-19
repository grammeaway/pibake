//! `pibake bundle`: Pi and every extension entry point of the stage in one file: an ES module; for
//! the `sea_snapshot` backend a CommonJS script that defers launch state to its resume; for
//! `sea_trained` CommonJS wrapped in one function expression, which a loader compiles and calls.
//!
//! Pi's own extension loader still decides what loads, from where and in what order. The bundle
//! only answers its jiti imports for the baked entry points (see `bundle/build.mjs`), so filters,
//! built-in toggles, project extensions and command source paths stay as in stock Pi.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use oxc::span::SourceType;
use serde::{Deserialize, Serialize};

use crate::config::Backend;
use crate::inventory::Inventory;
use crate::lock::PI_PACKAGE;
use crate::passes::{self, Pass, PassReport, snapshot_safety};
use crate::paths;
use crate::stage::{self, Outcome};

/// Under the directory holding `pibake.toml`.
pub const BUNDLE_ROOT: &str = "target/pibake/bundle";
/// What `node` runs.
pub const LAUNCHER: &str = "pi.mjs";
pub const REPORT: &str = "report.json";
const BUILD_SCRIPT: &str = include_str!("bundle/build.mjs");
const PROLOGUE: &str = include_str!("bundle/prologue.mjs");
const PROLOGUE_SNAPSHOT: &str = include_str!("bundle/prologue_snapshot.cjs");
const PROLOGUE_TRAINED: &str = include_str!("bundle/prologue_trained.cjs");
/// `sea_trained`'s plain-`node` launcher.
const LAUNCHER_TRAINED: &str = include_str!("bundle/pi.cjs");
/// `sea_trained`'s executable main, in the bundle so its path, which the executable embeds, is
/// stable.
const LOADER: (&str, &str) = ("loader.cjs", include_str!("bundle/loader_trained.cjs"));
/// Around a `sea_trained` bundle: the module wrapper as a function expression, so a loader can
/// compile the bundle text as it is, with a code cache, without a 20 MB concatenation.
const WRAPPER: (&str, &str) = (
    "(function (exports, require, module, __filename, __dirname) {\n",
    "\n})",
);
/// Both prologues' extraction of an executable's files, spliced in at `// pibake:extract`.
const EXTRACT: &str = include_str!("bundle/extract.js");
/// `pibake:staleness`: the entry's check of the setup an executable was built from.
pub const STALENESS: &str = include_str!("bundle/staleness.mjs");
/// `pibake:tool-json`: what `incremental_tool_json` calls instead of per-delta `parseStreamingJson`.
pub const TOOL_JSON: &str = include_str!("bundle/tool_json.mjs");
/// `pibake:frozen`'s code: the entry's check of settings passes baked in; `frozen_module` adds
/// the data.
pub const FROZEN: &str = include_str!("bundle/frozen.mjs");

/// Stock Pi's `dist/cli.js`, plus what Pi's single-file (Bun) entry `dist/bun/runtime-setup.js`
/// registers statically: OAuth flows and Bedrock, which Pi otherwise imports through variable
/// specifiers next to its release chunks. Resolved from Pi's package directory. First, in an
/// executable, the staleness check against the agent directory Pi will use. Pi's modules are
/// evaluated in the stock order: `dist/config.js` is already a dependency of `dist/main.js`.
/// Then the check of settings passes baked in.
const ENTRY: &str = r#"import { setupCli } from "./dist/cli/setup.js";
import { main } from "./dist/main.js";
import { getAgentDir } from "./dist/config.js";
import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { bedrockProviderModule } from "@earendil-works/pi-ai/bedrock-provider";
import { setBedrockProviderModule } from "@earendil-works/pi-ai/compat";
import { checkSetup } from "pibake:staleness";
import { checkFrozen, frozen } from "pibake:frozen";

checkSetup(getAgentDir(), globalThis.__pibake_setup);
checkFrozen(getAgentDir(), process.argv.slice(2), frozen);
registerBunOAuthFlows();
setBedrockProviderModule(bedrockProviderModule);
setupCli();
main(process.argv.slice(2));
"#;
/// `ENTRY` for a snapshot: the build evaluates every module, and the executable starts at `run`,
/// after the prologue's `__pibake_resume`. Traced (`bundle/trace.cjs`), it only evaluates.
const ENTRY_SNAPSHOT: &str = r#"import { startupSnapshot } from "node:v8";
import { setupCli } from "./dist/cli/setup.js";
import { main } from "./dist/main.js";
import { getAgentDir } from "./dist/config.js";
import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { bedrockProviderModule } from "@earendil-works/pi-ai/bedrock-provider";
import { setBedrockProviderModule } from "@earendil-works/pi-ai/compat";
import { checkSetup } from "pibake:staleness";
import { checkFrozen, frozen } from "pibake:frozen";

function run() {
	globalThis.__pibake_resume();
	checkSetup(getAgentDir(), globalThis.__pibake_setup);
	checkFrozen(getAgentDir(), process.argv.slice(2), frozen);
	registerBunOAuthFlows();
	setBedrockProviderModule(bedrockProviderModule);
	setupCli();
	main(process.argv.slice(2));
}

if (startupSnapshot.isBuildingSnapshot()) startupSnapshot.setDeserializeMainFunction(run);
else if (globalThis.__pibake_trace === undefined) run();
"#;
/// As stock Pi's `dist/bundle/cli.js`: the compile cache is on before the runtime loads.
const LAUNCHER_TEXT: &str = r#"import { enableCompileCache } from "node:module";

enableCompileCache();
await import("./main.mjs");
"#;

/// The stock entry `ENTRY` stands in for; a Pi release that changes it needs a pibake update.
const PI_CLI: &str = r#"#!/usr/bin/env node
import { setupCli } from "./cli/setup.js";
import { main } from "./main.js";
setupCli();
main(process.argv.slice(2));"#;
/// Calls in Pi's single-file entry setup. `ENTRY` makes the first two; the bundle finds
/// quickjs's wasm the way Pi's release bundle does.
const BUN_SETUP_CALLS: [&str; 3] = [
    "registerBunOAuthFlows",
    "setBedrockProviderModule",
    "setEmbeddedQuickJSWasmPath",
];
/// Worker files Pi's code finds beside its release chunks, where the bundle points it.
pub const PI_CHUNK_FILES: [&str; 2] = ["codemode-worker.js", "image-resize-worker.js"];
/// Pi's packages; their code is located as in Pi's release bundle, not rewritten.
const PI_SCOPE: &str = "node_modules/@earendil-works/";

/// One extension entry point: the path Pi's loader asks for, relative to the agent directory,
/// and the file in the stage that answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baked {
    /// The package source from `settings.json`, or `extensions` for local files.
    pub owner: String,
    pub key: String,
    pub stage_path: String,
}

/// A directory of extension code: in the stage, and where it is installed in the agent directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Owned {
    pub stage: String,
    pub live: String,
}

/// An entry point Pi may load that the bundle does not carry; Pi loads it through jiti.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unbaked {
    pub owner: String,
    pub entry: String,
    pub reason: String,
}

pub struct Plan {
    pub agent_dir: PathBuf,
    pub stage: PathBuf,
    pub baked: Vec<Baked>,
    pub owned: Vec<Owned>,
    pub unbaked: Vec<Unbaked>,
}

/// Where the stage keeps what the agent directory has at `agent_rel`.
pub fn stage_rel(agent_rel: &str) -> Option<String> {
    if let Some(rest) = agent_rel.strip_prefix("npm/node_modules/") {
        return Some(format!("node_modules/{rest}"));
    }
    if agent_rel.starts_with("git/") {
        return Some(agent_rel.to_owned());
    }
    agent_rel
        .strip_prefix("extensions/")
        .map(|rest| format!("local/extensions/{rest}"))
}

impl Plan {
    /// Every extension entry point the inventory resolves, checked to be in the stage.
    pub fn new(inventory: &Inventory, stage_dir: &Path) -> Result<Self> {
        let rel = |p: &Path| paths::display_rel(&inventory.agent_dir, p);
        let mut baked = vec![];
        let mut owned = BTreeSet::new();
        let mut unbaked = vec![];
        let mut add = |owner: &str, key: String| -> Result<()> {
            let stage_path = stage_rel(&key)
                .with_context(|| format!("{owner}: {key} is outside npm/, git/ and extensions/"))?;
            if !stage_dir.join(&stage_path).is_file() {
                bail!(
                    "{owner}: {stage_path} is not in the stage {}; run `pibake lock` and `pibake stage`",
                    stage_dir.display()
                );
            }
            baked.push(Baked {
                owner: owner.to_owned(),
                key,
                stage_path,
            });
            Ok(())
        };
        for package in &inventory.packages {
            let Some(installed) = &package.installed else {
                continue;
            };
            let root = rel(&package.install_path);
            let stage_root = stage_rel(&root).with_context(|| {
                format!(
                    "{}: installed at {root}, outside npm/ and git/",
                    package.source
                )
            })?;
            owned.insert(Owned {
                stage: stage_root,
                live: root.clone(),
            });
            for entry in &installed.extensions {
                add(&package.source, format!("{root}/{entry}"))?;
            }
            for u in &installed.unresolved {
                unbaked.push(Unbaked {
                    owner: package.source.clone(),
                    entry: u.entry.clone(),
                    reason: format!("{:?}", u.reason).to_lowercase(),
                });
            }
        }
        if !inventory.local.extensions.is_empty() {
            owned.insert(Owned {
                stage: "local/extensions".into(),
                live: "extensions".into(),
            });
        }
        for entry in &inventory.local.extensions {
            add("extensions", entry.clone())?;
        }
        Ok(Self {
            agent_dir: inventory.agent_dir.clone(),
            stage: stage_dir.to_path_buf(),
            baked,
            owned: owned.into_iter().collect(),
            unbaked,
        })
    }

    pub fn pi_dir(&self) -> PathBuf {
        self.stage.join("node_modules").join(PI_PACKAGE)
    }
}

/// The module Pi's loader asks through the jiti stand-in: agent-relative path → baked entry.
/// Resolved from the stage directory. It also gives extension code its installed location.
pub fn baked_module(baked: &[&Baked]) -> String {
    let mut s = String::from(
        r#"import { dirname, join, relative } from "node:path";
import { pathToFileURL } from "node:url";
import { getAgentDir } from "./node_modules/@earendil-works/pi-coding-agent/dist/config.js";

const baked = {
"#,
    );
    for b in baked {
        s.push_str(&format!(
            "\t{}: () => import({}),\n",
            js_string(&b.key),
            js_string(&format!("./{}", b.stage_path))
        ));
    }
    s.push_str(
        r#"};

export function bakedLoader(path) {
	const key = relative(getAgentDir(), path);
	return Object.hasOwn(baked, key) ? baked[key] : undefined;
}

globalThis.__pibake_live = (rel) => {
	const filename = join(getAgentDir(), rel);
	return { url: pathToFileURL(filename).href, dirname: dirname(filename), filename };
};
"#,
    );
    s
}

/// What the bundle's format and texts are for each backend.
struct Flavor {
    format: &'static str,
    main: &'static str,
    entry: &'static str,
    prologue: &'static str,
    /// What `node` runs, and its text, if not `main`.
    launcher: Option<(&'static str, &'static str)>,
    /// The executable's main, and its text, if not `main`; it runs `main` wrapped in `WRAPPER`.
    loader: Option<(&'static str, &'static str)>,
}

fn flavor(backend: Backend) -> Flavor {
    match backend {
        Backend::SeaCodecache => Flavor {
            format: "esm",
            main: "main.mjs",
            entry: ENTRY,
            prologue: PROLOGUE,
            launcher: Some((LAUNCHER, LAUNCHER_TEXT)),
            loader: None,
        },
        Backend::SeaSnapshot => Flavor {
            format: "cjs",
            main: "main.cjs",
            entry: ENTRY_SNAPSHOT,
            prologue: PROLOGUE_SNAPSHOT,
            launcher: None,
            loader: None,
        },
        Backend::SeaTrained => Flavor {
            format: "cjs",
            main: "main.cjs",
            entry: ENTRY,
            prologue: PROLOGUE_TRAINED,
            launcher: Some(("pi.cjs", LAUNCHER_TRAINED)),
            loader: Some(LOADER),
        },
    }
}

/// The bundle's main file: what an executable embeds.
pub fn main_file(backend: Backend) -> &'static str {
    flavor(backend).main
}

/// The executable's main: a loader of the main file, or the main file itself.
pub fn sea_main(backend: Backend) -> &'static str {
    let flavor = flavor(backend);
    flavor.loader.map_or(flavor.main, |(name, _)| name)
}

/// What `node` runs: a launcher, or a snapshot's CommonJS bundle itself.
pub fn launcher(backend: Backend) -> &'static str {
    let flavor = flavor(backend);
    flavor.launcher.map_or(flavor.main, |(name, _)| name)
}

fn prologue(flavor: &Flavor, stage: &Path) -> String {
    let text = flavor
        .prologue
        .replace("// pibake:extract\n", EXTRACT)
        .replace(
            r#""__PIBAKE_STAGE__""#,
            &js_string(&stage.to_string_lossy()),
        );
    if flavor.loader.is_some() {
        format!("{}{text}", WRAPPER.0)
    } else {
        text
    }
}

fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// Fails loudly when Pi's entry no longer has the shape `ENTRY` reproduces.
pub fn check_pi(pi_dir: &Path) -> Result<()> {
    let read = |rel: &str| {
        fs::read_to_string(pi_dir.join(rel))
            .with_context(|| format!("reading {rel} in {}", pi_dir.display()))
    };
    let cli = read("dist/cli.js")?;
    let cli: Vec<&str> = cli
        .lines()
        .filter(|l| !l.starts_with("//# sourceMappingURL"))
        .collect();
    if cli.join("\n").trim() != PI_CLI {
        bail!(
            "Pi's dist/cli.js is no longer `setupCli(); main(argv)`; pibake's entry needs an update:\n{}",
            cli.join("\n")
        );
    }
    let setup = read("dist/bun/runtime-setup.js")?;
    let calls: Vec<&str> = setup
        .lines()
        .filter_map(|l| {
            let name = l.split('(').next()?;
            (l.contains('(')
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .then_some(name)
        })
        .collect();
    for call in &calls {
        if !BUN_SETUP_CALLS.contains(call) {
            bail!("Pi's dist/bun/runtime-setup.js calls {call}(), which pibake's entry does not");
        }
    }
    for call in &BUN_SETUP_CALLS[..2] {
        if !calls.contains(call) {
            bail!("Pi's dist/bun/runtime-setup.js no longer calls {call}(); pibake's entry does");
        }
    }
    for file in PI_CHUNK_FILES {
        let path = pi_dir.join("dist/bundle/chunks").join(file);
        if !path.is_file() {
            bail!("{} is missing; Pi's release layout changed", path.display());
        }
    }
    Ok(())
}

/// What `build.mjs` reads.
#[derive(Debug, Clone, Serialize)]
pub struct BuildConfig {
    pub esbuild: PathBuf,
    pub stage: PathBuf,
    pub pi_dir: PathBuf,
    pub entry: String,
    /// The bundle's first code: where it is (the stage, or extracted files in an executable).
    pub prologue: String,
    /// The bundle's last code, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<&'static str>,
    /// The module `pibake:staleness`.
    pub staleness: String,
    /// The module `pibake:frozen`.
    pub frozen: String,
    /// The module `pibake:tool-json`.
    pub tool_json: String,
    pub baked: String,
    /// Absolute stage directory → agent-relative directory.
    pub owned: Vec<Owned>,
    pub outfile: PathBuf,
    pub write: bool,
    /// Absolute path → the text a pass rewrote it to; the bundle reads this instead.
    pub overrides: BTreeMap<String, String>,
    /// Absolute paths of overrides the bundle may leave out.
    pub optional: Vec<String>,
    /// esbuild's output format: `esm` or `cjs`.
    pub format: &'static str,
    /// Modules loaded on first use rather than at import (snapshot bundles).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lazy: Option<Lazy>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lazy {
    pub builtins: Vec<&'static str>,
    pub packages: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub text: String,
    pub id: Option<String>,
    /// Relative to the stage.
    pub file: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuildResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub metafile: Option<Metafile>,
    /// Stage-relative file → `pi`, `live` or `stage`: whose location its code was given.
    #[serde(default)]
    pub located: BTreeMap<String, String>,
    #[serde(default)]
    pub fixes: Vec<Fix>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metafile {
    pub inputs: BTreeMap<String, Input>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    pub imports: Vec<Import>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Import {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub file: String,
    pub fix: String,
}

/// Runs `build.mjs` with a config; the stand-in point for tests.
pub type Build<'a> = &'a dyn Fn(&BuildConfig) -> Result<BuildResult>;

pub fn run_node_build(config: &BuildConfig) -> Result<BuildResult> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("build.mjs");
    let config_path = dir.path().join("config.json");
    fs::write(&script, BUILD_SCRIPT)?;
    fs::write(&config_path, serde_json::to_vec(config)?)?;
    let out = Command::new("node")
        .arg(&script)
        .arg(&config_path)
        .output()
        .context("running node")?;
    if !out.status.success() {
        bail!(
            "node build.mjs failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("reading build.mjs output")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Bundled,
    BundledWithWarnings,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionReport {
    pub owner: String,
    pub entry: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub warnings: Vec<Message>,
    /// Files whose `import.meta.url`, `__dirname` or `__filename` now say `<file>: live|stage`.
    pub located: Vec<String>,
    pub fixes: Vec<Fix>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub extensions: Vec<ExtensionReport>,
    /// Warnings in code no baked extension reaches: Pi and its dependencies.
    pub pi_warnings: Vec<Message>,
    pub unbaked: Vec<Unbaked>,
    pub passes: Vec<PassReport>,
}

impl Report {
    pub fn summary(&self) -> String {
        let count = |s: Status| self.extensions.iter().filter(|e| e.status == s).count();
        let mut out = format!(
            "{} extension entry points: {} bundled, {} with warnings, {} failed\n",
            self.extensions.len(),
            count(Status::Bundled),
            count(Status::BundledWithWarnings),
            count(Status::Failed)
        );
        for e in &self.extensions {
            let detail = match e.status {
                Status::Bundled => "bundled".to_owned(),
                Status::Failed => format!(
                    "FAILED, loads through jiti: {}",
                    e.error.as_deref().unwrap_or("")
                ),
                Status::BundledWithWarnings => {
                    let mut parts = vec![];
                    if !e.warnings.is_empty() {
                        parts.push(format!("{} esbuild warnings", e.warnings.len()));
                    }
                    if !e.located.is_empty() {
                        parts.push(format!("{} files read their location", e.located.len()));
                    }
                    format!("bundled with warnings: {}", parts.join(", "))
                }
            };
            let fixes = if e.fixes.is_empty() {
                String::new()
            } else {
                format!(" ({} imports fixed)", e.fixes.len())
            };
            out.push_str(&format!("  {} {}: {detail}{fixes}\n", e.owner, e.entry));
        }
        for u in &self.unbaked {
            out.push_str(&format!(
                "  {} {}: not baked ({}), loads through jiti\n",
                u.owner, u.entry, u.reason
            ));
        }
        if !self.pi_warnings.is_empty() {
            out.push_str(&format!(
                "  Pi and its dependencies: {} esbuild warnings\n",
                self.pi_warnings.len()
            ));
        }
        for p in &self.passes {
            for c in &p.changes {
                let what = c.what.split(';').next().unwrap_or(&c.what);
                out.push_str(&format!("  pass {}: {}: {what}\n", p.pass.name(), c.file));
            }
        }
        out
    }
}

/// Files reachable from `start` through the metafile's imports, not entering Pi's packages.
pub fn reach(metafile: &Metafile, start: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start.to_owned()]);
    while let Some(file) = queue.pop_front() {
        if file.starts_with(PI_SCOPE) || !seen.insert(file.clone()) {
            continue;
        }
        for import in metafile.inputs.get(&file).map_or(&[][..], |i| &i.imports) {
            queue.push_back(import.path.clone());
        }
    }
    seen
}

/// Attributes warnings, location rewrites and fixes to the baked entries whose code reaches them.
pub fn report(
    baked: &[Baked],
    failed: &BTreeMap<String, String>,
    result: &BuildResult,
    unbaked: Vec<Unbaked>,
) -> Report {
    let empty = Metafile::default();
    let metafile = result.metafile.as_ref().unwrap_or(&empty);
    let mut reached_any = BTreeSet::new();
    let extensions = baked
        .iter()
        .map(|b| {
            if let Some(error) = failed.get(&b.key) {
                return ExtensionReport {
                    owner: b.owner.clone(),
                    entry: b.key.clone(),
                    status: Status::Failed,
                    error: Some(error.clone()),
                    warnings: vec![],
                    located: vec![],
                    fixes: vec![],
                };
            }
            let reached = reach(metafile, &b.stage_path);
            let has = |f: &Option<String>| f.as_ref().is_some_and(|f| reached.contains(f));
            let warnings: Vec<Message> = result
                .warnings
                .iter()
                .filter(|w| has(&w.file))
                .cloned()
                .collect();
            let located: Vec<String> = result
                .located
                .iter()
                .filter(|(f, _)| reached.contains(*f))
                .map(|(f, kind)| format!("{f}: {kind}"))
                .collect();
            let fixes: Vec<Fix> = result
                .fixes
                .iter()
                .filter(|f| reached.contains(&f.file))
                .cloned()
                .collect();
            reached_any.extend(reached);
            let status = if warnings.is_empty() && located.is_empty() {
                Status::Bundled
            } else {
                Status::BundledWithWarnings
            };
            ExtensionReport {
                owner: b.owner.clone(),
                entry: b.key.clone(),
                status,
                error: None,
                warnings,
                located,
                fixes,
            }
        })
        .collect();
    let pi_warnings = result
        .warnings
        .iter()
        .filter(|w| !w.file.as_ref().is_some_and(|f| reached_any.contains(f)))
        .cloned()
        .collect();
    Report {
        extensions,
        pi_warnings,
        unbaked,
        passes: vec![],
    }
}

pub struct Bundler<'a> {
    /// Holds one directory per hash.
    pub root: &'a Path,
    pub build: Build<'a>,
    pub passes: &'a passes::Options,
    pub backend: Backend,
}

impl Bundler<'_> {
    pub fn run(&self, plan: &Plan, log: &mut dyn FnMut(String)) -> Result<(Outcome, Report)> {
        let pi_dir = plan.pi_dir();
        check_pi(&pi_dir)?;
        let flavor = flavor(self.backend);
        let lazy = (self.backend == Backend::SeaSnapshot).then(|| Lazy {
            builtins: snapshot_safety::LAZY_BUILTINS.to_vec(),
            packages: snapshot_safety::LAZY_PACKAGES.to_vec(),
        });
        let all: Vec<&Baked> = plan.baked.iter().collect();
        let owned: Vec<Owned> = plan
            .owned
            .iter()
            .map(|o| Owned {
                stage: plan.stage.join(&o.stage).to_string_lossy().into_owned(),
                live: o.live.clone(),
            })
            .collect();
        let owned_json = serde_json::to_string(&owned)?;
        let baked_all = baked_module(&all);
        let stage_path = plan.stage.to_string_lossy();
        let setup = passes::Setup {
            agent_dir: plan.agent_dir.clone(),
            extension_dirs: plan.owned.iter().map(|o| o.stage.clone()).collect(),
        };
        let rewritten = passes::run(&plan.stage, self.passes, &setup)?;
        for p in &rewritten.reports {
            log(format!(
                "pass {}: {} files rewritten",
                p.pass.name(),
                p.changes.len()
            ));
            for note in &p.notes {
                log(format!("pass {}: {note}", p.pass.name()));
            }
        }
        let frozen = frozen_module(&rewritten.frozen)?;
        let overrides: BTreeMap<String, String> = rewritten
            .overrides
            .iter()
            .map(|(rel, text)| {
                (
                    plan.stage.join(rel).to_string_lossy().into_owned(),
                    text.clone(),
                )
            })
            .collect();
        let optional: Vec<String> = rewritten
            .optional
            .iter()
            .map(|rel| plan.stage.join(rel).to_string_lossy().into_owned())
            .collect();
        let passes_json = serde_json::to_string(&(self.passes, &overrides, &optional))?;
        let hash = stage::content_hash([
            ("pibake", env!("CARGO_PKG_VERSION").as_bytes()),
            ("stage", stage_path.as_bytes()),
            ("build.mjs", BUILD_SCRIPT.as_bytes()),
            ("format", flavor.format.as_bytes()),
            ("entry", flavor.entry.as_bytes()),
            (
                "launcher",
                flavor.launcher.map_or("", |(_, t)| t).as_bytes(),
            ),
            ("prologue", flavor.prologue.as_bytes()),
            ("loader", flavor.loader.map_or("", |(_, t)| t).as_bytes()),
            ("wrapper", [WRAPPER.0, WRAPPER.1].concat().as_bytes()),
            ("extract", EXTRACT.as_bytes()),
            ("lazy", serde_json::to_string(&lazy)?.as_bytes()),
            ("staleness", STALENESS.as_bytes()),
            ("frozen", frozen.as_bytes()),
            ("tool_json", TOOL_JSON.as_bytes()),
            ("baked", baked_all.as_bytes()),
            ("owned", owned_json.as_bytes()),
            ("passes", passes_json.as_bytes()),
        ]);
        let mut report_out = None;
        let outcome = stage::publish(self.root, &hash, log, |dir, log| {
            let config = |baked: &[&Baked], write: bool| BuildConfig {
                esbuild: plan.stage.join("node_modules/esbuild/lib/main.js"),
                stage: plan.stage.clone(),
                pi_dir: pi_dir.clone(),
                entry: flavor.entry.to_owned(),
                prologue: prologue(&flavor, &plan.stage),
                footer: flavor.loader.map(|_| WRAPPER.1),
                staleness: STALENESS.to_owned(),
                frozen: frozen.clone(),
                tool_json: TOOL_JSON.to_owned(),
                baked: baked_module(baked),
                owned: owned.clone(),
                outfile: dir.join(flavor.main),
                write,
                overrides: overrides.clone(),
                optional: optional.clone(),
                format: flavor.format,
                lazy: lazy.clone(),
            };
            log(format!(
                "esbuild: Pi and {} extension entry points",
                all.len()
            ));
            let mut result = (self.build)(&config(&all, true))?;
            let mut failed = BTreeMap::new();
            let mut kept = all.clone();
            if !result.errors.is_empty() {
                failed = self.isolate_failures(&all, &config, log)?;
                kept.retain(|b| !failed.contains_key(&b.key));
                log(format!("esbuild: again without {} failed", failed.len()));
                result = (self.build)(&config(&kept, true))?;
                if !result.errors.is_empty() {
                    bail!(
                        "esbuild failed even without the failing extensions:\n{}",
                        render_errors(&result.errors)
                    );
                }
            }
            if let Some(m) = &result.metafile {
                for rel in rewritten
                    .optional
                    .iter()
                    .filter(|r| !m.inputs.contains_key(*r))
                {
                    log(format!("left out of the bundle, as rewritten: {rel}"));
                }
            }
            let mut report = report(&plan.baked, &failed, &result, plan.unbaked.clone());
            report.passes = rewritten.reports.clone();
            if self.passes.passes.contains(&Pass::AsciiBundle) {
                report
                    .passes
                    .push(ascii_bundle(&dir.join(flavor.main), &flavor)?);
            }
            for (name, text) in flavor.launcher.into_iter().chain(flavor.loader) {
                fs::write(dir.join(name), text)?;
            }
            fs::write(dir.join("entry.mjs"), flavor.entry)?;
            fs::write(dir.join("baked.mjs"), baked_module(&kept))?;
            fs::write(
                dir.join(REPORT),
                format!("{}\n", serde_json::to_string_pretty(&report)?),
            )?;
            // The bundle locates itself in the stage (see its prologue); this link is for
            // `verify`, which finds the Pi package a script would import.
            symlink(plan.stage.join("node_modules"), dir.join("node_modules"))?;
            report_out = Some(report);
            Ok(())
        })?;
        let report = match report_out {
            Some(r) => r,
            None => {
                let dir = match &outcome {
                    Outcome::Cached(d) | Outcome::Built(d) => d,
                };
                serde_json::from_slice(&fs::read(dir.join(REPORT))?)
                    .with_context(|| format!("reading {}", dir.join(REPORT).display()))?
            }
        };
        Ok((outcome, report))
    }

    /// Which entries fail on their own. Pi alone must build.
    fn isolate_failures(
        &self,
        all: &[&Baked],
        config: &dyn Fn(&[&Baked], bool) -> BuildConfig,
        log: &mut dyn FnMut(String),
    ) -> Result<BTreeMap<String, String>> {
        log("esbuild failed; building Pi alone, then each extension".into());
        let pi = (self.build)(&config(&[], false))?;
        if !pi.errors.is_empty() {
            bail!(
                "esbuild failed on Pi itself:\n{}",
                render_errors(&pi.errors)
            );
        }
        let mut failed = BTreeMap::new();
        for b in all {
            let result = (self.build)(&config(&[b], false))?;
            if !result.errors.is_empty() {
                log(format!("{} {}: fails to bundle", b.owner, b.key));
                failed.insert(b.key.clone(), render_errors(&result.errors));
            }
        }
        Ok(failed)
    }
}

fn ascii_bundle(main: &Path, flavor: &Flavor) -> Result<PassReport> {
    let source_type = match flavor.format {
        "cjs" => SourceType::cjs(),
        _ => SourceType::mjs(),
    };
    let text = fs::read_to_string(main).with_context(|| format!("reading {}", main.display()))?;
    let mut changes = vec![];
    if let Some(e) =
        passes::ascii_bundle::run(&text, source_type, flavor.main).context("pass ascii_bundle")?
    {
        fs::write(main, &e.text)?;
        changes.push(passes::Change {
            file: flavor.main.to_owned(),
            what: format!(
                "escaped {} comments and {} regex literals to ASCII",
                e.comments, e.regexes
            ),
            bytes_removed: 0,
        });
    }
    Ok(PassReport {
        pass: Pass::AsciiBundle,
        changes,
        notes: vec![],
    })
}

/// `pibake:frozen`: its check, and what the passes froze.
pub fn frozen_module(frozen: &[passes::Frozen]) -> Result<String> {
    Ok(format!(
        "{FROZEN}\nexport const frozen = {};\n",
        serde_json::to_string(frozen)?
    ))
}

fn render_errors(errors: &[Message]) -> String {
    errors
        .iter()
        .map(|e| match (&e.file, e.line) {
            (Some(f), Some(l)) => format!("{f}:{l}: {}", e.text),
            _ => e.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_paths_of_the_three_install_roots() {
        assert_eq!(
            stage_rel("npm/node_modules/@a/b/index.ts").as_deref(),
            Some("node_modules/@a/b/index.ts")
        );
        assert_eq!(
            stage_rel("git/github.com/o/r/x.js").as_deref(),
            Some("git/github.com/o/r/x.js")
        );
        assert_eq!(
            stage_rel("extensions/sub/index.ts").as_deref(),
            Some("local/extensions/sub/index.ts")
        );
        for other in [
            "npm/x.ts",
            "extensionsx/a.ts",
            "skills/a.ts",
            "node_modules/a",
        ] {
            assert_eq!(stage_rel(other), None, "{other}");
        }
    }

    #[test]
    fn baked_module_maps_keys_to_stage_imports() {
        let b = Baked {
            owner: "npm:x".into(),
            key: "npm/node_modules/x/\"q\".ts".into(),
            stage_path: "node_modules/x/\"q\".ts".into(),
        };
        let text = baked_module(&[&b]);
        assert!(
            text.contains(
                r#"	"npm/node_modules/x/\"q\".ts": () => import("./node_modules/x/\"q\".ts"),"#
            ),
            "{text}"
        );
        assert!(text.contains("export function bakedLoader(path)"));
        assert!(baked_module(&[]).contains("const baked = {\n};"));
    }

    fn metafile(edges: &[(&str, &[&str])]) -> Metafile {
        Metafile {
            inputs: edges
                .iter()
                .map(|(from, to)| {
                    (
                        from.to_string(),
                        Input {
                            imports: to
                                .iter()
                                .map(|p| Import {
                                    path: p.to_string(),
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn reach_stops_at_pi() {
        let m = metafile(&[
            ("a.ts", &["b.ts", "node_modules/@earendil-works/pi-ai/x.js"]),
            ("b.ts", &["a.ts", "node_modules/dep/i.js"]),
            (
                "node_modules/@earendil-works/pi-ai/x.js",
                &["node_modules/pi-only/y.js"],
            ),
        ]);
        assert_eq!(
            reach(&m, "a.ts").into_iter().collect::<Vec<_>>(),
            ["a.ts", "b.ts", "node_modules/dep/i.js"]
        );
    }

    fn msg(file: &str) -> Message {
        Message {
            text: "w".into(),
            id: None,
            file: Some(file.into()),
            line: Some(1),
        }
    }

    #[test]
    fn report_attributes_by_reach() {
        let baked = vec![
            Baked {
                owner: "npm:a".into(),
                key: "npm/node_modules/a/i.ts".into(),
                stage_path: "node_modules/a/i.ts".into(),
            },
            Baked {
                owner: "extensions".into(),
                key: "extensions/b.ts".into(),
                stage_path: "local/extensions/b.ts".into(),
            },
            Baked {
                owner: "npm:c".into(),
                key: "npm/node_modules/c/i.ts".into(),
                stage_path: "node_modules/c/i.ts".into(),
            },
        ];
        let result = BuildResult {
            errors: vec![],
            warnings: vec![
                msg("node_modules/shared/s.js"),
                msg("node_modules/@earendil-works/pi-ai/x.js"),
            ],
            metafile: Some(metafile(&[
                ("node_modules/a/i.ts", &["node_modules/shared/s.js"]),
                ("local/extensions/b.ts", &["node_modules/shared/s.js"]),
            ])),
            located: BTreeMap::from([("local/extensions/b.ts".into(), "live".into())]),
            fixes: vec![Fix {
                file: "node_modules/a/i.ts".into(),
                fix: "#x -> y".into(),
            }],
        };
        let failed = BTreeMap::from([("npm/node_modules/c/i.ts".to_owned(), "boom".to_owned())]);
        let r = report(&baked, &failed, &result, vec![]);
        let [a, b, c] = &r.extensions[..] else {
            panic!("{r:?}")
        };
        assert_eq!(a.status, Status::BundledWithWarnings);
        assert_eq!(a.warnings, [msg("node_modules/shared/s.js")]);
        assert!(a.located.is_empty());
        assert_eq!(a.fixes.len(), 1);
        assert_eq!(b.status, Status::BundledWithWarnings);
        assert_eq!(b.located, ["local/extensions/b.ts: live"]);
        assert!(b.fixes.is_empty());
        assert_eq!(c.status, Status::Failed);
        assert_eq!(c.error.as_deref(), Some("boom"));
        assert_eq!(
            r.pi_warnings,
            [msg("node_modules/@earendil-works/pi-ai/x.js")]
        );
        let summary = r.summary();
        assert!(
            summary.starts_with("3 extension entry points: 0 bundled, 2 with warnings, 1 failed\n"),
            "{summary}"
        );
        assert!(
            summary.contains("npm:c npm/node_modules/c/i.ts: FAILED, loads through jiti: boom"),
            "{summary}"
        );
    }

    fn pi_dir(cli: &str, setup: &str, chunks: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("dist/bun")).unwrap();
        fs::create_dir_all(p.join("dist/bundle/chunks")).unwrap();
        fs::write(p.join("dist/cli.js"), cli).unwrap();
        fs::write(p.join("dist/bun/runtime-setup.js"), setup).unwrap();
        for c in chunks {
            fs::write(p.join("dist/bundle/chunks").join(c), "").unwrap();
        }
        dir
    }

    const CLI_1_3: &str = "#!/usr/bin/env node\nimport { setupCli } from \"./cli/setup.js\";\nimport { main } from \"./main.js\";\nsetupCli();\nmain(process.argv.slice(2));\n//# sourceMappingURL=cli.js.map";
    const SETUP_1_3: &str = "import { x } from \"y\";\nprocess.title = APP_NAME;\nprocess.emitWarning = (() => { });\nregisterBunOAuthFlows();\nsetBedrockProviderModule(bedrockProviderModule);\nsetEmbeddedQuickJSWasmPath(quickjsWasmPath);\n//# sourceMappingURL=runtime-setup.js.map";

    #[test]
    fn check_pi_accepts_1_3_and_names_what_moved() {
        check_pi(pi_dir(CLI_1_3, SETUP_1_3, &PI_CHUNK_FILES).path()).unwrap();
        let err = |dir: tempfile::TempDir| format!("{:#}", check_pi(dir.path()).unwrap_err());
        let cli = CLI_1_3.replace("setupCli();\n", "setupCli();\nwarmUp();\n");
        assert!(err(pi_dir(&cli, SETUP_1_3, &PI_CHUNK_FILES)).contains("dist/cli.js"));
        let setup = format!("{SETUP_1_3}\nregisterBundledThing();");
        assert!(err(pi_dir(CLI_1_3, &setup, &PI_CHUNK_FILES)).contains("registerBundledThing"));
        let setup = SETUP_1_3.replace("registerBunOAuthFlows();\n", "");
        assert!(
            err(pi_dir(CLI_1_3, &setup, &PI_CHUNK_FILES))
                .contains("no longer calls registerBunOAuthFlows")
        );
        assert!(
            err(pi_dir(CLI_1_3, SETUP_1_3, &["codemode-worker.js"]))
                .contains("image-resize-worker.js")
        );
    }
}
