//! `trim_highlight`: highlight.js registers only the languages in `[trim] highlight_languages`.
//!
//! Pi 1.3.0 registers a few languages at startup (`eagerLanguages` in
//! `dist/utils/syntax-highlight.js`) and, in interactive mode after the first render, all of
//! them through `highlight.js/lib/index.js`. Both lists are cut to the kept languages; esbuild
//! then leaves the dropped grammars out. A fence in a dropped language renders as plain text,
//! as Pi renders any language highlight.js does not know.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::{Result, anyhow, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    Argument, AssignmentTarget, Expression, ImportDeclarationSpecifier, ObjectPropertyKind,
    PropertyKind, Statement,
};
use oxc::semantic::SemanticBuilder;
use oxc::span::{GetSpan, SourceType, Span};

use super::{Change, Sources, parse, splice, with_line_break};
use crate::lock::PI_PACKAGE;

/// Pi's startup languages plus those of the owner's code (2026-10-16, owner).
pub const DEFAULT_LANGUAGES: [&str; 39] = [
    "armasm",
    "bash",
    "c",
    "cpp",
    "csharp",
    "css",
    "dart",
    "diff",
    "dockerfile",
    "dts",
    "erb",
    "go",
    "gradle",
    "groovy",
    "ini",
    "java",
    "javascript",
    "json",
    "kotlin",
    "lua",
    "makefile",
    "markdown",
    "nix",
    "perl",
    "php",
    "plaintext",
    "protobuf",
    "python",
    "ruby",
    "rust",
    "scala",
    "scss",
    "shell",
    "sql",
    "swift",
    "typescript",
    "x86asm",
    "xml",
    "yaml",
];

const SYNTAX: &str = "dist/utils/syntax-highlight.js";
const LANGUAGE_IMPORT: &str = "highlight.js/lib/languages/";
const EAGER: &str = "eagerLanguages";
const INDEX_SHAPE: &str = "`var hljs = require('./core');`, one `hljs.registerLanguage('<name>', require('./languages/<file>'));` per language, then `module.exports = hljs;`";

pub fn run(sources: &mut Sources, keep: &[String]) -> Result<Vec<Change>> {
    let syntax_rel = format!("node_modules/{PI_PACKAGE}/{SYNTAX}");
    let from = format!("node_modules/{PI_PACKAGE}/dist/utils");
    let index_rel = format!(
        "{}/lib/index.js",
        sources.resolve_package(&from, "highlight.js")?
    );
    let index_text = sources.read(&index_rel)?;
    let syntax_text = sources.read(&syntax_rel)?;
    let index = registrations(&index_text, &index_rel)?;
    let eager = eager_languages(&syntax_text, &syntax_rel)?;

    let known: BTreeSet<&str> = index
        .iter()
        .map(|r| r.name.as_str())
        .chain(eager.properties.iter().map(|p| p.name.as_str()))
        .collect();
    let unknown: Vec<&str> = keep
        .iter()
        .map(String::as_str)
        .filter(|k| !known.contains(k))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "[trim] highlight_languages names {} that highlight.js does not register; it knows: {}",
            unknown.join(", "),
            known.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let kept = |name: &str| keep.iter().any(|k| k == name);

    let mut changes = vec![];
    let dropped: Vec<&Registration> = index.iter().filter(|r| !kept(&r.name)).collect();
    let mut edits: Vec<_> = dropped
        .iter()
        .map(|r| {
            (
                with_line_break(&index_text, r.statement.clone()),
                String::new(),
            )
        })
        .collect();
    if let Some(removed) = sources.replace(&index_rel, splice(&index_text, &mut edits)?)? {
        changes.push(Change {
            file: index_rel,
            what: format!(
                "kept {} of {} languages; dropped {}",
                index.len() - dropped.len(),
                index.len(),
                names(dropped.iter().map(|r| r.name.as_str()))
            ),
            bytes_removed: removed,
        });
    }

    let (keep_props, drop_props): (Vec<&Property>, Vec<&Property>) =
        eager.properties.iter().partition(|p| kept(&p.name));
    if !drop_props.is_empty() {
        let mut object = String::from("{\n");
        for p in &keep_props {
            object.push_str(&format!("    {},\n", &syntax_text[p.span.clone()]));
        }
        object.push('}');
        let mut edits = vec![(eager.object.clone(), object)];
        for p in &drop_props {
            edits.push((
                with_line_break(&syntax_text, p.import.clone()),
                String::new(),
            ));
        }
        let text = splice(&syntax_text, &mut edits)?;
        if let Some(removed) = sources.replace(&syntax_rel, text)? {
            changes.push(Change {
                file: syntax_rel,
                what: format!(
                    "dropped {} of {} startup languages: {}",
                    drop_props.len(),
                    eager.properties.len(),
                    names(drop_props.iter().map(|p| p.name.as_str()))
                ),
                bytes_removed: removed,
            });
        }
    }
    Ok(changes)
}

fn names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

fn range(span: Span) -> Range<usize> {
    span.start as usize..span.end as usize
}

/// The statement's first line, for error messages.
fn snippet(text: &str, span: Span) -> String {
    let s = &text[range(span)];
    let line = s.lines().next().unwrap_or("");
    if line.len() > 100 {
        format!("{}…", &line[..line.floor_char_boundary(100)])
    } else {
        line.to_owned()
    }
}

#[derive(Debug)]
struct Registration {
    name: String,
    statement: Range<usize>,
}

/// highlight.js's `lib/index.js`; any statement outside its known shape fails the pass.
fn registrations(text: &str, rel: &str) -> Result<Vec<Registration>> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::cjs(), rel)?;
    let unexpected = |s: &Statement| {
        anyhow!(
            "{rel}: expected {INDEX_SHAPE}; found `{}`",
            snippet(text, s.span())
        )
    };
    let [first, middle @ .., last] = &program.body[..] else {
        bail!("{rel}: expected {INDEX_SHAPE}; found fewer than two statements");
    };
    let hljs = match first {
        Statement::VariableDeclaration(d) if d.declarations.len() == 1 => {
            let decl = &d.declarations[0];
            match (decl.id.get_identifier_name(), &decl.init) {
                (Some(name), Some(Expression::CallExpression(call)))
                    if call
                        .common_js_require()
                        .is_some_and(|s| s.value == "./core") =>
                {
                    name
                }
                _ => return Err(unexpected(first)),
            }
        }
        _ => return Err(unexpected(first)),
    };
    match last {
        Statement::ExpressionStatement(e) => match &e.expression {
            Expression::AssignmentExpression(a)
                if a.right.is_specific_id(hljs.as_str())
                    && matches!(&a.left, AssignmentTarget::StaticMemberExpression(m)
                        if m.object.is_specific_id("module") && m.property.name == "exports") => {}
            _ => return Err(unexpected(last)),
        },
        _ => return Err(unexpected(last)),
    }
    middle
        .iter()
        .map(|s| {
            let Statement::ExpressionStatement(e) = s else {
                return Err(unexpected(s));
            };
            let Expression::CallExpression(call) = &e.expression else {
                return Err(unexpected(s));
            };
            match &call.arguments[..] {
                [
                    Argument::StringLiteral(name),
                    Argument::CallExpression(module),
                ] if call
                    .callee
                    .is_specific_member_access(hljs.as_str(), "registerLanguage")
                    && module
                        .common_js_require()
                        .is_some_and(|m| m.value.starts_with("./languages/")) =>
                {
                    Ok(Registration {
                        name: name.value.to_string(),
                        statement: range(s.span()),
                    })
                }
                _ => Err(unexpected(s)),
            }
        })
        .collect()
}

#[derive(Debug)]
struct Property {
    name: String,
    span: Range<usize>,
    /// The import declaration of its grammar.
    import: Range<usize>,
}

#[derive(Debug)]
struct Eager {
    object: Range<usize>,
    properties: Vec<Property>,
}

/// Pi's `const eagerLanguages = { name, … }`, each value a default import of a grammar used
/// nowhere else.
fn eager_languages(text: &str, rel: &str) -> Result<Eager> {
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let semantic = SemanticBuilder::new().build(&program).semantic;
    let scoping = semantic.scoping();
    let mut imports = BTreeMap::new();
    let mut object = None;
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(d) if d.source.value.starts_with(LANGUAGE_IMPORT) => {
                let Some([ImportDeclarationSpecifier::ImportDefaultSpecifier(spec)]) =
                    d.specifiers.as_ref().map(|s| &s[..])
                else {
                    bail!(
                        "{rel}: expected `import <name> from \"{}\"`; found `{}`",
                        d.source.value,
                        snippet(text, d.span)
                    );
                };
                let uses = scoping
                    .get_resolved_reference_ids(spec.local.symbol_id())
                    .len();
                if uses != 1 {
                    bail!(
                        "{rel}: the grammar `{}` is used {uses} times; pibake expects only `{EAGER}` to use it",
                        spec.local.name
                    );
                }
                imports.insert(spec.local.name.as_str(), range(d.span));
            }
            Statement::VariableDeclaration(d) => {
                for decl in &d.declarations {
                    if decl.id.get_identifier_name().is_some_and(|n| n == EAGER) {
                        match &decl.init {
                            Some(Expression::ObjectExpression(o)) => object = Some(o),
                            _ => bail!("{rel}: `{EAGER}` is no longer an object literal"),
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let Some(object) = object else {
        bail!("{rel}: `const {EAGER} = {{ … }}` not found");
    };
    let mut properties = vec![];
    for p in &object.properties {
        let prop = match p {
            ObjectPropertyKind::ObjectProperty(p)
                if p.kind == PropertyKind::Init && !p.method && !p.computed =>
            {
                p
            }
            _ => bail!(
                "{rel}: expected `<name>: <grammar>` in `{EAGER}`; found `{}`",
                snippet(text, p.span())
            ),
        };
        let (Some(name), Expression::Identifier(id)) = (prop.key.static_name(), &prop.value) else {
            bail!(
                "{rel}: expected `<name>: <grammar>` in `{EAGER}`; found `{}`",
                snippet(text, prop.span)
            );
        };
        let Some(import) = imports.remove(id.name.as_str()) else {
            bail!(
                "{rel}: `{EAGER}` uses `{}`, which is not imported from {LANGUAGE_IMPORT}",
                id.name
            );
        };
        properties.push(Property {
            name: name.into_owned(),
            span: range(prop.span),
            import,
        });
    }
    if let Some(unused) = imports.keys().next() {
        bail!("{rel}: the grammar `{unused}` is imported but not in `{EAGER}`");
    }
    Ok(Eager {
        object: range(object.span),
        properties,
    })
}
