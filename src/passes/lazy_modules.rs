//! `lazy_modules`: the listed modules are evaluated on the first read of an export, not at
//! startup (`bundle/build.mjs`, `lazyModules`). Each is on the list after a trace showing no
//! startup read in RPC or print mode, and a review showing a top level that only builds its own
//! values (`performance_plans/p6_lazy_modules.md`).
//!
//! Once loaded, an export no code reassigns becomes a plain value on the stand-in, as a getter on
//! every call would cost hot TUI paths ~25x per call. The pass finds those exports here; the rest
//! stay getters, so a reassigned binding reads as live as in stock.

use anyhow::{Context, Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::{Declaration, ModuleExportName, Statement};
use oxc::semantic::{Scoping, SemanticBuilder};
use oxc::span::SourceType;
use serde::Serialize;

use super::{Change, Sources, parse};
use crate::lock::PI_PACKAGE;

/// Package and file in it, the package resolved as Pi's own code finds it. No `.mjs` files: esbuild
/// takes one for an ES module whatever its text, and the stand-in is CommonJS.
pub const FILES: [(&str, &str); 11] = [
    ("marked", "lib/marked.esm.js"),
    ("@earendil-works/pi-tui", "dist/components/markdown.js"),
    ("@earendil-works/pi-tui", "dist/utils.js"),
    ("@earendil-works/pi-tui", "dist/autocomplete.js"),
    ("@earendil-works/pi-tui", "dist/word-navigation.js"),
    ("@earendil-works/pi-tui", "dist/components/editor.js"),
    ("@earendil-works/pi-tui", "dist/components/input.js"),
    ("@earendil-works/pi-tui", "dist/alt-screen-search.js"),
    ("@earendil-works/pi-tui", "dist/tui-alt-screen.js"),
    (PI_PACKAGE, "dist/modes/interactive/components/mermaid.js"),
    (
        PI_PACKAGE,
        "dist/modes/interactive/components/custom-editor.js",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LazyFile {
    /// Relative to the stage.
    pub file: String,
    /// Exports never reassigned: read once, at load.
    pub fixed: Vec<String>,
}

pub fn run(sources: &Sources) -> Result<(Vec<Change>, Vec<LazyFile>)> {
    let pi = format!("node_modules/{PI_PACKAGE}");
    let mut changes = vec![];
    let mut files = vec![];
    for (package, path) in FILES {
        let rel = format!("{}/{path}", sources.resolve_package(&pi, package)?);
        let (fixed, all) = fixed_exports(&sources.read(&rel)?, &rel)?;
        changes.push(Change {
            file: rel.clone(),
            what: format!(
                "evaluated on first use; {} of {all} exports fixed at load",
                fixed.len()
            ),
            bytes_removed: 0,
        });
        files.push(LazyFile { file: rel, fixed });
    }
    Ok((changes, files))
}

/// The module's own exports that no code reassigns, sorted, and the count of all its exports.
/// Re-exports of other modules stay live.
fn fixed_exports(text: &str, rel: &str) -> Result<(Vec<String>, usize)> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let semantic = SemanticBuilder::new().build(&program).semantic;
    let scoping = semantic.scoping();
    let root = scoping.root_scope_id();
    let local = |name: &str| -> Result<bool> {
        let symbol = scoping
            .get_binding(root, name.into())
            .with_context(|| format!("{rel}: export {name} has no top-level binding"))?;
        Ok(!written(scoping, symbol))
    };
    // (exported name, never reassigned)
    let mut exports = vec![];
    for statement in &program.body {
        match statement {
            Statement::ExportDeclaration(e) => match &e.declaration {
                Declaration::VariableDeclaration(v) => {
                    for id in v
                        .declarations
                        .iter()
                        .flat_map(|d| d.id.get_binding_identifiers())
                    {
                        exports.push((id.name.to_string(), local(&id.name)?));
                    }
                }
                Declaration::FunctionDeclaration(f) => {
                    let id = f.id.as_ref().context("an exported function has no name")?;
                    exports.push((id.name.to_string(), local(&id.name)?));
                }
                Declaration::ClassDeclaration(c) => {
                    let id = c.id.as_ref().context("an exported class has no name")?;
                    exports.push((id.name.to_string(), local(&id.name)?));
                }
                _ => bail!("{rel}: an exported TypeScript declaration in JavaScript"),
            },
            Statement::ExportNamedDeclaration(e) => {
                for s in &e.specifiers {
                    exports.push((export_name(&s.exported), local(&export_name(&s.local))?));
                }
            }
            Statement::ExportFromDeclaration(e) => {
                exports.extend(
                    e.specifiers
                        .iter()
                        .map(|s| (export_name(&s.exported), false)),
                );
            }
            Statement::ExportAllDeclaration(_) => exports.push(("*".to_owned(), false)),
            Statement::ExportDefaultDeclaration(_) => exports.push(("default".to_owned(), false)),
            _ => {}
        }
    }
    if exports.is_empty() {
        bail!("{rel} exports nothing as an ES module");
    }
    if exports.iter().any(|(name, _)| name == "default") {
        bail!("{rel} has a default export, which a lazy import cannot keep");
    }
    let all = exports.len();
    let mut fixed: Vec<String> = exports
        .into_iter()
        .filter_map(|(name, fixed)| fixed.then_some(name))
        .collect();
    fixed.sort();
    Ok((fixed, all))
}

fn export_name(name: &ModuleExportName) -> String {
    name.name().to_string()
}

fn written(scoping: &Scoping, symbol: oxc::semantic::SymbolId) -> bool {
    scoping
        .get_resolved_reference_ids(symbol)
        .iter()
        .any(|&r| scoping.get_reference(r).is_write())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_never_reassigned_are_fixed() {
        let text = "import { x } from \"./x.js\";\n\
            export const a = /re/v, b = 1;\n\
            export function f() { return a; }\n\
            export class C {}\n\
            var R = {};\n\
            function set(v) { R = v; }\n\
            let n = 0;\n\
            export { R as defaults, n as count, set };\n\
            export { y } from \"./y.js\";\n\
            export * from \"./z.js\";\n\
            export function g() {}\n\
            g = () => 1;\n";
        let (fixed, all) = fixed_exports(text, "m.js").unwrap();
        assert_eq!(fixed, ["C", "a", "b", "count", "f", "set"]);
        assert_eq!(all, 10);
    }

    #[test]
    fn a_default_export_or_no_exports_fails() {
        let err = fixed_exports("export default 1;\n", "m.js")
            .unwrap_err()
            .to_string();
        assert!(err.contains("m.js has a default export"), "{err}");
        let err = fixed_exports("const a = 1;\nexport { a as default };\n", "m.js")
            .unwrap_err()
            .to_string();
        assert!(err.contains("m.js has a default export"), "{err}");
        let err = fixed_exports("module.exports = {};\n", "m.js")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("m.js exports nothing as an ES module"),
            "{err}"
        );
    }
}
