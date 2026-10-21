//! `lazy_http_dispatcher`: Pi's undici dispatcher is made on first use, not at startup.
//!
//! Pi 1.3.0's `configureHttpDispatcher` creates an `EnvHttpProxyAgent`, makes it undici's global
//! dispatcher and installs undici's `fetch` and web classes on `globalThis`, three or four times
//! before startup ends, where nothing fetches yet. The pass renames it and exports a stand-in
//! that hands it to `pibake:http-dispatcher` (`bundle/http_dispatcher.mjs`) with the names
//! undici's `install()` sets, read from undici here. The function is matched by its whole text:
//! any change in Pi fails the pass.

use anyhow::{Context, Result, bail};

use super::{Change, Sources, check_parses, keep_alive_http};
use crate::lock::PI_PACKAGE;

const FILE: &str = "dist/core/http-dispatcher.js";
const DECLARATION: &str =
    "export function configureHttpDispatcher(timeoutMs = DEFAULT_HTTP_IDLE_TIMEOUT_MS) {";
const STOCK: &str = "__pibake_configureHttpDispatcher";
const IMPORT: &str = "import { deferHttpDispatcher as __pibake_deferHttpDispatcher } from \"pibake:http-dispatcher\"; ";

/// Pi 1.3.0's function after `DECLARATION`.
const BODY: &str = r#"
    const normalizedTimeoutMs = parseHttpIdleTimeoutMs(timeoutMs);
    if (normalizedTimeoutMs === undefined) {
        throw new Error(`Invalid HTTP idle timeout: ${String(timeoutMs)}`);
    }
    const dispatcher = withUndiciErrorListener(new undici.EnvHttpProxyAgent({
        allowH2: false,
        // Keep HTTP origins on CONNECT tunnels as they were before Undici 8.7.
        proxyTunnel: true,
        bodyTimeout: normalizedTimeoutMs,
        connect: {
            autoSelectFamilyAttemptTimeout: DEFAULT_AUTO_SELECT_FAMILY_ATTEMPT_TIMEOUT_MS,
        },
        headersTimeout: normalizedTimeoutMs,
        clientFactory: createUndiciClient,
        factory: createUndiciOriginDispatcher,
    }));
    undici.setGlobalDispatcher(dispatcher);
    // Keep fetch and the dispatcher on the same undici implementation. Node 26.0's
    // bundled fetch can otherwise consume compressed responses through npm undici's
    // dispatcher without decompressing them, causing response.json() failures.
    // If a caller replaced fetch after module load, preserve that deliberate override.
    const shouldInstallGlobals = installedGlobalFetch === undefined
        ? globalThis.fetch === originalGlobalFetch
        : globalThis.fetch === installedGlobalFetch;
    if (shouldInstallGlobals) {
        undici.install?.();
        installedGlobalFetch = globalThis.fetch;
    }
}"#;

pub fn run(sources: &mut Sources) -> Result<Vec<Change>> {
    let pi = format!("node_modules/{PI_PACKAGE}");
    let rel = format!("{pi}/{FILE}");
    let undici = sources.resolve_package(&format!("{pi}/dist/core"), "undici")?;
    let installed = format!("{undici}/index.js");
    let globals = install_globals(&sources.read(&installed)?)
        .with_context(|| format!("reading undici's install() in {installed}"))?;
    let new = rewrite(&sources.read(&rel)?, &rel, &globals)?;
    Ok(sources
        .replace(&rel, new)?
        .map(|removed| Change {
            file: rel,
            what: format!(
                "makes the HTTP dispatcher on first use of {}, or of undici",
                globals.join(", ")
            ),
            bytes_removed: removed,
        })
        .into_iter()
        .collect())
}

/// The globals undici's `install()` sets, each as `globalThis.X = module.exports.X`.
fn install_globals(text: &str) -> Result<Vec<String>> {
    let start = text
        .find("\nfunction install () {\n")
        .context("no `function install ()`")?;
    let body = &text[start..];
    let body = &body[body.find("{\n").unwrap() + 2..];
    let body = &body[..body.find("\n}\n").context("`install` does not end")?];
    body.lines()
        .map(|line| {
            let (name, value) = line
                .trim()
                .strip_prefix("globalThis.")
                .and_then(|l| l.split_once(" = "))
                .with_context(|| format!("`{}` is not a global assignment", line.trim()))?;
            if value != format!("module.exports.{name}") {
                bail!("`{}` does not install undici's own {name}", line.trim());
            }
            Ok(name.to_owned())
        })
        .collect()
}

/// Stock's function renamed, followed on its last line by the exported stand-in, so the file's
/// line numbers stay as they are.
fn rewrite(text: &str, rel: &str, globals: &[String]) -> Result<String> {
    if text.contains(STOCK) {
        bail!("{rel} already defines {STOCK}");
    }
    let plain = format!("{DECLARATION}{BODY}");
    // `keep_alive_http` may have run first.
    let Some(stock) = [keep_alive_http::with_keep_alive(&plain), plain]
        .into_iter()
        .find(|stock| text.matches(stock.as_str()).count() == 1)
    else {
        bail!(
            "{rel}: `configureHttpDispatcher` is not the one pibake's lazy_http_dispatcher knows; it needs an update"
        );
    };
    let renamed = stock.replacen(
        "export function configureHttpDispatcher(",
        &format!("function {STOCK}("),
        1,
    );
    let stand_in = format!(
        " {DECLARATION} if (parseHttpIdleTimeoutMs(timeoutMs) === undefined) return {STOCK}(timeoutMs); __pibake_deferHttpDispatcher({STOCK}, timeoutMs, {}); }}",
        serde_json::to_string(globals)?
    );
    let new = format!(
        "{IMPORT}{}",
        text.replacen(&stock, &format!("{renamed}{stand_in}"), 1)
    );
    check_parses(&new, rel)?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> String {
        format!("const a = 1;\n{DECLARATION}{BODY}\nexport const b = 2;\n")
    }

    const INSTALL: &str = "module.exports.x = 1\n\nfunction install () {\n  globalThis.fetch = module.exports.fetch\n  globalThis.EventSource = module.exports.EventSource\n}\n\nmodule.exports.install = install\n";

    #[test]
    fn renames_stock_and_exports_the_stand_in_on_its_last_line() {
        let globals = ["fetch".to_owned(), "Headers".to_owned()];
        let out = rewrite(&file(), "h.js", &globals).unwrap();
        assert_eq!(out.lines().count(), file().lines().count());
        assert!(out.starts_with(IMPORT));
        assert!(out.contains(&format!("\nfunction {STOCK}(timeoutMs = DEFAULT_HTTP_IDLE_TIMEOUT_MS) {{\n    const normalizedTimeoutMs")));
        assert!(out.contains(&format!("\n}} {DECLARATION} if (parseHttpIdleTimeoutMs(timeoutMs) === undefined) return {STOCK}(timeoutMs); __pibake_deferHttpDispatcher({STOCK}, timeoutMs, [\"fetch\",\"Headers\"]); }}\nexport const b = 2;\n")), "{out}");
        assert_eq!(
            out.matches("export function configureHttpDispatcher(")
                .count(),
            1
        );
        let err = rewrite(&out, "h.js", &globals).unwrap_err().to_string();
        assert!(err.contains("already defines"), "{err}");
    }

    #[test]
    fn a_changed_function_fails() {
        let changed = file().replace("allowH2: false", "allowH2: true");
        let err = rewrite(&changed, "h.js", &[]).unwrap_err().to_string();
        assert!(
            err.contains("h.js: `configureHttpDispatcher` is not the one"),
            "{err}"
        );
        let twice = format!("{}{DECLARATION}{BODY}\n", file());
        assert!(rewrite(&twice, "h.js", &[]).is_err());
    }

    #[test]
    fn keeps_keep_alive_http_when_it_ran_first() {
        let kept = keep_alive_http::with_keep_alive(&file());
        assert_ne!(kept, file());
        let out = rewrite(&kept, "h.js", &[]).unwrap();
        assert_eq!(
            out,
            keep_alive_http::with_keep_alive(&rewrite(&file(), "h.js", &[]).unwrap())
        );
    }

    #[test]
    fn reads_the_globals_install_sets() {
        assert_eq!(install_globals(INSTALL).unwrap(), ["fetch", "EventSource"]);
        let other = INSTALL.replace("module.exports.fetch", "myFetch");
        let err = format!("{:#}", install_globals(&other).unwrap_err());
        assert!(err.contains("does not install undici's own fetch"), "{err}");
        let extra = INSTALL.replace("  globalThis.fetch", "  setup()\n  globalThis.fetch");
        let err = format!("{:#}", install_globals(&extra).unwrap_err());
        assert!(
            err.contains("`setup()` is not a global assignment"),
            "{err}"
        );
        assert!(install_globals("module.exports = {}\n").is_err());
    }
}
