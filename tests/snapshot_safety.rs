mod common;

use std::fs;
use std::path::Path;

use pibake::lock::PI_PACKAGE;
use pibake::passes::{self, Options, Output};

fn snapshot(stage: &Path) -> anyhow::Result<Output> {
    passes::run(
        stage,
        &Options {
            snapshot: true,
            ..Options::none()
        },
        &common::no_setup(stage),
    )
}

#[test]
fn every_site_of_pi_1_3_is_deferred_and_reported() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    let out = snapshot(stage.path()).unwrap();
    assert_eq!(out.overrides.len(), 9, "{:?}", out.overrides.keys());
    let report = &out.reports[0];
    assert_eq!(report.pass.name(), "snapshot_safety");
    let whats: Vec<&str> = report.changes.iter().map(|c| c.what.as_str()).collect();
    assert!(
        whats.contains(&"deferred graphemeSegmenter, wordSegmenter to launch"),
        "{whats:?}"
    );
    assert!(
        whats.contains(&"deferred stdoutColor, stderrColor to launch"),
        "{whats:?}"
    );

    let timings = &out.overrides[&format!("node_modules/{PI_PACKAGE}/dist/core/timings.js")];
    assert!(
        timings.starts_with(
            "let ENABLED; globalThis.__pibake_reinit(() => { (ENABLED = process.env.PI_TIMING === \"1\"); });\nexport function time() {"
        ),
        "{timings}"
    );
    let chalk = &out.overrides["node_modules/chalk/source/index.js"];
    assert!(
        chalk.contains("export const chalkStderr = createChalk({level: stderrColor ? stderrColor.level : 0}); globalThis.__pibake_reinit(() => { chalk.level = stdoutColor ? stdoutColor.level : 0; chalkStderr.level = stderrColor ? stderrColor.level : 0; });\n"),
        "{chalk}"
    );
    let colors = &out.overrides["node_modules/chalk/source/vendor/supports-color/index.js"];
    assert!(
        colors.contains("globalThis.__pibake_reinit(() => { if (\n\thasFlag('no-color')"),
        "{colors}"
    );
    assert!(colors.contains("const supportsColor = {}; globalThis.__pibake_reinit(() => { Object.assign(supportsColor, {"), "{colors}");
    // The argv check comes first, so it is resumed first.
    assert!(colors.find("hasFlag('no-color')").unwrap() < colors.find("Object.assign").unwrap());
}

#[test]
fn the_pass_only_runs_for_snapshots_and_leaves_its_output_as_is() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    let off = passes::run(
        stage.path(),
        &Options::none(),
        &common::no_setup(stage.path()),
    )
    .unwrap();
    assert!(off.overrides.is_empty() && off.reports.is_empty());

    let first = snapshot(stage.path()).unwrap();
    for (rel, text) in &first.overrides {
        fs::write(stage.path().join(rel), text).unwrap();
    }
    let again = snapshot(stage.path()).unwrap();
    assert!(again.overrides.is_empty(), "{:?}", again.overrides.keys());
}

#[test]
fn packages_resolve_as_pi_sees_them() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    // chalk nested under Pi wins over the hoisted copy.
    let nested = format!("node_modules/{PI_PACKAGE}/node_modules/chalk");
    for rel in [
        "source/index.js",
        "source/vendor/supports-color/index.js",
        "package.json",
    ] {
        let from = stage.path().join("node_modules/chalk").join(rel);
        common::write(
            &stage.path().join(&nested).join(rel),
            &fs::read_to_string(from).unwrap(),
        );
    }
    let out = snapshot(stage.path()).unwrap();
    assert!(
        out.overrides
            .contains_key(&format!("{nested}/source/index.js"))
    );
    assert!(
        !out.overrides
            .contains_key("node_modules/chalk/source/index.js")
    );
}

#[test]
fn a_changed_pi_fails_loudly_naming_the_file() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    let timings = stage
        .path()
        .join(format!("node_modules/{PI_PACKAGE}/dist/core/timings.js"));
    fs::write(
        &timings,
        "const ENABLED = process.env.PI_TIMING === \"yes\";\n",
    )
    .unwrap();
    let err = format!("{:#}", snapshot(stage.path()).unwrap_err());
    assert!(err.contains("dist/core/timings.js: `const ENABLED = process.env.PI_TIMING === \"1\";` is not a top-level statement"), "{err}");

    common::write_snapshot_stage(stage.path());
    fs::write(
        &timings,
        "const ENABLED = process.env.PI_TIMING === \"1\";\nexport const VERBOSE = ENABLED;\n",
    )
    .unwrap();
    let err = format!("{:#}", snapshot(stage.path()).unwrap_err());
    assert!(
        err.contains("dist/core/timings.js:2: `ENABLED` is read at load"),
        "{err}"
    );

    common::write_snapshot_stage(stage.path());
    fs::remove_dir_all(stage.path().join("node_modules/grok-mermaid")).unwrap();
    let err = format!("{:#}", snapshot(stage.path()).unwrap_err());
    assert!(err.contains("grok-mermaid"), "{err}");
}

/// Pi 1.5.1 renamed grok-mermaid to lovely-mermaid, and added tool search's word segmenter.
#[test]
fn the_sites_of_pi_1_5_1_are_deferred() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    let modules = stage.path().join("node_modules");
    fs::rename(modules.join("grok-mermaid"), modules.join("lovely-mermaid")).unwrap();
    let tool = format!("node_modules/{PI_PACKAGE}/dist/extensions/tool-search/tool.js");
    common::write(
        &stage.path().join(&tool),
        "const wordSegmenter = new Intl.Segmenter(undefined, { granularity: \"word\" });\nexport const words = (s) => [...wordSegmenter.segment(s)];\n",
    );
    let out = snapshot(stage.path()).unwrap();
    assert_eq!(out.overrides.len(), 10, "{:?}", out.overrides.keys());
    assert!(
        out.overrides
            .contains_key("node_modules/lovely-mermaid/dist/width.js")
    );
    assert!(out.overrides[&tool].starts_with("let wordSegmenter; globalThis.__pibake_reinit("));
}

/// An extension's dependency can install the package Pi used to need (grok-mermaid next to Pi
/// 1.5.1's lovely-mermaid). Both are rewritten, and a bundle without that extension leaves it out.
#[test]
fn installed_alternatives_are_all_deferred_and_optional() {
    let stage = tempfile::tempdir().unwrap();
    common::write_snapshot_stage(stage.path());
    let modules = stage.path().join("node_modules");
    fs::create_dir_all(modules.join("lovely-mermaid")).unwrap();
    for rel in ["dist/width.js", "package.json"] {
        let text = fs::read_to_string(modules.join("grok-mermaid").join(rel)).unwrap();
        common::write(&modules.join("lovely-mermaid").join(rel), &text);
    }
    let out = snapshot(stage.path()).unwrap();
    for rel in [
        "node_modules/grok-mermaid/dist/width.js",
        "node_modules/lovely-mermaid/dist/width.js",
    ] {
        assert!(out.overrides.contains_key(rel), "{rel}");
        assert!(out.optional.iter().any(|o| o == rel), "{rel}: {:?}", out.optional);
    }
    assert_eq!(out.optional.len(), 2, "{:?}", out.optional);
}
