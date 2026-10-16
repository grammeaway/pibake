//! l9: `trim_apis` on fixture stages in pi-ai 1.3.0's shapes, the API set it derives from the
//! setup, and the startup check of what it froze (`bundle/frozen.mjs`). Tests running the JS
//! need `node`, so they sit behind PIBAKE_E2E.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{BEDROCK, BEDROCK_PROVIDER, PI_AI, api_file, api_js, e2e_enabled, write};
use hegel::TestCase;
use hegel::generators as gs;
use pibake::bundle;
use pibake::passes::{self, Frozen, Options, Output, Pass, Setup};

const APIS: [&str; 5] = [
    "anthropic-messages",
    BEDROCK,
    "google-generative-ai",
    "openai-completions",
    "openai-responses",
];
const CATALOG: [(&str, &[&str]); 5] = [
    ("amazon-bedrock", &[BEDROCK]),
    ("anthropic", &["anthropic-messages"]),
    (
        "github-copilot",
        &[
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
        ],
    ),
    ("google", &["google-generative-ai"]),
    ("typesafe", &[]),
];
const EXTENSION: &str = "node_modules/ext-a";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        common::write_apis_stage(&root.path().join("stage"), &APIS, &CATALOG);
        fs::create_dir_all(root.path().join("agent")).unwrap();
        let f = Self { root };
        f.write_stage(
            &format!("{EXTENSION}/index.ts"),
            "export default () => {};\n",
        );
        f
    }

    fn stage(&self) -> PathBuf {
        self.root.path().join("stage")
    }

    fn agent(&self) -> PathBuf {
        self.root.path().join("agent")
    }

    fn write_stage(&self, rel: &str, text: &str) {
        write(&self.stage().join(rel), text);
    }

    fn write_agent(&self, rel: &str, text: &str) {
        write(&self.agent().join(rel), text);
    }

    fn run(&self, apis: Option<&[&str]>) -> anyhow::Result<Output> {
        passes::run(
            &self.stage(),
            &Options {
                passes: vec![Pass::TrimApis],
                highlight_languages: vec![],
                apis: apis.map(|a| a.iter().map(|s| s.to_string()).collect()),
            },
            &Setup {
                agent_dir: self.agent(),
                extension_dirs: vec![EXTENSION.to_owned()],
            },
        )
    }

    fn err(&self, apis: Option<&[&str]>) -> String {
        format!("{:#}", self.run(apis).unwrap_err())
    }

    /// The APIs left in, read back from what the pass wrote.
    fn kept(&self, out: &Output) -> Vec<&'static str> {
        APIS.iter()
            .copied()
            .filter(|a| !out.overrides.contains_key(&api_file(a)))
            .collect()
    }

    fn notes(out: &Output) -> Vec<String> {
        out.reports[0].notes.clone()
    }
}

fn catalog_map() -> BTreeMap<String, Vec<String>> {
    CATALOG
        .iter()
        .map(|(p, a)| (p.to_string(), a.iter().map(|s| s.to_string()).collect()))
        .collect()
}

#[test]
fn stubs_exactly_the_apis_left_out() {
    let f = Fixture::new();
    let out = f
        .run(Some(&["openai-completions", "anthropic-messages"]))
        .unwrap();
    let left_out = [BEDROCK, "google-generative-ai", "openai-responses"];
    assert_eq!(out.overrides.keys().cloned().collect::<Vec<_>>(), {
        let mut files: Vec<String> = left_out.iter().map(|a| api_file(a)).collect();
        files.sort();
        files
    });
    for api in left_out {
        assert_eq!(out.overrides[&api_file(api)], api_js(api, true), "{api}");
    }
    let report = &out.reports[0];
    assert_eq!(report.pass, Pass::TrimApis);
    let whats: Vec<(&str, &str)> = report
        .changes
        .iter()
        .map(|c| (c.file.as_str(), c.what.as_str()))
        .collect();
    assert_eq!(
        whats,
        [
            (BEDROCK_PROVIDER, "left out the bedrock-converse-stream API"),
            (
                &*api_file("google-generative-ai"),
                "left out the google-generative-ai API"
            ),
            (
                &*api_file("openai-responses"),
                "left out the openai-responses API"
            ),
        ]
    );
    assert_eq!(
        report.notes,
        ["keeps anthropic-messages, openai-completions (from [trim] apis)"]
    );
    assert_eq!(
        out.frozen,
        [Frozen::Apis {
            setting: "[trim] apis".into(),
            left_out: left_out.map(String::from).to_vec(),
            providers: catalog_map(),
        }]
    );
}

#[test]
fn keeping_everything_rewrites_nothing_and_freezes_nothing_out() {
    let f = Fixture::new();
    let out = f.run(Some(&APIS)).unwrap();
    assert!(out.overrides.is_empty(), "{:?}", out.overrides.keys());
    assert!(out.reports[0].changes.is_empty());
    let [Frozen::Apis { left_out, .. }] = &out.frozen[..] else {
        panic!("{:?}", out.frozen)
    };
    assert!(left_out.is_empty());
}

#[test]
fn running_on_its_own_output_changes_nothing() {
    let f = Fixture::new();
    let keep: &[&str] = &["anthropic-messages"];
    let out = f.run(Some(keep)).unwrap();
    for (rel, text) in &out.overrides {
        f.write_stage(rel, text);
    }
    let again = f.run(Some(keep)).unwrap();
    assert!(again.overrides.is_empty(), "{:?}", again.overrides.keys());
    assert_eq!(again.frozen, out.frozen);

    // An API stubbed already cannot be kept again: the pass only ever sees stock sources.
    let err = f.err(Some(&["anthropic-messages", "openai-responses"]));
    assert!(
        err.contains("the openai-responses API is already stubbed out, but [trim] apis keeps it"),
        "{err}"
    );
    let err = f.err(Some(&["anthropic-messages", BEDROCK]));
    assert!(
        err.contains("the bedrock-converse-stream API is already stubbed out"),
        "{err}"
    );
}

#[test]
fn unknown_apis_are_named_with_the_known_ones() {
    let f = Fixture::new();
    let err = f.err(Some(&["anthropic-messages", "cobol-rpc"]));
    assert!(
        err.contains("[trim] apis names cobol-rpc, which pi-ai's BUILTIN_APIS does not register; it knows: anthropic-messages, bedrock-converse-stream, google-generative-ai, openai-completions, openai-responses"),
        "{err}"
    );
}

#[test]
fn unknown_shapes_fail_naming_the_file_and_the_expected_shape() {
    let compat = format!("{PI_AI}/dist/compat.js");
    let cases: Vec<(String, String, &str)> = vec![
        (
            compat.clone(),
            "export const x = 1;\n".into(),
            "dist/compat.js: `const BUILTIN_APIS = [ … ]` not found",
        ),
        (
            compat.clone(),
            "const BUILTIN_APIS = new Map();\n".into(),
            "`BUILTIN_APIS` is no longer an array literal",
        ),
        (
            compat.clone(),
            common::compat_js(&APIS).replace(
                "[\"openai-responses\", openai_responsesApi()]",
                "[\"openai-responses\", openai_responsesApi]",
            ),
            "expected `[\"<api>\", <factory>()]` in `BUILTIN_APIS`; found `[\"openai-responses\", openai_responsesApi]`",
        ),
        (
            compat.clone(),
            common::compat_js(&APIS).replace(
                "import { openai_responsesApi } from \"./api/openai-responses.lazy.js\";\n",
                "",
            ),
            "`BUILTIN_APIS` calls `openai_responsesApi`, which is not imported from ./api/<name>.lazy.js",
        ),
        (
            api_file("google-generative-ai"),
            "import { lazyApi } from \"./lazy.js\";\nconst load = () => import(\"./google-generative-ai.js\");\nexport const google_generative_aiApi = () => lazyApi(load);\n".into(),
            "expected `export const google_generative_aiApi = () => lazyApi(() => import(\"./<file>.js\"));`; found `export const google_generative_aiApi = () => lazyApi(load);`",
        ),
        (
            api_file("google-generative-ai"),
            "export const somethingElse = 1;\n".into(),
            "`export const google_generative_aiApi` not found",
        ),
        (
            api_file("google-generative-ai"),
            "import { lazyApi } from \"./lazy.js\";\nexport const google_generative_aiApi = () => lazyApi(() => loadGoogle());\n".into(),
            "found `() => loadGoogle()`",
        ),
        (
            BEDROCK_PROVIDER.into(),
            "import { stream } from \"./api/bedrock-converse-stream.js\";\nexport const bedrockProviderModule = { stream };\n".into(),
            "expected only `import { stream, streamSimple } from \"./api/bedrock-converse-stream.js\";`; found `import { stream } from",
        ),
        (
            BEDROCK_PROVIDER.into(),
            "export const bedrockProviderModule = {};\n".into(),
            "dist/bedrock-provider.js: expected `import { stream, streamSimple }",
        ),
    ];
    for (rel, text, expected) in cases {
        let f = Fixture::new();
        f.write_stage(&rel, &text);
        let err = f.err(Some(&["anthropic-messages"]));
        assert!(err.contains(&rel), "{rel}: {err}");
        assert!(
            err.contains(expected),
            "{rel}: expected {expected:?} in:\n{err}"
        );
    }

    let f = Fixture::new();
    for (p, _) in CATALOG {
        fs::remove_file(
            f.stage()
                .join(format!("{PI_AI}/dist/providers/data/{p}.json")),
        )
        .unwrap();
    }
    let err = f.err(Some(&["anthropic-messages"]));
    assert!(err.contains("pi-ai's catalog layout changed"), "{err}");
}

#[test]
fn derives_the_default_providers_apis() {
    let f = Fixture::new();
    f.write_agent(
        "settings.json",
        r#"{"defaultProvider": "anthropic", "theme": "dark"}"#,
    );
    let out = f.run(None).unwrap();
    assert_eq!(f.kept(&out), ["anthropic-messages"]);
    assert_eq!(
        Fixture::notes(&out),
        ["keeps anthropic-messages: settings.json defaultProvider anthropic"]
    );

    f.write_agent("settings.json", r#"{"defaultProvider": "github-copilot"}"#);
    let out = f.run(None).unwrap();
    assert_eq!(
        f.kept(&out),
        [
            "anthropic-messages",
            "openai-completions",
            "openai-responses"
        ]
    );
}

#[test]
fn keeps_every_api_when_the_setup_does_not_say() {
    for (settings, why) in [
        (
            None,
            "settings.json names no defaultProvider, so Pi picks one at startup",
        ),
        (
            Some(r#"{"theme": "dark"}"#),
            "settings.json names no defaultProvider",
        ),
        (
            Some(r#"{"defaultProvider": "sakana-fugu"}"#),
            "settings.json defaultProvider sakana-fugu is neither built in nor in models.json",
        ),
        (
            Some(
                r#"{"defaultProvider": "anthropic", "enabledModels": ["anthropic/claude-x", "claude-*"]}"#,
            ),
            "settings.json enabledModels \"claude-*\" names no known provider, so it may match any",
        ),
    ] {
        let f = Fixture::new();
        if let Some(s) = settings {
            f.write_agent("settings.json", s);
        }
        let out = f.run(None).unwrap();
        assert!(out.overrides.is_empty(), "{settings:?}");
        let notes = Fixture::notes(&out);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("keeps every API: "), "{notes:?}");
        assert!(notes[0].contains(why), "{notes:?}");
    }
}

#[test]
fn derives_from_enabled_models_models_json_and_extension_sources() {
    let f = Fixture::new();
    f.write_agent(
        "settings.json",
        r#"{"defaultProvider": "anthropic", "enabledModels": ["google/gemini-x:high"]}"#,
    );
    let out = f.run(None).unwrap();
    assert_eq!(f.kept(&out), ["anthropic-messages", "google-generative-ai"]);

    f.write_agent("settings.json", r#"{"defaultProvider": "anthropic"}"#);
    // A custom provider (provider and model `api`), and a built-in one overridden without `api`.
    f.write_agent(
        "models.json",
        r#"{"providers": {
            "local": {"api": "openai-completions", "baseUrl": "http://x", "apiKey": "k",
                      "models": [{"id": "a"}, {"id": "b", "api": "openai-responses"}, {"id": "c", "api": "not-an-api"}]},
            "amazon-bedrock": {"baseUrl": "http://y"}
        }}"#,
    );
    let out = f.run(None).unwrap();
    assert_eq!(
        f.kept(&out),
        [
            "anthropic-messages",
            BEDROCK,
            "openai-completions",
            "openai-responses"
        ]
    );
    assert_eq!(
        Fixture::notes(&out),
        [
            "keeps anthropic-messages: settings.json defaultProvider anthropic",
            "keeps bedrock-converse-stream: models.json provider amazon-bedrock",
            "keeps openai-completions: models.json provider local",
            "keeps openai-responses: models.json provider local",
        ]
    );

    // An API named as a string in an extension's own source stays; dependencies and other files
    // do not count.
    fs::remove_file(f.agent().join("models.json")).unwrap();
    f.write_stage(
        &format!("{EXTENSION}/src/provider.ts"),
        "pi.registerProvider(\"x\", { api: 'openai-responses' });\n",
    );
    f.write_stage(
        &format!("{EXTENSION}/node_modules/dep/index.js"),
        "const api = \"google-generative-ai\";\n",
    );
    f.write_stage(
        &format!("{EXTENSION}/README.md"),
        "uses \"bedrock-converse-stream\"\n",
    );
    f.write_stage(
        &format!("{EXTENSION}/notes.ts"),
        "// mentions openai-completions bare\n",
    );
    let out = f.run(None).unwrap();
    assert_eq!(f.kept(&out), ["anthropic-messages", "openai-responses"]);
    assert_eq!(
        Fixture::notes(&out)[1],
        "keeps openai-responses: node_modules/ext-a names it"
    );
}

#[test]
fn a_broken_settings_file_fails_the_pass() {
    let f = Fixture::new();
    f.write_agent("settings.json", "{ not json");
    let err = f.err(None);
    assert!(
        err.contains("parsing") && err.contains("settings.json"),
        "{err}"
    );
}

/// Runs `checkFrozen` from the exact `pibake:frozen` text the bundle embeds; returns stderr.
fn check(frozen: &[Frozen], agent: &Path, argv: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("frozen.mjs"),
        bundle::frozen_module(frozen).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.path().join("driver.mjs"),
        "import { checkFrozen, frozen } from \"./frozen.mjs\";\ncheckFrozen(process.argv[2], process.argv.slice(3), frozen);\n",
    )
    .unwrap();
    let out = Command::new("node")
        .arg(dir.path().join("driver.mjs"))
        .arg(agent)
        .args(argv)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    String::from_utf8(out.stderr).unwrap()
}

fn line(items: &str, them: bool) -> String {
    format!(
        "node: this build left out {items}; add {} to [trim] apis in pibake.toml, then run pibake install\n",
        if them { "them" } else { "it" }
    )
}

#[test]
fn e2e_frozen_check_names_what_the_setup_asks_past() {
    if !e2e_enabled() {
        return;
    }
    let f = Fixture::new();
    let frozen = f
        .run(Some(&["anthropic-messages", "openai-completions"]))
        .unwrap()
        .frozen;
    f.write_agent("settings.json", r#"{"defaultProvider": "anthropic"}"#);
    f.write_agent(
        "models.json",
        r#"{"providers": {"local": {"api": "openai-completions", "models": [{"id": "a"}]}}}"#,
    );
    let agent = f.agent();
    let run = |argv: &[&str]| check(&frozen, &agent, argv);

    assert_eq!(run(&[]), "");
    assert_eq!(
        run(&["--model", "gemini-x"]),
        "",
        "provider-less patterns are not checked"
    );
    assert_eq!(run(&["--model", "local/a", "-p", "hi"]), "");
    assert_eq!(
        run(&["--provider", "google", "--model", "gemini-x"]),
        line("the google-generative-ai API (--provider google)", false)
    );
    assert_eq!(
        run(&["--model", "google/gemini-x:high"]),
        line(
            "the google-generative-ai API (--model google/gemini-x:high)",
            false
        )
    );
    assert_eq!(
        run(&["--models", "anthropic/a, amazon-bedrock/b"]),
        line(
            "the bedrock-converse-stream API (--models amazon-bedrock/b)",
            false
        )
    );
    assert_eq!(
        run(&["--provider", "github-copilot", "--model", "x"]),
        line(
            "the openai-responses API (--provider github-copilot)",
            false
        )
    );

    f.write_agent(
        "settings.json",
        r#"{"defaultProvider": "google", "enabledModels": ["amazon-bedrock/x", "github-copilot/y"]}"#,
    );
    f.write_agent(
        "models.json",
        r#"{"providers": {"mine": {"models": [{"id": "a", "api": "openai-responses"}]}}}"#,
    );
    assert_eq!(
        run(&["--provider", "google", "--model", "g"]),
        line(
            "the google-generative-ai API (--provider google), the google-generative-ai API (settings.json defaultProvider google), the bedrock-converse-stream API (settings.json enabledModels amazon-bedrock/x), 2 more",
            true
        )
    );

    // A malformed file is Pi's to report; the check stays quiet about it.
    f.write_agent("settings.json", "{ not json");
    f.write_agent("models.json", "[]");
    assert_eq!(run(&[]), "");

    let unknown: Vec<serde_json::Value> =
        vec![serde_json::json!({"kind": "theme", "setting": "[trim] theme"})];
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("frozen.mjs"),
        format!(
            "{}\nexport const frozen = {};\n",
            bundle::FROZEN,
            serde_json::to_string(&unknown).unwrap()
        ),
    )
    .unwrap();
    fs::write(
        dir.path().join("driver.mjs"),
        "import { checkFrozen, frozen } from \"./frozen.mjs\";\ncheckFrozen(\"/nonexistent\", [], frozen);\n",
    )
    .unwrap();
    let out = Command::new("node")
        .arg(dir.path().join("driver.mjs"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "node: checking [trim] theme failed: pibake has no check for theme\n"
    );
}

/// For random setups, the startup check finds nothing missing in the very setup the API set was
/// derived from; asking for a built-in provider names exactly its APIs that were left out.
#[hegel::test(test_cases = 30)]
fn e2e_derived_apis_cover_what_the_check_asks_for(tc: TestCase) {
    if !e2e_enabled() {
        return;
    }
    let f = Fixture::new();
    let providers: Vec<&str> = CATALOG.iter().map(|(p, _)| *p).collect();
    let api = || gs::sampled_from(APIS.to_vec());
    let default: &str = tc.draw(gs::sampled_from(providers.clone()));
    let enabled: Vec<&str> = tc.draw(gs::vecs(gs::sampled_from(providers.clone())).max_size(2));
    let mut custom = serde_json::Map::new();
    for i in 0..tc.draw(gs::integers::<usize>().max_value(2)) {
        let mut entry = serde_json::json!({ "models": [{ "id": "m" }] });
        if tc.draw(gs::booleans()) {
            entry["api"] = tc.draw(api()).into();
        }
        if tc.draw(gs::booleans()) {
            entry["models"][0]["api"] = tc.draw(api()).into();
        }
        custom.insert(format!("custom{i}"), entry);
    }
    let mentioned: Vec<&str> = tc.draw(gs::vecs(api()).max_size(2));
    f.write_agent(
        "settings.json",
        &serde_json::json!({
            "defaultProvider": default,
            "enabledModels": enabled.iter().map(|p| format!("{p}/model")).collect::<Vec<_>>(),
        })
        .to_string(),
    );
    f.write_agent(
        "models.json",
        &serde_json::json!({ "providers": custom }).to_string(),
    );
    f.write_stage(
        &format!("{EXTENSION}/index.ts"),
        &mentioned
            .iter()
            .map(|a| format!("const x = \"{a}\";\n"))
            .collect::<String>(),
    );

    let frozen = f.run(None).unwrap().frozen;
    assert_eq!(check(&frozen, &f.agent(), &[]), "");

    let [Frozen::Apis { left_out, .. }] = &frozen[..] else {
        panic!("{frozen:?}")
    };
    let asked: &str = tc.draw(gs::sampled_from(providers));
    let (_, asked_apis) = CATALOG.iter().find(|(p, _)| *p == asked).unwrap();
    // A custom provider of that name would add its own APIs; these are all `custom<i>`.
    let expected: Vec<String> = asked_apis
        .iter()
        .filter(|a| left_out.iter().any(|l| l == *a))
        .map(|a| format!("the {a} API (--provider {asked})"))
        .collect();
    let stderr = check(&frozen, &f.agent(), &["--provider", asked, "--model", "m"]);
    if expected.is_empty() {
        assert_eq!(stderr, "");
    } else {
        assert_eq!(stderr, line(&expected.join(", "), expected.len() > 1));
    }
}

/// For any kept set, exactly the other APIs' files are stubbed, each exactly as expected, the
/// frozen record lists them, and a second run on the output changes nothing.
#[hegel::test(test_cases = 200)]
fn trim_apis_stubs_exactly_the_rest_and_is_idempotent(tc: TestCase) {
    let keep: Vec<&str> = APIS
        .iter()
        .copied()
        .filter(|_| tc.draw(gs::booleans()))
        .collect();
    let f = Fixture::new();
    let out = f.run(Some(&keep)).unwrap();
    for api in APIS {
        let rel = api_file(api);
        let text = out
            .overrides
            .get(&rel)
            .cloned()
            .unwrap_or_else(|| fs::read_to_string(f.stage().join(&rel)).unwrap());
        assert_eq!(text, api_js(api, !keep.contains(&api)), "{api}");
        assert_eq!(
            out.overrides.contains_key(&rel),
            !keep.contains(&api),
            "{api}"
        );
    }
    let [Frozen::Apis { left_out, .. }] = &out.frozen[..] else {
        panic!("{:?}", out.frozen)
    };
    let expected: Vec<&str> = APIS.iter().copied().filter(|a| !keep.contains(a)).collect();
    assert_eq!(left_out, &expected);

    for (rel, text) in &out.overrides {
        f.write_stage(rel, text);
    }
    let again = f.run(Some(&keep)).unwrap();
    assert!(again.overrides.is_empty(), "{:?}", again.overrides.keys());
}
