//! `prewarm_first_turn`: once RPC or interactive mode is ready, and after a model change, Pi
//! makes its HTTP dispatcher (deferred by `lazy_http_dispatcher`) and evaluates the current
//! model's stream API module, each in its own macrotask, instead of on the first request.
//!
//! The same modules and dispatcher result, only sooner, while the user types. pi-ai's `lazyApi`
//! keeps its loader as a non-enumerable `warm`; compat's `BUILTIN_APIS` holds one per API, and
//! every provider's loader for an API imports the same module. Each edit stays on its line.

use anyhow::{Result, bail};

use super::{Change, Sources, check_parses};
use crate::lock::PI_PACKAGE;

const PI_AI: &str = "@earendil-works/pi-ai";
const COMPAT: &str = "from \"@earendil-works/pi-ai/compat\";";

/// Reads `globalThis.fetch` (the dispatcher), then calls the model's API loader. Unref'd, so a
/// warm-up never holds the process open; a failed load is left for the request to report.
const PREWARM: &str = "export function pibakePrewarm(getModel) { const later = (f) => setTimeout(f).unref(); later(() => { void globalThis.fetch; later(() => BUILTIN_APIS.find(([api]) => api === getModel()?.api)?.[1].warm?.()?.catch(() => { })); }); } ";

struct Edit {
    package: &'static str,
    file: &'static str,
    /// Pi 1.3.0's text; it must occur exactly once.
    old: String,
    new: String,
}

fn edit(package: &'static str, file: &'static str, old: &str, new: &str) -> Edit {
    Edit {
        package,
        file,
        old: old.into(),
        new: new.into(),
    }
}

fn edits() -> Vec<Edit> {
    const RPC: &str = "dist/modes/rpc/rpc-mode.js";
    const INTERACTIVE: &str = "dist/modes/interactive/interactive-mode.js";
    const SESSION: &str = "dist/core/agent-session.js";
    let crypto = "import * as crypto from \"node:crypto\";";
    let interactive_import = format!("import {{ isRetryableAssistantError, }} {COMPAT}");
    let session_import = format!("streamSimple, }} {COMPAT}");
    let model_select =
        "        await this._extensionRunner.emit({\n            type: \"model_select\",";
    let keep_alive = "    // Keep process alive forever\n    return new Promise(() => { });";
    let run = "    async run() {\n        await this.init();\n";
    vec![
        edit(
            PI_AI,
            "dist/api/lazy.js",
            "    return api;\n}",
            "    return Object.defineProperty(api, \"warm\", { value: load });\n}",
        ),
        edit(
            PI_AI,
            "dist/compat.js",
            "const BUILTIN_APIS = [",
            &format!("{PREWARM}const BUILTIN_APIS = ["),
        ),
        edit(
            PI_PACKAGE,
            RPC,
            crypto,
            &format!("import {{ pibakePrewarm }} {COMPAT} {crypto}"),
        ),
        edit(
            PI_PACKAGE,
            RPC,
            keep_alive,
            &keep_alive.replacen("return", "pibakePrewarm(() => session.model); return", 1),
        ),
        edit(
            PI_PACKAGE,
            INTERACTIVE,
            &interactive_import,
            &interactive_import.replacen("{ ", "{ pibakePrewarm, ", 1),
        ),
        edit(
            PI_PACKAGE,
            INTERACTIVE,
            run,
            &run.replacen(
                "init();",
                "init(); pibakePrewarm(() => this.session.model);",
                1,
            ),
        ),
        edit(
            PI_PACKAGE,
            SESSION,
            &session_import,
            &format!("streamSimple, pibakePrewarm, }} {COMPAT}"),
        ),
        edit(
            PI_PACKAGE,
            SESSION,
            model_select,
            &model_select.replacen("await", "pibakePrewarm(() => nextModel); await", 1),
        ),
    ]
}

pub fn run(sources: &mut Sources) -> Result<Vec<Change>> {
    let edits = edits();
    let mut files: Vec<String> = edits
        .iter()
        .map(|e| format!("node_modules/{}/{}", e.package, e.file))
        .collect();
    files.dedup();
    let mut changes = vec![];
    for rel in files {
        let mine = edits
            .iter()
            .filter(|e| rel == format!("node_modules/{}/{}", e.package, e.file));
        let new = rewrite(&sources.read(&rel)?, &rel, mine)?;
        if let Some(removed) = sources.replace(&rel, new)? {
            changes.push(Change {
                file: rel,
                what: "warms the first turn's HTTP dispatcher and stream API in idle time".into(),
                bytes_removed: removed,
            });
        }
    }
    Ok(changes)
}

fn rewrite<'a>(text: &str, rel: &str, edits: impl Iterator<Item = &'a Edit>) -> Result<String> {
    let mut out = text.to_owned();
    for e in edits {
        if out.matches(&e.old).count() != 1 {
            bail!(
                "{rel}: `{}` is not there exactly once; pibake's prewarm_first_turn needs an update",
                e.old
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            );
        }
        out = out.replacen(&e.old, &e.new, 1);
    }
    check_parses(&out, rel)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for each file, holding Pi 1.3.0's texts the edits expect.
    fn files() -> Vec<(String, String)> {
        let mut by_file: Vec<(String, String)> = vec![];
        for e in edits() {
            let rel = format!("node_modules/{}/{}", e.package, e.file);
            let body = match e.file {
                "dist/api/lazy.js" => {
                    "export function lazyApi(load) {\n    const api = {};\n    return api;\n}\n"
                        .to_owned()
                }
                "dist/compat.js" => "const BUILTIN_APIS = [];\n".to_owned(),
                "dist/modes/rpc/rpc-mode.js" if e.old.starts_with("import") => {
                    format!("{}\n", e.old)
                }
                "dist/modes/rpc/rpc-mode.js" => format!(
                    "export async function runRpcMode(session) {{\n{}\n}}\n",
                    e.old
                ),
                "dist/modes/interactive/interactive-mode.js" if e.old.starts_with("import") => {
                    format!("{}\n", e.old)
                }
                "dist/modes/interactive/interactive-mode.js" => {
                    format!("class I {{\n{}    }}\n}}\n", e.old)
                }
                "dist/core/agent-session.js" if e.old.starts_with("streamSimple") => {
                    format!("import {{ {} \n", e.old)
                }
                _ => format!(
                    "class S {{\n    async m(nextModel) {{\n{}\n        }});\n    }}\n}}\n",
                    e.old
                ),
            };
            match by_file.iter_mut().find(|(r, _)| *r == rel) {
                Some((_, text)) => text.push_str(&body),
                None => by_file.push((rel, body)),
            }
        }
        by_file
    }

    fn rewrite_file(rel: &str, text: &str) -> Result<String> {
        let edits = edits();
        let mine = edits
            .iter()
            .filter(|e| rel == format!("node_modules/{}/{}", e.package, e.file));
        rewrite(text, rel, mine)
    }

    #[test]
    fn every_edit_lands_on_its_own_lines() {
        for (rel, text) in files() {
            let out = rewrite_file(&rel, &text).unwrap();
            assert_eq!(out.lines().count(), text.lines().count(), "{rel}");
            assert!(
                out.contains("pibakePrewarm") || out.contains("\"warm\""),
                "{rel}: {out}"
            );
        }
    }

    #[test]
    fn hooks_pass_the_current_model() {
        let all: String = files()
            .iter()
            .map(|(rel, text)| rewrite_file(rel, text).unwrap())
            .collect();
        for hook in [
            "pibakePrewarm(() => session.model); return new Promise",
            "await this.init(); pibakePrewarm(() => this.session.model);",
            "pibakePrewarm(() => nextModel); await this._extensionRunner.emit",
            "import { pibakePrewarm } from \"@earendil-works/pi-ai/compat\"; import * as crypto",
            "import { pibakePrewarm, isRetryableAssistantError, } from",
            "streamSimple, pibakePrewarm, } from",
        ] {
            assert!(all.contains(hook), "{hook}");
        }
    }

    #[test]
    fn a_moved_text_fails_naming_it() {
        for (rel, text) in files() {
            let first = edits()
                .into_iter()
                .find(|e| rel == format!("node_modules/{}/{}", e.package, e.file))
                .unwrap();
            for changed in [text.replace(&first.old, ""), format!("{text}{text}")] {
                let err = rewrite_file(&rel, &changed).unwrap_err().to_string();
                assert!(
                    err.contains(&rel) && err.contains("prewarm_first_turn needs an update"),
                    "{err}"
                );
            }
        }
    }
}
