mod common;

use std::path::Path;

use common::{PI_AI, write};
use pibake::passes::{self, Options, Output, Pass};

const IMPORT: &str =
    "import { parseStreamingJson, parseToolCallArguments } from \"../utils/json-parse.js\";\n";

/// Each provider's per-delta lines as pi-ai 1.3.0 writes them, then its end-of-call parse.
const PROVIDERS: [(&str, &str); 6] = [
    (
        "anthropic-messages.js",
        "                            block.partialJson += event.delta.partial_json;\n                            block.arguments = parseStreamingJson(block.partialJson);\n",
    ),
    (
        "bedrock-converse-stream.js",
        "        block.partialJson = (block.partialJson || \"\") + (delta.toolUse.input || \"\");\n        block.arguments = parseStreamingJson(block.partialJson);\n",
    ),
    (
        "mistral-conversations.js",
        "            block.partialArgs = (block.partialArgs || \"\") + argsDelta;\n            block.arguments = parseStreamingJson(block.partialArgs);\n",
    ),
    (
        "openai-completions.js",
        "                                block.partialArgs = (block.partialArgs ?? \"\") + toolCall.function.arguments;\n                                block.arguments = parseStreamingJson(block.partialArgs);\n",
    ),
    (
        "openai-responses-shared.js",
        "            slot.block.partialJson += event.delta;\n            slot.block.arguments = parseStreamingJson(slot.block.partialJson);\n            pushToolCallDelta(slot, event.delta);\n            const previousPartialJson = slot.block.partialJson;\n            slot.block.partialJson = event.arguments;\n            slot.block.arguments = parseStreamingJson(slot.block.partialJson);\n",
    ),
    (
        "pi-messages.js",
        "                const json = `${toolJson.get(event.contentIndex) ?? \"\"}${event.delta}`;\n                toolJson.set(event.contentIndex, json);\n                partial.content[event.contentIndex].arguments =\n                    parseStreamingJson(json);\n",
    ),
];

fn provider_js(body: &str) -> String {
    format!("{IMPORT}function f() {{\n{body}    return parseToolCallArguments(\"{{}}\");\n}}\n")
}

fn stage() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(PI_AI).join("package.json"),
        r#"{"name": "@earendil-works/pi-ai", "version": "1.3.0"}"#,
    );
    for (file, body) in PROVIDERS {
        write(
            &dir.path().join(PI_AI).join("dist/api").join(file),
            &provider_js(body),
        );
    }
    write(
        &dir.path()
            .join(PI_AI)
            .join("dist/api/openai-codex-responses.js"),
        "import { x } from \"./openai-responses-shared.js\";\n",
    );
    dir
}

fn run(stage: &Path) -> anyhow::Result<Output> {
    passes::run(
        stage,
        &Options {
            passes: vec![Pass::IncrementalToolJson],
            ..Options::none()
        },
        &common::no_setup(stage),
    )
}

#[test]
fn every_site_calls_the_incremental_parser_and_nothing_else_changes() {
    let s = stage();
    let out = run(s.path()).unwrap();
    let rel = |f: &str| format!("{PI_AI}/dist/api/{f}");
    assert_eq!(
        out.overrides.keys().cloned().collect::<Vec<_>>(),
        PROVIDERS.map(|(f, _)| rel(f))
    );
    assert_eq!(out.optional, PROVIDERS.map(|(f, _)| rel(f)));
    let call = |key: &str, buffer: &str, delta: &str| {
        format!("__pibake_streamToolJson(parseStreamingJson, {key}, {buffer}, {delta})")
    };
    let expected = [
        (
            "anthropic-messages.js",
            vec![call(
                "block",
                "block.partialJson",
                "event.delta.partial_json",
            )],
        ),
        (
            "bedrock-converse-stream.js",
            vec![call(
                "block",
                "block.partialJson",
                "(delta.toolUse.input || \"\")",
            )],
        ),
        (
            "mistral-conversations.js",
            vec![call("block", "block.partialArgs", "argsDelta")],
        ),
        (
            "openai-completions.js",
            vec![call(
                "block",
                "block.partialArgs",
                "toolCall.function.arguments",
            )],
        ),
        (
            "openai-responses-shared.js",
            vec![
                call("slot.block", "slot.block.partialJson", "event.delta"),
                call("slot.block", "slot.block.partialJson", "null"),
            ],
        ),
        (
            "pi-messages.js",
            vec![call(
                "partial.content[event.contentIndex]",
                "json",
                "event.delta",
            )],
        ),
    ];
    for ((file, body), (_, calls)) in PROVIDERS.iter().zip(expected) {
        let mut want = provider_js(body);
        let stock_calls: Vec<_> = want
            .match_indices("parseStreamingJson(")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(stock_calls.len(), calls.len(), "{file}");
        for (at, new) in stock_calls.iter().zip(&calls).rev() {
            let end = at + want[*at..].find(';').unwrap();
            want.replace_range(at..&end, new);
        }
        want.insert_str(
            0,
            "import { streamToolJson as __pibake_streamToolJson } from \"pibake:tool-json\"; ",
        );
        assert_eq!(out.overrides[&rel(file)], want, "{file}");
    }
}

#[test]
fn an_unknown_provider_calling_the_stock_parser_fails_the_pass() {
    let s = stage();
    write(
        &s.path().join(PI_AI).join("dist/api/new-api.js"),
        &provider_js("        block.arguments = parseStreamingJson(block.partialJson);\n"),
    );
    let err = format!("{:#}", run(s.path()).unwrap_err());
    assert!(err.contains("pass incremental_tool_json"), "{err}");
    assert!(err.contains("new-api.js in "), "{err}");
}

#[test]
fn a_changed_site_fails_the_pass_naming_it() {
    let s = stage();
    let (file, body) = PROVIDERS[3];
    write(
        &s.path().join(PI_AI).join("dist/api").join(file),
        &provider_js(&body.replace("(block.partialArgs ?? \"\")", "block.partialArgs")),
    );
    let err = format!("{:#}", run(s.path()).unwrap_err());
    assert!(
        err.contains(
            "openai-completions.js:4: `parseStreamingJson(block.partialArgs);` is not a call site"
        ),
        "{err}"
    );
}

#[test]
fn a_missing_provider_file_fails_the_pass() {
    let s = stage();
    std::fs::remove_file(s.path().join(PI_AI).join("dist/api/pi-messages.js")).unwrap();
    let err = format!("{:#}", run(s.path()).unwrap_err());
    assert!(err.contains("pi-messages.js"), "{err}");
}
