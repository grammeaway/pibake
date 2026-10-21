//! `incremental_tool_json`: pi-ai's providers parse a streamed tool call incrementally.
//!
//! Each provider appends a delta to the call's buffer and re-parses the whole buffer with
//! `parseStreamingJson`, which makes a large streamed call quadratic. At each of these sites the
//! call goes to `pibake:tool-json` (`bundle/tool_json.mjs`), keyed by the tool-call object and
//! given the delta. It returns what stock would, from the delta alone while a string streams.
//! The parse at the end of a call (`parseToolCallArguments`) is untouched. The sites are matched
//! by exact text: any other per-delta call in `dist/api/` fails the pass, naming it.

use std::fs;
use std::ops::Range;

use anyhow::{Context, Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::{ImportDeclarationSpecifier, Statement};
use oxc::semantic::SemanticBuilder;
use oxc::span::{GetSpan, SourceType};

use super::{Change, Sources, check_parses, line_of, parse, splice};
use crate::lock::PI_PACKAGE;

const PI_AI: &str = "@earendil-works/pi-ai";
const API_DIR: &str = "dist/api";
const STOCK: &str = "parseStreamingJson";
const STOCK_MODULE: &str = "../utils/json-parse.js";
const OURS: &str = "__pibake_streamToolJson";
const IMPORT: &str =
    "import { streamToolJson as __pibake_streamToolJson } from \"pibake:tool-json\"; ";

/// One `parseStreamingJson(<buffer>)` call in Pi 1.3.0's pi-ai.
struct Site {
    /// In `dist/api/`.
    file: &'static str,
    /// What precedes the call, in order; any whitespace may separate them and the call.
    before: &'static [&'static str],
    call: &'static str,
    /// The tool-call object the parser state belongs to.
    key: &'static str,
    /// What was appended to the buffer; `null` when the buffer was replaced.
    delta: &'static str,
}

const SITES: [Site; 7] = [
    Site {
        file: "anthropic-messages.js",
        before: &[
            "block.partialJson += event.delta.partial_json;",
            "block.arguments =",
        ],
        call: "parseStreamingJson(block.partialJson)",
        key: "block",
        delta: "event.delta.partial_json",
    },
    Site {
        file: "bedrock-converse-stream.js",
        before: &[
            "block.partialJson = (block.partialJson || \"\") + (delta.toolUse.input || \"\");",
            "block.arguments =",
        ],
        call: "parseStreamingJson(block.partialJson)",
        key: "block",
        delta: "(delta.toolUse.input || \"\")",
    },
    Site {
        file: "mistral-conversations.js",
        before: &[
            "block.partialArgs = (block.partialArgs || \"\") + argsDelta;",
            "block.arguments =",
        ],
        call: "parseStreamingJson(block.partialArgs)",
        key: "block",
        delta: "argsDelta",
    },
    Site {
        file: "openai-completions.js",
        before: &[
            "block.partialArgs = (block.partialArgs ?? \"\") + toolCall.function.arguments;",
            "block.arguments =",
        ],
        call: "parseStreamingJson(block.partialArgs)",
        key: "block",
        delta: "toolCall.function.arguments",
    },
    Site {
        file: "openai-responses-shared.js",
        before: &[
            "slot.block.partialJson += event.delta;",
            "slot.block.arguments =",
        ],
        call: "parseStreamingJson(slot.block.partialJson)",
        key: "slot.block",
        delta: "event.delta",
    },
    // `response.function_call_arguments.done` replaces the buffer: the parser starts over.
    Site {
        file: "openai-responses-shared.js",
        before: &[
            "slot.block.partialJson = event.arguments;",
            "slot.block.arguments =",
        ],
        call: "parseStreamingJson(slot.block.partialJson)",
        key: "slot.block",
        delta: "null",
    },
    Site {
        file: "pi-messages.js",
        before: &[
            "toolJson.set(event.contentIndex, json);",
            "partial.content[event.contentIndex].arguments =",
        ],
        call: "parseStreamingJson(json)",
        key: "partial.content[event.contentIndex]",
        delta: "event.delta",
    },
];

pub struct Out {
    pub changes: Vec<Change>,
    /// Rewritten files: a provider whose API `trim_apis` left out is not bundled.
    pub optional: Vec<String>,
}

pub fn run(sources: &mut Sources) -> Result<Out> {
    let pi_ai = sources.resolve_package(&format!("node_modules/{PI_PACKAGE}/dist"), PI_AI)?;
    let api_dir = format!("{pi_ai}/{API_DIR}");
    let mut files: Vec<&str> = SITES.iter().map(|s| s.file).collect();
    files.dedup();
    unknown_callers(&sources.stage().join(&api_dir), &files)?;
    let mut out = Out {
        changes: vec![],
        optional: vec![],
    };
    for file in files {
        let rel = format!("{api_dir}/{file}");
        let text = sources.read(&rel)?;
        let sites: Vec<&Site> = SITES.iter().filter(|s| s.file == file).collect();
        let new = rewrite(&text, &rel, &sites)?;
        if let Some(removed) = sources.replace(&rel, new)? {
            out.changes.push(Change {
                file: rel.clone(),
                what: format!(
                    "parses {} streamed tool-call site(s) incrementally",
                    sites.len()
                ),
                bytes_removed: removed,
            });
        }
        out.optional.push(rel);
    }
    Ok(out)
}

/// A provider in `dir` that calls `parseStreamingJson` without a site here would stay quadratic.
fn unknown_callers(dir: &std::path::Path, known: &[&str]) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut unknown = vec![];
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".js")
            && !known.contains(&name.as_str())
            && fs::read_to_string(dir.join(&name))?.contains(STOCK)
        {
            unknown.push(name);
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        bail!(
            "{} in {} use {STOCK}, which pibake's incremental_tool_json does not know; it needs an update",
            unknown.join(", "),
            dir.display()
        );
    }
    Ok(())
}

/// `text` with each site's call sent to `pibake:tool-json`. Every call of the imported
/// `parseStreamingJson` must be one of `sites`, each found once.
fn rewrite(text: &str, rel: &str, sites: &[&Site]) -> Result<String> {
    if text.contains(OURS) {
        bail!("{rel} already calls {OURS}");
    }
    let allocator = Allocator::default();
    let program = parse(&allocator, text, SourceType::mjs(), rel)?;
    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&program)
        .semantic;
    let scoping = semantic.scoping();
    let mut stock = None;
    for statement in &program.body {
        let Statement::ImportDeclaration(d) = statement else {
            continue;
        };
        for s in d.specifiers.iter().flatten() {
            if let ImportDeclarationSpecifier::ImportSpecifier(s) = s
                && s.local.name == STOCK
            {
                if d.source.value != STOCK_MODULE || s.imported.name() != STOCK {
                    bail!("{rel}: `{STOCK}` is not imported from \"{STOCK_MODULE}\"");
                }
                stock = Some(s.local.symbol_id());
            }
        }
    }
    let stock = stock.with_context(|| format!("{rel}: `{STOCK}` is not imported"))?;
    let calls: Vec<usize> = scoping
        .get_resolved_reference_ids(stock)
        .iter()
        .map(|&r| {
            let node = scoping.get_reference(r).node_id();
            semantic.nodes().get_node(node).span().start as usize
        })
        .collect();
    let mut edits: Vec<(Range<usize>, String)> = vec![];
    for &at in &calls {
        let found: Vec<&&Site> = sites
            .iter()
            .filter(|s| text[at..].starts_with(s.call) && preceded_by(&text[..at], s.before))
            .collect();
        let [site] = found[..] else {
            bail!(
                "{rel}:{}: `{}` is not a call site pibake's incremental_tool_json knows; it needs an update",
                line_of(text, at),
                text[at..].lines().next().unwrap_or("")
            );
        };
        let argument = &site.call[STOCK.len() + 1..site.call.len() - 1];
        edits.push((
            at..at + site.call.len(),
            format!("{OURS}({STOCK}, {}, {argument}, {})", site.key, site.delta),
        ));
    }
    if edits.len() != sites.len() {
        bail!(
            "{rel}: expected {} streamed tool-call site(s), found {}; pibake's incremental_tool_json needs an update",
            sites.len(),
            edits.len()
        );
    }
    // On the first line, so the file's line numbers stay as they are.
    edits.push((0..0, IMPORT.to_owned()));
    let new = splice(text, &mut edits)?;
    check_parses(&new, rel)?;
    Ok(new)
}

fn preceded_by(mut text: &str, fragments: &[&str]) -> bool {
    for fragment in fragments.iter().rev() {
        let Some(rest) = text.trim_end().strip_suffix(fragment) else {
            return false;
        };
        text = rest;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &str = "import { parseJsonWithRepair, parseStreamingJson, parseToolCallArguments } from \"../utils/json-parse.js\";\nfunction f(event, block) {\n    block.partialJson += event.delta.partial_json;\n    block.arguments = parseStreamingJson(block.partialJson);\n    return parseToolCallArguments(block.partialJson);\n}\n";

    #[test]
    fn rewrites_the_site_and_keeps_line_numbers() {
        let sites: Vec<&Site> = SITES
            .iter()
            .filter(|s| s.file == "anthropic-messages.js")
            .collect();
        let out = rewrite(ANTHROPIC, "a.js", &sites).unwrap();
        assert_eq!(out.lines().count(), ANTHROPIC.lines().count());
        assert!(out.starts_with(IMPORT));
        assert!(out.contains("    block.arguments = __pibake_streamToolJson(parseStreamingJson, block, block.partialJson, event.delta.partial_json);\n"), "{out}");
        assert!(out.contains("return parseToolCallArguments(block.partialJson);"));
        let err = rewrite(&out, "a.js", &sites).unwrap_err().to_string();
        assert!(err.contains("already calls"), "{err}");
    }

    #[test]
    fn an_unknown_call_or_a_missing_site_fails() {
        let sites: Vec<&Site> = SITES
            .iter()
            .filter(|s| s.file == "anthropic-messages.js")
            .collect();
        let extra = ANTHROPIC.replace(
            "    return",
            "    const x = parseStreamingJson(other);\n    return",
        );
        let err = format!("{:#}", rewrite(&extra, "a.js", &sites).unwrap_err());
        assert!(
            err.contains("a.js:5: `parseStreamingJson(other);`"),
            "{err}"
        );
        let changed = ANTHROPIC.replace("+= event.delta.partial_json", "+= d");
        let err = format!("{:#}", rewrite(&changed, "a.js", &sites).unwrap_err());
        assert!(err.contains("not a call site"), "{err}");
        let gone = ANTHROPIC.replace(
            "    block.arguments = parseStreamingJson(block.partialJson);\n",
            "",
        );
        let err = format!("{:#}", rewrite(&gone, "a.js", &sites).unwrap_err());
        assert!(
            err.contains("expected 1 streamed tool-call site(s), found 0"),
            "{err}"
        );
        let elsewhere = ANTHROPIC.replace("../utils/json-parse.js", "./mine.js");
        let err = format!("{:#}", rewrite(&elsewhere, "a.js", &sites).unwrap_err());
        assert!(err.contains("not imported from"), "{err}");
    }

    #[test]
    fn whitespace_between_fragments_is_free() {
        assert!(preceded_by("a;\n   b =\n      ", &["a;", "b ="]));
        assert!(!preceded_by("a; c; b = ", &["a;", "b ="]));
    }
}
