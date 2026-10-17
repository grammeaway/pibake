//! `snapshot_safety`: what the `sea_snapshot` backend needs from module code. Every module the
//! entry imports runs at build time, and the snapshot keeps the result, so module code must not
//! read anything a launch decides (environment, terminal, agent directory) or create native state
//! V8 cannot serialize. Three explicit mechanisms:
//!
//! - **Deferred initialisers** (this pass): each listed top-level `const`, matched by its exact
//!   text, is rewritten to be assigned by `__pibake_reinit`, which runs it when the snapshot
//!   resumes, or at once outside a snapshot build (`bundle/prologue_snapshot.cjs`). A reference to
//!   a deferred binding outside a function fails the pass: it would read `undefined` at build.
//! - **Lazy modules** (`bundle/build.mjs`): `LAZY_BUILTINS` and `LAZY_PACKAGES` load on first use.
//! - **The gate** (`check`): what `bundle/trace.cjs` saw module code read or create at load must be
//!   listed in `ACCEPTED`, or the build fails naming the module.
//!
//! Signal listeners and the working directory are the prologue's, for any module.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::{Context, Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::AstKind;
use oxc::ast::ast::{Expression, Statement};
use oxc::semantic::SemanticBuilder;
use oxc::span::{GetSpan, SourceType, Span};
use serde::Deserialize;

use super::{Change, Sources, parse, splice};
use crate::lock::PI_PACKAGE;

/// Builtins whose loading creates state a snapshot cannot hold (each checked alone on Node 26).
pub const LAZY_BUILTINS: [&str; 6] = ["http", "https", "http2", "tls", "repl", "sqlite"];
/// Packages that load such builtins and use them at their own top level.
pub const LAZY_PACKAGES: [&str; 1] = ["undici"];

const REINIT: &str = "globalThis.__pibake_reinit";
const PI: Option<&str> = None;

struct Site {
    /// The package, as Pi's `dist/` resolves it; `None` for Pi itself.
    package: Option<&'static str>,
    file: &'static str,
    /// The top-level statement, as written.
    statement: &'static str,
    how: How,
    /// A top-level statement that reads the binding at load, and what to rerun after it.
    rerun: Option<(&'static str, &'static str)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    /// `const <pattern> = <init>;` becomes `let <names>;`, assigned on resume.
    Const,
    /// `const <name> = { … };` stays the same object, filled in on resume, so importers that
    /// hold it see the contents.
    InPlace,
    /// The whole statement runs on resume.
    Statement,
}

const CALLBACK_HOST: &str =
    r#"const CALLBACK_HOST = getProviderEnvValue("PI_OAUTH_CALLBACK_HOST") || "127.0.0.1";"#;

/// Pi 1.3.0's load-time reads of launch state, and its ICU objects created at load.
const SITES: [Site; 11] = [
    Site {
        package: Some("@earendil-works/pi-tui"),
        file: "dist/utils.js",
        statement: r#"const graphemeSegmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });"#,
        how: How::Const,
        rerun: None,
    },
    Site {
        package: Some("@earendil-works/pi-tui"),
        file: "dist/utils.js",
        statement: r#"const wordSegmenter = new Intl.Segmenter(undefined, { granularity: "word" });"#,
        how: How::Const,
        rerun: None,
    },
    Site {
        package: PI,
        file: "dist/modes/interactive/components/easter-egg-3d.js",
        statement: "const segmenter = new Intl.Segmenter();",
        how: How::Const,
        rerun: None,
    },
    Site {
        package: Some("grok-mermaid"),
        file: "dist/width.js",
        statement: "const segmenter = new Intl.Segmenter('en', { granularity: 'grapheme' });",
        how: How::Const,
        rerun: None,
    },
    Site {
        package: PI,
        file: "dist/core/timings.js",
        statement: r#"const ENABLED = process.env.PI_TIMING === "1";"#,
        how: How::Const,
        rerun: None,
    },
    Site {
        package: Some("@earendil-works/pi-ai"),
        file: "dist/auth/oauth/anthropic.js",
        statement: CALLBACK_HOST,
        how: How::Const,
        rerun: None,
    },
    Site {
        package: Some("@earendil-works/pi-ai"),
        file: "dist/auth/oauth/openai-chatgpt.js",
        statement: CALLBACK_HOST,
        how: How::Const,
        rerun: None,
    },
    Site {
        package: PI,
        file: "dist/utils/tools-manager.js",
        statement: "const TOOLS_DIR = getBinDir();",
        how: How::Const,
        rerun: None,
    },
    Site {
        package: Some("chalk"),
        file: "source/vendor/supports-color/index.js",
        statement: "if (\n\thasFlag('no-color')\n\t|| hasFlag('no-colors')\n\t|| hasFlag('color=false')\n\t|| hasFlag('color=never')\n) {\n\tflagForceColor = 0;\n} else if (\n\thasFlag('color')\n\t|| hasFlag('colors')\n\t|| hasFlag('color=true')\n\t|| hasFlag('color=always')\n) {\n\tflagForceColor = 1;\n}",
        how: How::Statement,
        rerun: None,
    },
    Site {
        package: Some("chalk"),
        file: "source/vendor/supports-color/index.js",
        statement: "const supportsColor = {\n\tstdout: createSupportsColor({isTTY: tty.isatty(1)}),\n\tstderr: createSupportsColor({isTTY: tty.isatty(2)}),\n};",
        how: How::InPlace,
        rerun: None,
    },
    Site {
        package: Some("chalk"),
        file: "source/index.js",
        statement: "const {stdout: stdoutColor, stderr: stderrColor} = supportsColor;",
        how: How::Const,
        rerun: Some((
            "export const chalkStderr = createChalk({level: stderrColor ? stderrColor.level : 0});",
            "chalk.level = stdoutColor ? stdoutColor.level : 0; chalkStderr.level = stderrColor ? stderrColor.level : 0;",
        )),
    },
];

pub fn run(sources: &mut Sources) -> Result<Vec<Change>> {
    let pi = format!("node_modules/{PI_PACKAGE}");
    let mut by_file: BTreeMap<String, Vec<&Site>> = BTreeMap::new();
    for site in &SITES {
        let dir = match site.package {
            None => pi.clone(),
            Some(p) => sources.resolve_package(&format!("{pi}/dist"), p)?,
        };
        by_file
            .entry(format!("{dir}/{}", site.file))
            .or_default()
            .push(site);
    }
    let mut changes = vec![];
    for (rel, sites) in by_file {
        let text = sources.read(&rel)?;
        // Already rewritten: a pass runs once, but its output must stay a fixed point.
        if text.contains(REINIT) {
            continue;
        }
        let (rewritten, names) = defer(&text, &rel, &sites)?;
        if let Some(removed) = sources.replace(&rel, rewritten)? {
            changes.push(Change {
                file: rel,
                what: format!("deferred {} to launch", names.join(", ")),
                bytes_removed: removed,
            });
        }
    }
    Ok(changes)
}

fn defer(text: &str, rel: &str, sites: &[&Site]) -> Result<(String, Vec<String>)> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&program)
        .semantic;
    let scoping = semantic.scoping();
    let top_level = |wanted: &str| {
        program
            .body
            .iter()
            .find(|s| text[range(s.span())].trim() == wanted)
            .with_context(|| {
                format!("{rel}: `{wanted}` is not a top-level statement any more; pibake's snapshot_safety needs an update")
            })
    };
    let mut edits = vec![];
    let mut all_names = vec![];
    for site in sites {
        let statement = top_level(site.statement)?;
        if site.how == How::Statement {
            let body = &text[range(statement.span())];
            edits.push((
                range(statement.span()),
                format!("{REINIT}(() => {{ {body} }});"),
            ));
            continue;
        }
        let Some(decl) = const_declarator(statement) else {
            bail!("{rel}: `{}` is not one `const`", site.statement);
        };
        let init = decl.init.as_ref().context("a const has an initialiser")?;
        let rerun = site
            .rerun
            .map(|(s, _)| top_level(s).map(|s| range(s.span())))
            .transpose()?;
        let names: Vec<&str> = decl
            .id
            .get_binding_identifiers()
            .iter()
            .map(|id| id.name.as_str())
            .collect();
        for id in decl.id.get_binding_identifiers() {
            for &r in scoping.get_resolved_reference_ids(id.symbol_id()) {
                let reference = scoping.get_reference(r);
                let in_function = scoping
                    .scope_ancestors(reference.scope_id())
                    .any(|s| scoping.scope_flags(s).is_function());
                let node = reference.node_id();
                let exported = matches!(
                    semantic.nodes().parent_kind(node),
                    AstKind::ExportSpecifier(_)
                );
                let at = range(semantic.nodes().get_node(node).span());
                let rerun_reads = rerun.as_ref().is_some_and(|r| r.contains(&at.start));
                // An in-place object exists at load; only its contents wait.
                if !(in_function || exported || rerun_reads || site.how == How::InPlace) {
                    bail!(
                        "{rel}:{}: `{}` is read at load, where a deferred value is still undefined",
                        line_of(text, at.start),
                        id.name
                    );
                }
            }
        }
        let pattern = &text[range(decl.id.span())];
        let init_text = &text[range(init.span())];
        let replacement = if site.how == How::InPlace {
            if !matches!(init, Expression::ObjectExpression(_)) || names.len() != 1 {
                bail!("{rel}: `{pattern}` is no longer one object literal");
            }
            format!(
                "const {pattern} = {{}}; {REINIT}(() => {{ Object.assign({pattern}, {init_text}); }});"
            )
        } else {
            format!(
                "let {}; {REINIT}(() => {{ ({pattern} = {init_text}); }});",
                names.join(", ")
            )
        };
        edits.push((range(statement.span()), replacement));
        if let (Some(r), Some((_, code))) = (rerun, site.rerun) {
            edits.push((r.end..r.end, format!(" {REINIT}(() => {{ {code} }});")));
        }
        all_names.extend(names.iter().map(|n| n.to_string()));
    }
    Ok((splice(text, &mut edits)?, all_names))
}

fn const_declarator<'s, 'a>(
    statement: &'s Statement<'a>,
) -> Option<&'s oxc::ast::ast::VariableDeclarator<'a>> {
    match statement {
        Statement::VariableDeclaration(d) if d.kind.is_const() && d.declarations.len() == 1 => {
            d.declarations.first()
        }
        _ => None,
    }
}

fn range(span: Span) -> Range<usize> {
    span.start as usize..span.end as usize
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// What `bundle/trace.cjs` saw a module do at load.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub struct Finding {
    /// The bundled module (stage-relative, as esbuild names it).
    pub module: String,
    /// `env`, `cwd`, `argv`, `os`, `tty`, `time`, `random`, `timer`, `intl`, `load`, `fs`, `live`.
    pub kind: String,
    pub detail: String,
    /// In the bundle file.
    pub line: u64,
}

struct Accepted {
    /// The module's path from its package name, e.g. `which/which.js`.
    file: &'static str,
    kind: &'static str,
    detail: &'static str,
    why: &'static str,
}

/// Load-time reads the snapshot may keep, each with why the frozen value is the launch's too.
const ACCEPTED: [Accepted; 14] = [
    Accepted {
        file: "graceful-fs/polyfills.js",
        kind: "cwd",
        detail: "",
        why: "memoizes the working directory; the resume clears it with process.chdir and checks it",
    },
    Accepted {
        file: "graceful-fs/polyfills.js",
        kind: "env",
        detail: "GRACEFUL_FS_PLATFORM",
        why: "a test hook",
    },
    Accepted {
        file: "graceful-fs/graceful-fs.js",
        kind: "env",
        detail: "TEST_GRACEFUL_FS_GLOBAL_PATCH",
        why: "a test hook",
    },
    Accepted {
        file: "graceful-fs/graceful-fs.js",
        kind: "env",
        detail: "NODE_DEBUG",
        why: "graceful-fs's debug log; the build's environment has none",
    },
    Accepted {
        file: "semver/internal/debug.js",
        kind: "env",
        detail: "NODE_DEBUG",
        why: "semver's debug log; the build's environment has none",
    },
    Accepted {
        file: "minimatch/dist/esm/index.js",
        kind: "env",
        detail: "__MINIMATCH_TESTING_PLATFORM__",
        why: "a test hook",
    },
    Accepted {
        file: "which/which.js",
        kind: "env",
        detail: "OSTYPE",
        why: "Cygwin/MSYS detection; the host does not change",
    },
    Accepted {
        file: "@earendil-works/pi-coding-agent/dist/core/keybindings.js",
        kind: "env",
        detail: "WSL_DISTRO_NAME",
        why: "WSL detection for default keys; the host does not change",
    },
    Accepted {
        file: "@earendil-works/pi-coding-agent/dist/core/keybindings.js",
        kind: "env",
        detail: "WSL_INTEROP",
        why: "WSL detection for default keys; the host does not change",
    },
    Accepted {
        file: "@earendil-works/pi-coding-agent/dist/config.js",
        kind: "env",
        detail: "PI_PACKAGE_DIR",
        why: "a pibake executable carries its own Pi package",
    },
    Accepted {
        file: "@earendil-works/pi-coding-agent/dist/extensions/codemode/tool.js",
        kind: "env",
        detail: "PI_PACKAGE_DIR",
        why: "a pibake executable carries its own Pi package",
    },
    Accepted {
        file: "signal-exit/index.js",
        kind: "env",
        detail: "FORCE_COLOR",
        why: "Node's `assert` picks its error-diff colours at load; diffs are uncoloured",
    },
    Accepted {
        file: "signal-exit/index.js",
        kind: "tty",
        detail: "stderr.isTTY",
        why: "Node's `assert` picks its error-diff colours at load; diffs are uncoloured",
    },
    Accepted {
        file: "brace-expansion/dist/esm/index.js",
        kind: "random",
        detail: "Math.random()",
        why: "placeholder tokens, unique per process either way",
    },
];

/// The module's path from its package name: after the last `node_modules/`.
fn package_file(module: &str) -> &str {
    module
        .rfind("node_modules/")
        .map_or(module, |i| &module[i + "node_modules/".len()..])
}

/// The accepted findings, one line each; any other finding fails, naming its module.
pub fn check(findings: &[Finding]) -> Result<Vec<String>> {
    let mut accepted = BTreeSet::new();
    let mut unknown = BTreeMap::new();
    for f in findings {
        // Loading other builtins is fine; these are what `LAZY_BUILTINS` must keep out.
        if f.kind == "load" && !LAZY_BUILTINS.contains(&f.detail.as_str()) {
            continue;
        }
        let file = package_file(&f.module);
        match ACCEPTED
            .iter()
            .find(|a| a.file == file && a.kind == f.kind && a.detail == f.detail)
        {
            Some(a) => {
                accepted.insert(format!("{file}: {} {} ({})", a.kind, a.detail, a.why));
            }
            None => {
                unknown
                    .entry((f.module.as_str(), f.kind.as_str(), f.detail.as_str()))
                    .or_insert(f.line);
            }
        }
    }
    if !unknown.is_empty() {
        let lines: Vec<String> = unknown
            .iter()
            .map(|((module, kind, detail), line)| {
                format!("  {module} (main.cjs:{line}): {kind} {detail}")
            })
            .collect();
        bail!(
            "a snapshot would keep the build's value of what these modules read or create at load:\n{}\n\
             handle them in pibake's src/passes/snapshot_safety.rs, or build with [build] backend = \"sea_codecache\"",
            lines.join("\n")
        );
    }
    Ok(accepted.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(module: &str, kind: &str, detail: &str, line: u64) -> Finding {
        Finding {
            module: module.into(),
            kind: kind.into(),
            detail: detail.into(),
            line,
        }
    }

    #[test]
    fn package_file_is_the_path_from_the_package_name() {
        assert_eq!(
            package_file("node_modules/which/which.js"),
            "which/which.js"
        );
        assert_eq!(
            package_file("node_modules/@s/p/node_modules/graceful-fs/polyfills.js"),
            "graceful-fs/polyfills.js"
        );
        assert_eq!(
            package_file("local/extensions/a.ts"),
            "local/extensions/a.ts"
        );
    }

    #[test]
    fn the_gate_accepts_listed_reads_wherever_the_package_sits() {
        let kept = check(&[
            finding("node_modules/which/which.js", "env", "OSTYPE", 3),
            finding(
                "node_modules/a/node_modules/which/which.js",
                "env",
                "OSTYPE",
                9,
            ),
            finding("node_modules/graceful-fs/polyfills.js", "cwd", "", 5),
            // Other builtins may load; only the lazy ones may not.
            finding("node_modules/x/index.js", "load", "fs", 7),
        ])
        .unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(
            kept[0].starts_with("graceful-fs/polyfills.js: cwd "),
            "{kept:?}"
        );
        assert!(
            kept[1].starts_with("which/which.js: env OSTYPE ("),
            "{kept:?}"
        );
        assert!(check(&[]).unwrap().is_empty());
    }

    #[test]
    fn the_gate_names_each_unhandled_read_once_with_its_first_line() {
        let err = check(&[
            finding("node_modules/which/which.js", "env", "PATH", 3),
            finding("node_modules/which/which.js", "env", "PATH", 1),
            finding("node_modules/x/index.js", "load", "http2", 7),
            finding("node_modules/x/index.js", "time", "Date.now()", 8),
            // A listed variable read by another module is not accepted.
            finding("node_modules/y/index.js", "env", "OSTYPE", 2),
        ])
        .unwrap_err();
        let text = format!("{err:#}");
        for line in [
            "  node_modules/which/which.js (main.cjs:3): env PATH\n",
            "  node_modules/x/index.js (main.cjs:7): load http2\n",
            "  node_modules/x/index.js (main.cjs:8): time Date.now()\n",
            "  node_modules/y/index.js (main.cjs:2): env OSTYPE\n",
        ] {
            assert!(text.contains(line), "{line:?} in:\n{text}");
        }
        assert_eq!(text.matches("env PATH").count(), 1, "{text}");
    }

    fn site(statement: &'static str, how: How) -> Site {
        Site {
            package: None,
            file: "f.js",
            statement,
            how,
            rerun: None,
        }
    }

    #[test]
    fn each_kind_of_site_rewrites_to_its_shape() {
        let text = "import { f } from \"./f.js\";\nconst A = f(1);\nconst {x: B, y} = f(2);\nconst O = {\n\tk: f(3),\n};\nexport default O;\nlet flag;\nif (f(4)) {\n\tflag = 1;\n}\nexport function g() { return [A, B, y, flag]; }\n";
        let sites = [
            site("const A = f(1);", How::Const),
            site("const {x: B, y} = f(2);", How::Const),
            site("const O = {\n\tk: f(3),\n};", How::InPlace),
            site("if (f(4)) {\n\tflag = 1;\n}", How::Statement),
        ];
        let (out, names) = defer(text, "f.js", &sites.iter().collect::<Vec<_>>()).unwrap();
        assert_eq!(names, ["A", "B", "y", "O"]);
        assert_eq!(
            out,
            "import { f } from \"./f.js\";\n\
             let A; globalThis.__pibake_reinit(() => { (A = f(1)); });\n\
             let B, y; globalThis.__pibake_reinit(() => { ({x: B, y} = f(2)); });\n\
             const O = {}; globalThis.__pibake_reinit(() => { Object.assign(O, {\n\tk: f(3),\n}); });\n\
             export default O;\nlet flag;\n\
             globalThis.__pibake_reinit(() => { if (f(4)) {\n\tflag = 1;\n} });\n\
             export function g() { return [A, B, y, flag]; }\n"
        );
    }

    #[test]
    fn a_rerun_follows_its_statement_and_may_read_the_binding() {
        let text = "const {s: S} = src;\nexport const C = make(S);\nexport { S as shown };\n";
        let site = Site {
            rerun: Some(("export const C = make(S);", "C.level = S;")),
            ..site("const {s: S} = src;", How::Const)
        };
        let (out, _) = defer(text, "f.js", &[&site]).unwrap();
        assert_eq!(
            out,
            "let S; globalThis.__pibake_reinit(() => { ({s: S} = src); });\n\
             export const C = make(S); globalThis.__pibake_reinit(() => { C.level = S; });\n\
             export { S as shown };\n"
        );
    }

    #[test]
    fn a_read_at_load_or_a_changed_statement_fails_naming_the_file() {
        let read = defer(
            "const A = f();\nconst B = A + 1;\n",
            "pkg/f.js",
            &[&site("const A = f();", How::Const)],
        )
        .unwrap_err();
        assert_eq!(
            format!("{read:#}"),
            "pkg/f.js:2: `A` is read at load, where a deferred value is still undefined"
        );
        let block = defer(
            "const A = f();\n{ use(A); }\n",
            "pkg/f.js",
            &[&site("const A = f();", How::Const)],
        )
        .unwrap_err();
        assert!(format!("{block:#}").contains("pkg/f.js:2"), "{block:#}");
        let moved = defer(
            "const A = f(2);\n",
            "pkg/f.js",
            &[&site("const A = f();", How::Const)],
        )
        .unwrap_err();
        assert!(
            format!("{moved:#}")
                .contains("pkg/f.js: `const A = f();` is not a top-level statement"),
            "{moved:#}"
        );
        let not_const = defer(
            "let A = f();\n",
            "pkg/f.js",
            &[&site("let A = f();", How::Const)],
        )
        .unwrap_err();
        assert!(
            format!("{not_const:#}").contains("is not one `const`"),
            "{not_const:#}"
        );
        let not_object = defer(
            "const O = f();\n",
            "pkg/f.js",
            &[&site("const O = f();", How::InPlace)],
        )
        .unwrap_err();
        assert!(
            format!("{not_object:#}").contains("one object literal"),
            "{not_object:#}"
        );
    }

    /// Runs `driver` with `node` in `dir`, returning its stdout.
    fn node(dir: &std::path::Path, driver: &str) -> String {
        std::fs::write(dir.join("driver.mjs"), driver).unwrap();
        let out = std::process::Command::new("node")
            .arg(dir.join("driver.mjs"))
            .env_clear()
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Random top-level `const`s reading the environment, deferred: evaluated at once they give
    /// the original's values; deferred, then resumed under another environment, they give what
    /// the original gives under that one.
    #[hegel::test(test_cases = 30)]
    fn deferred_consts_read_the_environment_they_run_in(tc: hegel::TestCase) {
        use hegel::generators as gs;
        if std::env::var_os("PIBAKE_E2E").is_none_or(|v| v != "1") {
            return;
        }
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let mut statements = vec![];
        let mut names = vec![];
        for i in 0..count {
            let var = format!("PIBAKE_P{}", tc.draw(gs::integers::<u8>().max_value(2)));
            let init = match tc.draw(gs::integers::<u8>().max_value(3)) {
                0 => format!("process.env.{var} === \"1\""),
                1 => format!("process.env.{var} || \"dflt\""),
                2 => format!("(process.env.{var} ?? \"\").length"),
                _ => format!("{{ a: process.env.{var} ?? null, b: {i} }}"),
            };
            let statement = if init.starts_with('{') && tc.draw(gs::booleans()) {
                names.extend([format!("a{i}"), format!("b{i}")]);
                format!("const {{a: a{i}, b: b{i}}} = {init};")
            } else {
                names.push(format!("v{i}"));
                format!("const v{i} = {init};")
            };
            statements.push(statement);
        }
        let filler = "function helper() { return 1; }\n";
        let text = format!(
            "{filler}{}\nexport function values() {{ return [{}]; }}\n",
            statements.join("\n"),
            names.join(", ")
        );
        let leaked: Vec<&'static str> = statements
            .iter()
            .map(|s| &*Box::leak(s.clone().into_boxed_str()))
            .collect();
        let sites: Vec<Site> = leaked.iter().map(|s| site(s, How::Const)).collect();
        let (out, _) = defer(&text, "m.mjs", &sites.iter().collect::<Vec<_>>()).unwrap();
        let allocator = Allocator::default();
        parse(&allocator, &out, SourceType::mjs(), "m.mjs").unwrap();

        let draw_env = |tc: &hegel::TestCase| -> serde_json::Value {
            let mut env = serde_json::Map::new();
            for k in 0..3 {
                if let Some(v) = tc.draw(gs::optional(gs::sampled_from(vec!["1", "0", "", "x y"])))
                {
                    env.insert(format!("PIBAKE_P{k}"), v.into());
                }
            }
            env.into()
        };
        let (first, second) = (draw_env(&tc), draw_env(&tc));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("original.mjs"), &text).unwrap();
        std::fs::write(dir.path().join("deferred.mjs"), &out).unwrap();
        let result = node(
            dir.path(),
            &format!(
                r#"const set = (env) => {{ for (const k of Object.keys(process.env)) delete process.env[k]; Object.assign(process.env, env); }};
const [first, second] = [{first}, {second}];
let building = false;
const queue = [];
globalThis.__pibake_reinit = (init) => (building ? queue.push(init) : init());
set(first);
const now = (await import("./deferred.mjs?now")).values();
const nowExpected = (await import("./original.mjs?first")).values();
building = true;
const resumed = await import("./deferred.mjs?resumed");
set(second);
for (const init of queue) init();
const later = resumed.values();
const laterExpected = (await import("./original.mjs?second")).values();
console.log(JSON.stringify([now, nowExpected, later, laterExpected]));
"#
            ),
        );
        let [now, now_expected, later, later_expected]: [serde_json::Value; 4] =
            serde_json::from_str(&result).unwrap();
        assert_eq!(now, now_expected, "{out}");
        assert_eq!(later, later_expected, "{out}");
    }
}
