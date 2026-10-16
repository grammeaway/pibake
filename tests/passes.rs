mod common;

use std::fs;
use std::path::Path;

use common::{HIGHLIGHT_INDEX, SYNTAX_HIGHLIGHT, highlight_index_js, syntax_highlight_js, write};
use pibake::passes::{self, Options, Output, Pass};

fn trim(stage: &Path, keep: &[&str]) -> anyhow::Result<Output> {
    passes::run(
        stage,
        &Options {
            passes: vec![Pass::TrimHighlight],
            highlight_languages: keep.iter().map(|s| s.to_string()).collect(),
            apis: None,
        },
        &common::no_setup(stage),
    )
}

fn err(stage: &Path, keep: &[&str]) -> String {
    format!("{:#}", trim(stage, keep).unwrap_err())
}

fn stage(index: &[&str], eager: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    common::write_highlight_stage(dir.path(), index, eager);
    dir
}

const INDEX: [&str; 6] = ["1c", "bash", "c-like", "rust", "xml", "zephir"];
const EAGER: [&str; 3] = ["bash", "rust", "c-like"];

#[test]
fn keeps_exactly_the_listed_languages() {
    let s = stage(&INDEX, &EAGER);
    let out = trim(s.path(), &["rust", "xml"]).unwrap();
    // Exactly the files as if they had been written with the kept languages only.
    assert_eq!(
        out.overrides[HIGHLIGHT_INDEX],
        highlight_index_js(&["rust", "xml"])
    );
    assert_eq!(
        out.overrides[SYNTAX_HIGHLIGHT],
        syntax_highlight_js(&["rust"])
    );
    let [report] = &out.reports[..] else {
        panic!("{:?}", out.reports)
    };
    assert_eq!(report.pass, Pass::TrimHighlight);
    let whats: Vec<(&str, &str)> = report
        .changes
        .iter()
        .map(|c| (c.file.as_str(), c.what.as_str()))
        .collect();
    assert_eq!(
        whats,
        [
            (
                HIGHLIGHT_INDEX,
                "kept 2 of 6 languages; dropped 1c, bash, c-like, zephir"
            ),
            (
                SYNTAX_HIGHLIGHT,
                "dropped 2 of 3 startup languages: bash, c-like"
            ),
        ]
    );
    assert!(report.changes.iter().all(|c| c.bytes_removed > 0));
}

#[test]
fn keeping_everything_rewrites_nothing() {
    let s = stage(&INDEX, &EAGER);
    let out = trim(s.path(), &INDEX).unwrap();
    assert!(out.overrides.is_empty(), "{:?}", out.overrides);
    assert!(out.reports[0].changes.is_empty());
}

#[test]
fn running_on_its_own_output_changes_nothing() {
    let s = stage(&INDEX, &EAGER);
    let first = trim(s.path(), &["rust", "1c"]).unwrap();
    for (rel, text) in &first.overrides {
        fs::write(s.path().join(rel), text).unwrap();
    }
    let second = trim(s.path(), &["rust", "1c"]).unwrap();
    assert!(second.overrides.is_empty(), "{:?}", second.overrides);
}

#[test]
fn passes_off_rewrite_nothing() {
    let s = stage(&INDEX, &EAGER);
    let out = passes::run(s.path(), &Options::none(), &common::no_setup(s.path())).unwrap();
    assert_eq!(out, Output::default());
}

#[test]
fn unknown_languages_are_named() {
    let s = stage(&INDEX, &EAGER);
    let e = err(s.path(), &["rust", "hcl", "vue"]);
    assert!(
        e.contains("names hcl, vue that highlight.js does not register"),
        "{e}"
    );
    assert!(
        e.contains("it knows: 1c, bash, c-like, rust, xml, zephir"),
        "{e}"
    );
}

#[test]
fn an_unknown_index_shape_fails_with_the_pattern_and_the_statement() {
    let s = stage(&INDEX, &EAGER);
    let index = s.path().join(HIGHLIGHT_INDEX);
    let text = highlight_index_js(&INDEX);
    for (bad, found) in [
        (
            text.replace(
                "hljs.registerLanguage('xml', ",
                "hljs.registerLanguage(lang('xml'), ",
            ),
            "hljs.registerLanguage(lang('xml')",
        ),
        (
            text.replace(
                "module.exports = hljs;",
                "module.exports = hljs;\nhljs.configure({});",
            ),
            "hljs.configure({});",
        ),
        (
            text.replace("require('./core')", "require('./core.js')"),
            "var hljs = require('./core.js');",
        ),
        (
            text.replace("module.exports = hljs;", "exports.default = hljs;"),
            "exports.default = hljs;",
        ),
    ] {
        fs::write(&index, bad).unwrap();
        let e = err(s.path(), &["rust"]);
        assert!(e.contains("hljs.registerLanguage('<name>'"), "{e}");
        assert!(e.contains(&format!("found `{found}")), "{e}");
    }
    fs::write(&index, "this is not ( javascript").unwrap();
    assert!(err(s.path(), &["rust"]).contains("does not parse"));
    fs::remove_dir_all(s.path().join("node_modules/highlight.js")).unwrap();
    let e = err(s.path(), &["rust"]);
    assert!(e.contains("highlight.js is not installed"), "{e}");
}

#[test]
fn an_unknown_eager_shape_fails() {
    let s = stage(&INDEX, &EAGER);
    let syntax = s.path().join(SYNTAX_HIGHLIGHT);
    let text = syntax_highlight_js(&EAGER);
    for (bad, expected) in [
        (
            text.replace("const eagerLanguages", "const startupLanguages")
                .replace("entries(eagerLanguages)", "entries(startupLanguages)"),
            "`const eagerLanguages = { … }` not found",
        ),
        (
            text.replace(
                "    \"bash\": lang_bash,\n",
                "    \"bash\": lang_bash,\n    ...more,\n",
            ),
            "found `...more`",
        ),
        (
            format!("{text}export const b = lang_bash;\n"),
            "`lang_bash` is used 2 times",
        ),
        (
            format!(
                "{}export const b = lang_bash;\n",
                text.replace("    \"bash\": lang_bash,\n", "")
            ),
            "`lang_bash` is imported but not in `eagerLanguages`",
        ),
        (
            text.replace("\"bash\": lang_bash", "\"bash\": other"),
            "`lang_bash` is used 0 times",
        ),
        (
            text.replace("import lang_rust from", "import * as lang_rust from"),
            "expected `import <name> from \"highlight.js/lib/languages/rust.js\"`",
        ),
        (
            text.replace(
                "const eagerLanguages = {",
                "const eagerLanguages = Object.assign({",
            )
            .replace("};\nfor", "});\nfor"),
            "`eagerLanguages` is no longer an object literal",
        ),
    ] {
        fs::write(&syntax, bad).unwrap();
        let e = err(s.path(), &["rust"]);
        assert!(e.contains(expected), "{expected}: {e}");
    }
    fs::remove_file(&syntax).unwrap();
    assert!(err(s.path(), &["rust"]).contains("syntax-highlight.js"));
}

#[test]
fn reads_a_nested_highlight_js_first() {
    let s = stage(&INDEX, &EAGER);
    let nested = "node_modules/@earendil-works/pi-coding-agent/node_modules/highlight.js";
    write(
        &s.path().join(nested).join("package.json"),
        r#"{"name": "highlight.js"}"#,
    );
    write(
        &s.path().join(nested).join("lib/index.js"),
        &highlight_index_js(&["rust", "go"]),
    );
    let out = trim(s.path(), &["rust"]).unwrap();
    let files: Vec<&String> = out.overrides.keys().collect();
    assert_eq!(
        files,
        [
            &SYNTAX_HIGHLIGHT.to_owned(),
            &format!("{nested}/lib/index.js")
        ]
    );
}
