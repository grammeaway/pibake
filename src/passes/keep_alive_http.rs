//! `keep_alive_http`: Pi's undici dispatcher keeps an idle connection for `KEEP_ALIVE_MS`.
//!
//! Pi 1.3.0's `configureHttpDispatcher` sets no `keepAliveTimeout`, so undici closes an idle
//! socket after its default 4 s unless the server sends a `Keep-Alive: timeout=` hint, which
//! `api.anthropic.com` does not. A prompt after the user's think time then pays a new TCP and TLS
//! handshake. undici hands the option on through Pi's factories to each origin's `Pool` and
//! `Client`; a server's hint still wins. The options object is matched by its whole text.

use anyhow::{Result, bail};

use super::{Change, Sources, check_parses};
use crate::lock::PI_PACKAGE;

const FILE: &str = "dist/core/http-dispatcher.js";
/// Below every provider idle timeout seen (`api.anthropic.com`: over 20 min), and longer than a
/// user's usual think time.
const KEEP_ALIVE_MS: u32 = 120_000;

/// Pi 1.3.0's `EnvHttpProxyAgent` options.
const OPTIONS: &str = r#"new undici.EnvHttpProxyAgent({
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
    }));"#;
const LAST: &str = "factory: createUndiciOriginDispatcher,";

pub fn run(sources: &mut Sources) -> Result<Vec<Change>> {
    let rel = format!("node_modules/{PI_PACKAGE}/{FILE}");
    let new = rewrite(&sources.read(&rel)?, &rel)?;
    Ok(sources
        .replace(&rel, new)?
        .map(|removed| Change {
            file: rel,
            what: format!("keeps idle HTTP connections for {} s", KEEP_ALIVE_MS / 1000),
            bytes_removed: removed,
        })
        .into_iter()
        .collect())
}

/// `text` with the option added on the options' last line, so line numbers stay as they are.
pub fn with_keep_alive(text: &str) -> String {
    text.replacen(
        LAST,
        &format!("{LAST} keepAliveTimeout: {KEEP_ALIVE_MS},"),
        1,
    )
}

fn rewrite(text: &str, rel: &str) -> Result<String> {
    if text.matches(OPTIONS).count() != 1 {
        bail!(
            "{rel}: the `EnvHttpProxyAgent` options are not the ones pibake's keep_alive_http knows; it needs an update"
        );
    }
    let new = text.replacen(OPTIONS, &with_keep_alive(OPTIONS), 1);
    check_parses(&new, rel)?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> String {
        format!("function f() {{\n    const dispatcher = withUndiciErrorListener({OPTIONS}\n}}\n")
    }

    #[test]
    fn adds_the_option_on_the_last_line() {
        let out = rewrite(&file(), "h.js").unwrap();
        assert_eq!(out.lines().count(), file().lines().count());
        assert!(out.contains(
            "\n        factory: createUndiciOriginDispatcher, keepAliveTimeout: 120000,\n    }));\n"
        ));
        assert_eq!(out.replace(" keepAliveTimeout: 120000,", ""), file());
    }

    #[test]
    fn other_options_fail() {
        for changed in [
            file().replace("allowH2: false", "allowH2: true"),
            rewrite(&file(), "h.js").unwrap(),
            format!("{}{}", file(), file()),
            String::new(),
        ] {
            let err = rewrite(&changed, "h.js").unwrap_err().to_string();
            assert!(
                err.contains("h.js: the `EnvHttpProxyAgent` options are not"),
                "{err}"
            );
        }
    }
}
