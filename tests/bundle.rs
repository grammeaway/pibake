mod common;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use common::{bundled_launcher, e2e_enabled, pibake, write};
use pibake::bundle::{
    BuildConfig, BuildResult, Bundler, Import, Input, LAUNCHER, Message, Metafile, Plan, REPORT,
    Status,
};
use pibake::config::Backend;
use pibake::inventory;
use pibake::lock::PI_PACKAGE;
use pibake::passes::{Options, Pass};
use pibake::stage::{MARKER, Outcome};

const PI_CLI: &str = "#!/usr/bin/env node\nimport { setupCli } from \"./cli/setup.js\";\nimport { main } from \"./main.js\";\nsetupCli();\nmain(process.argv.slice(2));\n//# sourceMappingURL=cli.js.map\n";
const PI_SETUP: &str = "registerBunOAuthFlows();\nsetBedrockProviderModule(bedrockProviderModule);\nsetEmbeddedQuickJSWasmPath(quickjsWasmPath);\n";

/// An agent dir with one npm package and two local extensions, and a stage holding them.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let f = Self { root };
        f.write("agent/settings.json", r#"{"packages": ["npm:pkg"]}"#);
        let manifest = r#"{"name":"pkg","version":"1.0.0","pi":{"extensions":["./index.ts"]}}"#;
        for (rel, content) in [
            ("npm/node_modules/pkg/package.json", manifest),
            ("npm/node_modules/pkg/index.ts", "export default () => {};"),
            ("extensions/a.ts", "export default () => {};"),
            ("extensions/b.js", "export default () => {};"),
        ] {
            f.write(&format!("agent/{rel}"), content);
            let staged = pibake::bundle::stage_rel(rel).unwrap();
            f.write(&format!("stage/{staged}"), content);
        }
        let pi = format!("stage/node_modules/{PI_PACKAGE}");
        f.write(&format!("{pi}/dist/cli.js"), PI_CLI);
        f.write(&format!("{pi}/dist/bun/runtime-setup.js"), PI_SETUP);
        f.write(&format!("{pi}/dist/bundle/chunks/codemode-worker.js"), "");
        f.write(
            &format!("{pi}/dist/bundle/chunks/image-resize-worker.js"),
            "",
        );
        f
    }

    fn write(&self, rel: &str, content: &str) {
        write(&self.path(rel), content);
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn plan(&self) -> Result<Plan> {
        let inv = inventory::read(&self.path("agent"), self.root.path())?;
        Plan::new(&inv, &self.path("stage"))
    }

    fn bundle(&self, fake: &FakeBuild) -> Result<(Outcome, pibake::bundle::Report)> {
        let build = |c: &BuildConfig| fake.run(c);
        Bundler {
            root: &self.path("bundles"),
            build: &build,
            passes: &Options::none(),
            backend: Backend::SeaCodecache,
        }
        .run(&self.plan()?, &mut |_| {})
    }

    fn bundle_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path("bundles"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

/// Records each build's baked entries; fails builds whose baked module imports `failing`.
struct FakeBuild {
    failing: Vec<&'static str>,
    calls: RefCell<Vec<(Vec<String>, bool)>>,
}

impl FakeBuild {
    fn new(failing: &[&'static str]) -> Self {
        Self {
            failing: failing.to_vec(),
            calls: RefCell::new(vec![]),
        }
    }

    fn run(&self, c: &BuildConfig) -> Result<BuildResult> {
        let baked: Vec<String> = c
            .baked
            .lines()
            .filter_map(|l| Some(l.split("import(\"./").nth(1)?.split('"').next()?.to_owned()))
            .collect();
        self.calls.borrow_mut().push((baked.clone(), c.write));
        assert!(c.esbuild.ends_with("node_modules/esbuild/lib/main.js"));
        assert_eq!(c.pi_dir, c.stage.join("node_modules").join(PI_PACKAGE));
        let errors: Vec<Message> = baked
            .iter()
            .filter(|b| self.failing.iter().any(|f| b.contains(f)))
            .map(|b| Message {
                text: "Could not resolve".into(),
                id: None,
                file: Some(b.clone()),
                line: Some(1),
            })
            .collect();
        if c.write && errors.is_empty() {
            fs::write(&c.outfile, "// bundle").unwrap();
        }
        let inputs = baked
            .iter()
            .map(|b| {
                (
                    b.clone(),
                    Input {
                        imports: vec![Import {
                            path: "node_modules/dep/i.js".into(),
                        }],
                    },
                )
            })
            .collect();
        Ok(BuildResult {
            warnings: vec![Message {
                text: "dynamic import".into(),
                id: Some("unsupported-dynamic-import".into()),
                file: Some("node_modules/dep/i.js".into()),
                line: Some(3),
            }],
            metafile: errors.is_empty().then_some(Metafile { inputs }),
            errors,
            ..Default::default()
        })
    }
}

fn dir(outcome: &Outcome) -> &Path {
    match outcome {
        Outcome::Cached(d) | Outcome::Built(d) => d,
    }
}

#[test]
fn plans_every_entry_point_against_the_stage() {
    let f = Fixture::new();
    let plan = f.plan().unwrap();
    let keys: Vec<(&str, &str, &str)> = plan
        .baked
        .iter()
        .map(|b| (b.owner.as_str(), b.key.as_str(), b.stage_path.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            (
                "npm:pkg",
                "npm/node_modules/pkg/index.ts",
                "node_modules/pkg/index.ts"
            ),
            ("extensions", "extensions/a.ts", "local/extensions/a.ts"),
            ("extensions", "extensions/b.js", "local/extensions/b.js"),
        ]
    );
    let owned: Vec<(&str, &str)> = plan
        .owned
        .iter()
        .map(|o| (o.stage.as_str(), o.live.as_str()))
        .collect();
    assert_eq!(
        owned,
        [
            ("local/extensions", "extensions"),
            ("node_modules/pkg", "npm/node_modules/pkg"),
        ]
    );

    fs::remove_file(f.path("stage/local/extensions/b.js")).unwrap();
    let err = format!("{:#}", f.plan().err().unwrap());
    assert!(
        err.contains("local/extensions/b.js is not in the stage"),
        "{err}"
    );
}

#[test]
fn bundles_then_reuses() {
    let f = Fixture::new();
    let fake = FakeBuild::new(&[]);
    let (outcome, report) = f.bundle(&fake).unwrap();
    assert!(matches!(outcome, Outcome::Built(_)));
    let d = dir(&outcome).to_path_buf();
    for name in [
        LAUNCHER,
        "main.mjs",
        REPORT,
        MARKER,
        "entry.mjs",
        "baked.mjs",
    ] {
        assert!(d.join(name).is_file(), "{name}");
    }
    assert_eq!(
        fs::read_link(d.join("node_modules")).unwrap(),
        f.path("stage/node_modules")
    );
    assert_eq!(fake.calls.borrow().len(), 1);
    assert_eq!(fake.calls.borrow()[0].0.len(), 3);
    assert!(
        report
            .extensions
            .iter()
            .all(|e| e.status == Status::BundledWithWarnings && e.warnings.len() == 1),
        "{report:?}"
    );
    assert!(report.pi_warnings.is_empty());

    let again = FakeBuild::new(&[]);
    let (outcome2, report2) = f.bundle(&again).unwrap();
    assert!(matches!(outcome2, Outcome::Cached(_)));
    assert_eq!(dir(&outcome2), d);
    assert!(again.calls.borrow().is_empty());
    assert_eq!(report2, report);
}

#[test]
fn failing_extensions_are_left_to_jiti() {
    let f = Fixture::new();
    let fake = FakeBuild::new(&["b.js"]);
    let (outcome, report) = f.bundle(&fake).unwrap();
    let calls = fake.calls.borrow();
    let all = vec![
        "node_modules/pkg/index.ts".to_owned(),
        "local/extensions/a.ts".to_owned(),
        "local/extensions/b.js".to_owned(),
    ];
    let expected: Vec<(Vec<String>, bool)> = vec![
        (all.clone(), true),
        (vec![], false),
        (vec![all[0].clone()], false),
        (vec![all[1].clone()], false),
        (vec![all[2].clone()], false),
        (all[..2].to_vec(), true),
    ];
    assert_eq!(*calls, expected);
    let statuses: Vec<&Status> = report.extensions.iter().map(|e| &e.status).collect();
    assert_eq!(
        statuses,
        [
            &Status::BundledWithWarnings,
            &Status::BundledWithWarnings,
            &Status::Failed
        ]
    );
    assert!(
        report.extensions[2]
            .error
            .as_deref()
            .unwrap()
            .contains("local/extensions/b.js:1: Could not resolve")
    );
    // The baked module the bundle keeps leaves b.js out, so Pi loads it from disk.
    let baked = fs::read_to_string(dir(&outcome).join("baked.mjs")).unwrap();
    assert!(baked.contains("local/extensions/a.ts"), "{baked}");
    assert!(!baked.contains("local/extensions/b.js"), "{baked}");
}

#[test]
fn pi_failing_alone_is_an_error_and_leaves_nothing() {
    let f = Fixture::new();
    // Every build fails: an error in Pi's own code shows in every baked set, the empty one included.
    let build = |_: &BuildConfig| -> Result<BuildResult> {
        Ok(BuildResult {
            errors: vec![Message {
                text: "pi broke".into(),
                id: None,
                file: Some("node_modules/@earendil-works/pi-coding-agent/dist/main.js".into()),
                line: Some(1),
            }],
            ..Default::default()
        })
    };
    let err = Bundler {
        root: &f.path("bundles"),
        build: &build,
        passes: &Options::none(),
        backend: Backend::SeaCodecache,
    }
    .run(&f.plan().unwrap(), &mut |_| {})
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("esbuild failed on Pi itself"),
        "{err:#}"
    );
    assert!(f.bundle_entries().is_empty(), "{:?}", f.bundle_entries());
}

#[test]
fn a_changed_pi_entry_fails_loudly() {
    let f = Fixture::new();
    f.write(
        &format!("stage/node_modules/{PI_PACKAGE}/dist/cli.js"),
        "import { main } from \"./main.js\";\nmain(process.argv.slice(3));\n",
    );
    let err = f.bundle(&FakeBuild::new(&[])).unwrap_err();
    assert!(format!("{err:#}").contains("dist/cli.js"), "{err:#}");
}

#[test]
fn changed_inputs_get_their_own_bundle() {
    let f = Fixture::new();
    let (first, _) = f.bundle(&FakeBuild::new(&[])).unwrap();
    // Another local extension: a different baked module, so a different hash.
    f.write("agent/extensions/c.ts", "export default () => {};");
    f.write("stage/local/extensions/c.ts", "export default () => {};");
    let (second, _) = f.bundle(&FakeBuild::new(&[])).unwrap();
    assert!(matches!(second, Outcome::Built(_)));
    assert_ne!(dir(&first), dir(&second));
    assert_eq!(f.bundle_entries().len(), 2);
}

fn trim(langs: &[&str]) -> Options {
    Options {
        passes: vec![Pass::TrimHighlight],
        highlight_languages: langs.iter().map(|s| s.to_string()).collect(),
        apis: None,
        snapshot: false,
    }
}

#[test]
fn pass_rewrites_reach_the_build_and_key_the_bundle() {
    let f = Fixture::new();
    common::write_highlight_stage(&f.path("stage"), &["rust", "go", "xml"], &["rust", "go"]);
    let seen = RefCell::new(vec![]);
    let build = |c: &BuildConfig| {
        seen.borrow_mut().push(c.overrides.clone());
        FakeBuild::new(&[]).run(c)
    };
    let run = |options: &Options| {
        Bundler {
            root: &f.path("bundles"),
            build: &build,
            passes: options,
            backend: Backend::SeaCodecache,
        }
        .run(&f.plan().unwrap(), &mut |_| {})
    };
    let (first, report) = run(&trim(&["rust"])).unwrap();
    let overrides = seen.borrow()[0].clone();
    let index = f.path("stage").join(common::HIGHLIGHT_INDEX);
    let syntax = f.path("stage").join(common::SYNTAX_HIGHLIGHT);
    assert_eq!(
        overrides.keys().cloned().collect::<Vec<_>>(),
        [syntax, index].map(|p| p.to_string_lossy().into_owned())
    );
    assert_eq!(report.passes.len(), 1);
    assert_eq!(report.passes[0].changes.len(), 2);
    assert!(
        report.summary().contains(&format!(
            "  pass trim_highlight: {}: kept 1 of 3 languages\n",
            common::HIGHLIGHT_INDEX
        )),
        "{}",
        report.summary()
    );
    let stored: pibake::bundle::Report =
        serde_json::from_slice(&fs::read(dir(&first).join(REPORT)).unwrap()).unwrap();
    assert_eq!(stored, report);

    // Other options are another bundle; the same ones a cache hit.
    let (second, _) = run(&trim(&["rust", "xml"])).unwrap();
    assert_ne!(dir(&first), dir(&second));
    let (third, _) = run(&Options::none()).unwrap();
    assert!(seen.borrow().last().unwrap().is_empty());
    assert_ne!(dir(&third), dir(&first));
    let calls = seen.borrow().len();
    assert!(matches!(
        run(&trim(&["rust"])).unwrap().0,
        Outcome::Cached(_)
    ));
    assert_eq!(seen.borrow().len(), calls);

    // A pass that fails stops the bundle before esbuild runs.
    let err = run(&trim(&["cobol"])).unwrap_err();
    assert!(format!("{err:#}").contains("cobol"), "{err:#}");
    assert_eq!(seen.borrow().len(), calls);
    assert_eq!(f.bundle_entries().len(), 3);
}

const ONCE_TS: &str = r#"const g = globalThis as any;
g.__pibake_module = (g.__pibake_module ?? 0) + 1;
const loads = g.__pibake_module;
export default function (pi: any) {
	g.__pibake_factory = (g.__pibake_factory ?? 0) + 1;
	pi.registerCommand(`once-${loads}-${g.__pibake_factory}`, { description: "counts loads", handler: async () => {} });
}
"#;

/// Real `pi`, `node` and npm (network): a fixture setup with local extensions that exercise the
/// bundle's fixes is locked, staged and bundled, then compared with stock Pi on the same setup.
#[test]
fn e2e_bundle_a_fixture_setup() {
    use pibake::fake_provider::FakeProvider;
    use pibake::harness::PiCommand;
    use pibake::snapshot;

    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let files = [
        ("settings.json", "{}"),
        ("extensions/once.ts", ONCE_TS),
        (
            "extensions/located.js",
            "import { fileURLToPath } from \"node:url\";\nexport default function (pi) {\n\tpi.registerCommand(\"located\", { description: fileURLToPath(import.meta.url), handler: async () => {} });\n}\n",
        ),
        (
            "extensions/pkg/package.json",
            r##"{"name": "pkg", "pi": {"extensions": ["./index.ts"]}, "imports": {"#lib/*": "./lib/*"}}"##,
        ),
        (
            "extensions/pkg/index.ts",
            "import { name } from \"#lib/name\";\nexport default function (pi: any) {\n\tpi.registerCommand(name, { description: \"package imports\", handler: async () => {} });\n}\n",
        ),
        (
            "extensions/pkg/lib/name.ts",
            "export const name = \"from-imports\";\n",
        ),
        (
            "extensions/compiled.js",
            "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.default = function (pi) {\n\tpi.registerCommand(\"from-cjs\", { description: \"compiled CommonJS\", handler: async () => {} });\n};\n",
        ),
        (
            "extensions/broken.ts",
            "export default function (pi: any) {\n\tpi.registerCommand(\"from-broken\", { description: \"esbuild cannot resolve its import, jiti loads it\", handler: async () => { await import(\"pibake-no-such-module\"); } });\n}\n",
        ),
    ];
    for (rel, content) in files {
        write(&agent.join(rel), content);
    }
    let project = root.path().join("project");
    write(
        &project.join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", agent.display()),
    );
    pibake(&project, &["lock"], true);
    let stdout = pibake(&project, &["bundle"], true);
    let launcher = bundled_launcher(&stdout);
    let report: pibake::bundle::Report =
        serde_json::from_slice(&fs::read(launcher.with_file_name(REPORT)).unwrap()).unwrap();
    let status = |entry: &str| {
        report
            .extensions
            .iter()
            .find(|e| e.entry == entry)
            .unwrap_or_else(|| panic!("{entry} not in {report:?}"))
    };
    assert_eq!(status("extensions/once.ts").status, Status::Bundled);
    assert_eq!(
        status("extensions/broken.ts").status,
        Status::Failed,
        "{stdout}"
    );
    let located = status("extensions/located.js");
    assert_eq!(located.located, ["local/extensions/located.js: live"]);
    assert_eq!(status("extensions/pkg/index.ts").fixes.len(), 1, "{stdout}");

    // A second bundle is a cache hit.
    let again = pibake(&project, &["bundle"], true);
    assert!(again.contains("bundle up to date: "), "{again}");
    assert_eq!(bundled_launcher(&again), launcher);

    let bundle_cmd = format!("node {}", launcher.display());
    let verify = pibake(
        &project,
        &["verify", "--pi", &bundle_cmd, "--against", "pi"],
        true,
    );
    assert!(verify.starts_with("same: "), "{verify}");

    let home = pibake::paths::home_dir().unwrap();
    let provider = FakeProvider::start().unwrap();
    let names = |pi: &str| -> Vec<String> {
        let snap =
            snapshot::capture(&PiCommand::parse(pi).unwrap(), &agent, &home, &provider).unwrap();
        let mut names: Vec<String> = snap
            .commands
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_owned))
            .filter(|n| n.starts_with("once") || n.starts_with("from-"))
            .collect();
        names.sort();
        names
    };
    // Each factory runs once, its module evaluated once, as in stock Pi.
    let baked = ["from-broken", "from-cjs", "from-imports", "once-1-1"];
    assert_eq!(names(&bundle_cmd), baked);
    assert_eq!(names("pi"), baked);
    // The bundle answers from what it baked, not from disk; what it did not bake loads from
    // disk through jiti.
    write(
        &agent.join("extensions/once.ts"),
        &ONCE_TS.replace("`once-${loads}-${g.__pibake_factory}`", "\"from-disk\""),
    );
    write(
        &agent.join("extensions/late.ts"),
        "export default function (pi: any) {\n\tpi.registerCommand(\"from-late\", { description: \"added after bundling\", handler: async () => {} });\n}\n",
    );
    assert_eq!(
        names(&bundle_cmd),
        [
            "from-broken",
            "from-cjs",
            "from-imports",
            "from-late",
            "once-1-1"
        ]
    );
    assert_eq!(
        names("pi"),
        [
            "from-broken",
            "from-cjs",
            "from-disk",
            "from-imports",
            "from-late"
        ]
    );
}

/// Real `pi`, `node`, npm and git (network): locks, stages and bundles the live setup into a
/// scratch project and verifies the bundle against stock Pi. Reads `~/.pi/agent`; asserts it is
/// unchanged.
#[test]
fn e2e_bundle_the_live_setup() {
    if !e2e_enabled() {
        return;
    }
    let home = pibake::paths::home_dir().unwrap();
    let live = home.join(".pi/agent");
    let inv = inventory::read(&live, &home).unwrap();
    let watched: Vec<String> = inv
        .config
        .iter()
        .chain(&inv.install_roots)
        .cloned()
        .collect();
    let before = common::fingerprint(&live, &watched);

    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", live.display()),
    );
    pibake(root.path(), &["lock"], true);
    let stdout = pibake(root.path(), &["bundle"], true);
    assert!(stdout.contains(" 0 failed\n"), "{stdout}");
    let launcher = bundled_launcher(&stdout);
    let verify = pibake(
        root.path(),
        &[
            "verify",
            "--pi",
            &format!("node {}", launcher.display()),
            "--against",
            "pi",
        ],
        true,
    );
    assert!(verify.starts_with("same: "), "{verify}");
    assert_eq!(
        common::fingerprint(&live, &watched),
        before,
        "~/.pi/agent changed"
    );
}

/// Calls Pi's own code-block highlighter (`getMarkdownTheme().highlightCode`, what markdown
/// fences render through) and reports per language whether the output differs from plain text.
/// Where it can, it first loads highlight.js's full language set, as interactive mode does
/// after its first render.
const RENDER_TS: &str = r#"import { getMarkdownTheme, initTheme } from "@earendil-works/pi-coding-agent";

const samples: Record<string, string> = {
	rust: "fn main() { let x = 1; }",
	python: "def f():\n    return 1",
	yaml: "key: value\nlist:\n  - 1",
	fortran: "program hello\n  print *, \"hi\"\nend program hello",
};

export default async function (pi: any) {
	let lazy = "unavailable";
	try {
		await import("highlight.js/lib/index.js");
		lazy = "loaded";
	} catch {}
	initTheme();
	const md = getMarkdownTheme();
	const result: Record<string, string> = { lazy };
	for (const [lang, code] of Object.entries(samples)) {
		const out = md.highlightCode!(code, lang).join("\n");
		result[lang] = out === md.highlightCode!(code, "pibake-unknown").join("\n") ? "plain" : "highlighted";
	}
	pi.registerCommand("render-check", { description: JSON.stringify(result), handler: async () => {} });
}
"#;

/// Real `pi`, `node` and npm (network): a setup trimmed to rust and yaml highlights those, and
/// renders python (startup) and fortran (loaded later) as plain text, without errors.
#[test]
fn e2e_trimmed_highlighting_renders() {
    use pibake::fake_provider::FakeProvider;
    use pibake::harness::PiCommand;
    use pibake::snapshot;

    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    write(&agent.join("settings.json"), "{}");
    write(&agent.join("extensions/render.ts"), RENDER_TS);
    let project = root.path().join("project");
    let source = format!("[source]\nagent_dir = \"{}\"\n", agent.display());
    write(
        &project.join("pibake.toml"),
        &format!("{source}[trim]\nhighlight_languages = [\"rust\", \"yaml\"]\n"),
    );
    write(
        &project.join("off.toml"),
        &format!("{source}[build]\npasses = []\n"),
    );
    pibake(&project, &["lock"], true);
    let stdout = pibake(&project, &["bundle"], true);
    assert!(
        stdout.contains(
            "pass trim_highlight: node_modules/highlight.js/lib/index.js: kept 2 of 191 languages\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("startup languages: "),
        "python and the other startup languages are dropped: {stdout}"
    );
    let trimmed = bundled_launcher(&stdout);
    let off = bundled_launcher(&pibake(&project, &["--config", "off.toml", "bundle"], true));
    let grammars = |launcher: &Path| -> Vec<String> {
        let main = fs::read_to_string(launcher.with_file_name("main.mjs")).unwrap();
        ["rust", "yaml", "python", "fortran"]
            .into_iter()
            .filter(|l| {
                main.contains(&format!(
                    "// node_modules/highlight.js/lib/languages/{l}.js\n"
                ))
            })
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(grammars(&trimmed), ["rust", "yaml"]);
    assert_eq!(grammars(&off), ["rust", "yaml", "python", "fortran"]);
    let size = |l: &Path| fs::metadata(l.with_file_name("main.mjs")).unwrap().len();
    assert!(
        size(&trimmed) + 1_000_000 < size(&off),
        "{} vs {}",
        size(&trimmed),
        size(&off)
    );

    let home = pibake::paths::home_dir().unwrap();
    let provider = FakeProvider::start().unwrap();
    let render = |pi: &str| -> serde_json::Value {
        let snap =
            snapshot::capture(&PiCommand::parse(pi).unwrap(), &agent, &home, &provider).unwrap();
        let cmd = snap
            .commands
            .iter()
            .find(|c| c["name"] == "render-check")
            .unwrap_or_else(|| panic!("no render-check in {:?}", snap.commands));
        serde_json::from_str(cmd["description"].as_str().unwrap()).unwrap()
    };
    let expect = |lazy: &str, highlighted: &[&str]| {
        let mut v = serde_json::json!({ "lazy": lazy });
        for l in ["rust", "python", "yaml", "fortran"] {
            v[l] = (if highlighted.contains(&l) {
                "highlighted"
            } else {
                "plain"
            })
            .into();
        }
        v
    };
    assert_eq!(
        render(&format!("node {}", trimmed.display())),
        expect("loaded", &["rust", "yaml"])
    );
    assert_eq!(
        render(&format!("node {}", off.display())),
        expect("loaded", &["rust", "python", "yaml", "fortran"])
    );
    // Stock Pi: its startup languages only; the extension cannot reach highlight.js from disk.
    assert_eq!(render("pi"), expect("unavailable", &["rust", "python"]));
}
