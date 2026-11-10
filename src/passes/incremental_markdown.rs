//! `incremental_markdown`: a streamed answer is lexed and rendered per delta, not per character
//! of the whole text.
//!
//! pi-tui's `Markdown.render` lexes the whole text with marked and renders, wraps and pads every
//! block on each delta, and Pi's mermaid transformer lexes it once more before that: quadratic
//! in the answer's length. The pass sends both lexes and each block's rendering to
//! `pibake:markdown` (`bundle/markdown.mjs`), which keeps what an extended text leaves as it was.
//! The rendering moves into a function per block with stock's statements, in stock's order.
//! The block cache keys on Pi's theme global and on a flag the pass sets once highlight.js has
//! loaded every language, after which Pi redraws. Every site is matched by its whole text, and
//! so is the theme global: any change in Pi fails the pass.
//!
//! pi-tui 1.5.0 reuses a block's rendering itself, keyed on its source; there the pass sends only
//! the two lexes to `pibake:markdown`. pi-tui 1.5.1 then removes comment-only HTML blocks from
//! the tokens, which `pibake:markdown` allows for (see `lexMarkdown`).

use anyhow::{Result, bail};

use super::{Change, Sources, check_parses};
use crate::lock::PI_PACKAGE;

const PI_TUI: &str = "@earendil-works/pi-tui";
const MARKDOWN: &str = "dist/components/markdown.js";
const MERMAID: &str = "dist/modes/interactive/components/mermaid.js";
const THEME: &str = "dist/modes/interactive/theme/theme.js";
const SYNTAX: &str = "dist/utils/syntax-highlight.js";
const OURS: &str = "__pibake_lexMarkdown";
const MARKDOWN_IMPORT: &str = "import { lexMarkdown as __pibake_lexMarkdown, markdownBlock as __pibake_markdownBlock, markdownContext as __pibake_markdownContext } from \"pibake:markdown\"; ";
const LEX_IMPORT: &str =
    "import { lexMarkdown as __pibake_lexMarkdown } from \"pibake:markdown\"; ";

/// pi-tui 1.3.0's `Markdown.render`, from the lex to the padded content lines.
const RENDER: &str = r#"        // Parse markdown to HTML-like tokens
        const cached = this.cachedTokens?.deref();
        let tokens = cached?.source === normalizedText ? cached.tokens : undefined;
        if (!tokens) {
            tokens = markdownParser.lexer(normalizedText);
            trimPartialClosingFences(tokens);
            this.cachedTokens = new WeakRef({ source: normalizedText, tokens });
        }
        // Convert tokens to styled terminal output
        const renderedLines = [];
        for (let i = 0; i < tokens.length; i++) {
            const token = tokens[i];
            const nextToken = tokens[i + 1];
            const tokenLines = this.renderToken(token, contentWidth, nextToken?.type);
            for (const tokenLine of tokenLines) {
                renderedLines.push(tokenLine);
            }
        }
        // Wrap lines (NO padding, NO background yet)
        const wrappedLines = [];
        for (const line of renderedLines) {
            if (isImageLine(line)) {
                wrappedLines.push(line);
            }
            else {
                for (const wrappedLine of wrapTextWithAnsi(line, contentWidth)) {
                    wrappedLines.push(wrappedLine);
                }
            }
        }
        // Add margins and background to each wrapped line
        const leftMargin = " ".repeat(this.paddingX);
        const rightMargin = " ".repeat(this.paddingX);
        const bgFn = this.defaultTextStyle?.bgColor;
        const contentLines = [];
        for (const line of wrappedLines) {
            if (isImageLine(line)) {
                contentLines.push(line);
                continue;
            }
            const lineWithMargins = leftMargin + line + rightMargin;
            if (bgFn) {
                contentLines.push(applyBackgroundToLine(lineWithMargins, width, bgFn));
            }
            else {
                // No background - just pad to width
                const visibleLen = visibleWidth(lineWithMargins);
                const paddingNeeded = Math.max(0, width - visibleLen);
                contentLines.push(lineWithMargins + " ".repeat(paddingNeeded));
            }
        }
"#;

/// The same, per block through `pibake:markdown`; padded to `RENDER`'s line count.
const RENDER_OURS: &str = r#"        const tokens = __pibake_lexMarkdown(markdownParser, normalizedText, trimPartialClosingFences);
        const leftMargin = " ".repeat(this.paddingX);
        const rightMargin = " ".repeat(this.paddingX);
        const bgFn = this.defaultTextStyle?.bgColor;
        const context = __pibake_markdownContext(this, width, getCapabilities().hyperlinks);
        const contentLines = [];
        for (let i = 0; i < tokens.length; i++) {
            const token = tokens[i];
            const nextType = tokens[i + 1]?.type;
            const blockLines = __pibake_markdownBlock(token, nextType, context, () => {
                const lines = [];
                for (const tokenLine of this.renderToken(token, contentWidth, nextType)) {
                    const wrapped = isImageLine(tokenLine) ? [tokenLine] : wrapTextWithAnsi(tokenLine, contentWidth);
                    for (const line of wrapped) {
                        if (isImageLine(line)) {
                            lines.push(line);
                            continue;
                        }
                        const lineWithMargins = leftMargin + line + rightMargin;
                        if (bgFn) {
                            lines.push(applyBackgroundToLine(lineWithMargins, width, bgFn));
                        }
                        else {
                            const visibleLen = visibleWidth(lineWithMargins);
                            lines.push(lineWithMargins + " ".repeat(Math.max(0, width - visibleLen)));
                        }
                    }
                }
                return lines;
            });
            for (const line of blockLines) {
                contentLines.push(line);
            }
        }
"#;

/// pi-tui 1.5.0's lex in `Markdown.render`, up to where it starts reusing blocks.
const LEX: &str = r#"        if (!tokens) {
            tokens = markdownParser.lexer(normalizedText);
            trimPartialClosingFences(tokens);
            this.cachedTokens = new WeakRef({ source: normalizedText, tokens });
        }
        // A reference link resolves through a definition anywhere in the text, so blocks with the same source can render
        // differently; such texts are rare in practice and render without reusing blocks.
        const reuseBlocks = Object.keys(tokens.links).length === 0;
"#;
const LEX_OURS: &str = r#"        if (!tokens) {
            tokens = __pibake_lexMarkdown(markdownParser, normalizedText, trimPartialClosingFences);
            this.cachedTokens = new WeakRef({ source: normalizedText, tokens });
        }
        // A reference link resolves through a definition anywhere in the text, so blocks with the same source can render
        // differently; such texts are rare in practice and render without reusing blocks.
        const reuseBlocks = Object.keys(tokens.links).length === 0;
"#;
/// pi-tui 1.5.1's, the same but for `removeHtmlCommentBlocks`.
const LEX_1_5_1: &str = r#"        if (!tokens) {
            tokens = markdownParser.lexer(normalizedText);
            trimPartialClosingFences(tokens);
            removeHtmlCommentBlocks(tokens);
            this.cachedTokens = new WeakRef({ source: normalizedText, tokens });
        }
        // A reference link resolves through a definition anywhere in the text, so blocks with the same source can render
        // differently; such texts are rare in practice and render without reusing blocks.
        const reuseBlocks = Object.keys(tokens.links).length === 0;
"#;
const LEX_1_5_1_OURS: &str = r#"        if (!tokens) {
            tokens = __pibake_lexMarkdown(markdownParser, normalizedText, trimPartialClosingFences);
            removeHtmlCommentBlocks(tokens);
            this.cachedTokens = new WeakRef({ source: normalizedText, tokens });
        }
        // A reference link resolves through a definition anywhere in the text, so blocks with the same source can render
        // differently; such texts are rare in practice and render without reusing blocks.
        const reuseBlocks = Object.keys(tokens.links).length === 0;
"#;

/// Pi 1.3.0's mermaid transformer lexing the whole text.
const MERMAID_LEX: &str = "        return markdownParser\n            .lexer(markdown)\n";
const MERMAID_LEX_OURS: &str =
    "        return __pibake_lexMarkdown(markdownParser,\n            markdown)\n";

/// Pi 1.3.0's `loadAllHighlightLanguages` resolving; ours also sets the flag `markdownContext` reads.
const ALL_LANGUAGES: &str =
    "void import(\"highlight.js/lib/index.js\").then(() => resolve(), () => {";
const ALL_LANGUAGES_OURS: &str = "void import(\"highlight.js/lib/index.js\").then(() => { globalThis[Symbol.for(\"pibake:highlight-all\")] = true; resolve(); }, () => {";

/// Where Pi 1.3.0 keeps its theme, which `markdownContext` reads.
const THEME_GLOBAL: [&str; 2] = [
    "const THEME_KEY = Symbol.for(\"@earendil-works/pi-coding-agent:theme\");",
    "function setGlobalTheme(t) {\n    globalThis[THEME_KEY] = t;\n",
];

pub fn run(sources: &mut Sources) -> Result<Vec<Change>> {
    let pi = format!("node_modules/{PI_PACKAGE}");
    let markdown = format!("{}/{MARKDOWN}", sources.resolve_package(&pi, PI_TUI)?);
    let mermaid = format!("{pi}/{MERMAID}");
    let syntax = format!("{pi}/{SYNTAX}");
    const LEXES: &str = "lexes a streamed text from its last unchanged block";
    let mermaid_lex = (&mermaid, MERMAID_LEX, MERMAID_LEX_OURS, LEX_IMPORT, LEXES);
    let text = sources.read(&markdown)?;
    let rewrites = if text.contains(RENDER) {
        let theme = format!("{pi}/{THEME}");
        let text = sources.read(&theme)?;
        if let Some(missing) = THEME_GLOBAL.iter().find(|t| text.matches(*t).count() != 1) {
            bail!(
                "{theme}: no `{}`; Pi's theme global is not the one pibake's incremental_markdown knows; it needs an update",
                missing.lines().next().unwrap_or("")
            );
        }
        vec![
            (
                &markdown,
                RENDER,
                RENDER_OURS,
                MARKDOWN_IMPORT,
                "lexes and renders a streamed text from its last unchanged block",
            ),
            mermaid_lex,
            (
                &syntax,
                ALL_LANGUAGES,
                ALL_LANGUAGES_OURS,
                "",
                "flags when every highlight.js language has loaded",
            ),
        ]
    } else {
        // The newest when none matches, for `rewrite`'s error.
        let (lex, ours) = [(LEX, LEX_OURS), (LEX_1_5_1, LEX_1_5_1_OURS)]
            .into_iter()
            .rfind(|(lex, _)| text.contains(lex))
            .unwrap_or((LEX_1_5_1, LEX_1_5_1_OURS));
        vec![(&markdown, lex, ours, LEX_IMPORT, LEXES), mermaid_lex]
    };
    let mut changes = vec![];
    for (rel, stock, ours, import, what) in rewrites {
        let new = rewrite(&sources.read(rel)?, rel, stock, ours, import)?;
        if let Some(removed) = sources.replace(rel, new)? {
            changes.push(Change {
                file: rel.clone(),
                what: what.to_owned(),
                bytes_removed: removed,
            });
        }
    }
    Ok(changes)
}

/// `text` with its one `stock` replaced by `ours` padded to as many lines, and `import` on the
/// first line, so the file's line numbers stay as they are.
fn rewrite(text: &str, rel: &str, stock: &str, ours: &str, import: &str) -> Result<String> {
    if text.contains(OURS) {
        bail!("{rel} already calls {OURS}");
    }
    if text.matches(stock).count() != 1 {
        bail!(
            "{rel}: `{}` is not the code pibake's incremental_markdown knows; it needs an update",
            stock.lines().next().unwrap_or("").trim()
        );
    }
    let padding = stock.lines().count() - ours.lines().count();
    let new = format!(
        "{import}{}",
        text.replacen(stock, &format!("{ours}{}", "\n".repeat(padding)), 1)
    );
    check_parses(&new, rel)?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> String {
        format!(
            "import {{ x }} from \"./x.js\";\nclass M {{\n    render(width) {{\n{RENDER}        return contentLines;\n    }}\n}}\n"
        )
    }

    #[test]
    fn replaces_the_render_and_keeps_line_numbers() {
        let out = rewrite(&file(), "m.js", RENDER, RENDER_OURS, MARKDOWN_IMPORT).unwrap();
        assert_eq!(out.lines().count(), file().lines().count());
        assert!(out.starts_with(&format!("{MARKDOWN_IMPORT}import {{ x }}")));
        assert!(out.contains(RENDER_OURS));
        assert!(!out.contains("markdownParser.lexer("));
        assert!(
            out.ends_with("        return contentLines;\n    }\n}\n"),
            "{out}"
        );
        let err = rewrite(&out, "m.js", RENDER, RENDER_OURS, MARKDOWN_IMPORT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already calls"), "{err}");
    }

    #[test]
    fn the_mermaid_lex_keeps_line_numbers() {
        let text = format!(
            "const f = (markdown) => {{\n{MERMAID_LEX}            .map((t) => t.raw).join(\"\");\n}};\n"
        );
        let out = rewrite(&text, "m.js", MERMAID_LEX, MERMAID_LEX_OURS, LEX_IMPORT).unwrap();
        assert_eq!(out.lines().count(), text.lines().count());
        assert!(out.contains(
            "return __pibake_lexMarkdown(markdownParser,\n            markdown)\n            .map("
        ));
    }

    #[test]
    fn the_1_5_lexes_keep_line_numbers() {
        for (lex, ours) in [(LEX, LEX_OURS), (LEX_1_5_1, LEX_1_5_1_OURS)] {
            let text =
                format!("class M {{\n    render(width) {{\n{lex}        return x;\n    }}\n}}\n");
            let out = rewrite(&text, "m.js", lex, ours, LEX_IMPORT).unwrap();
            assert_eq!(out.lines().count(), text.lines().count());
            assert!(!out.contains("markdownParser.lexer("));
            assert!(out.contains(
                "tokens = __pibake_lexMarkdown(markdownParser, normalizedText, trimPartialClosingFences);\n"
            ));
            assert_eq!(
                out.contains("removeHtmlCommentBlocks(tokens);"),
                lex.contains("removeHtmlCommentBlocks(tokens);")
            );
        }
    }

    #[test]
    fn changed_or_repeated_code_fails() {
        let changed = file().replace("nextToken?.type);", "nextToken?.type, ctx);");
        let err = rewrite(&changed, "m.js", RENDER, RENDER_OURS, MARKDOWN_IMPORT)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("m.js: `// Parse markdown to HTML-like tokens` is not the code"),
            "{err}"
        );
        let twice = format!("{}{RENDER}", file());
        assert!(rewrite(&twice, "m.js", RENDER, RENDER_OURS, MARKDOWN_IMPORT).is_err());
    }
}
