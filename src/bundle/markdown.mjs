// pibake:markdown: pi-tui's `Markdown.render` and Pi's mermaid transformer lex and render only what
// a streamed delta changed (pass incremental_markdown, src/passes/incremental_markdown.rs). Every
// result equals stock's.
//
// Lexing: when the text extends one lexed recently, the tokens before a cut are kept and the rest
// is lexed from there. A cut is a blank line before a block that another token follows, so that
// block's first line is complete, and whether it continues what precedes the blank line was
// settled when it was lexed. Fuzzed against full lexes (performance_plans/progress.md, p8), the
// only constructs that reach over a cut are link reference definitions (`]:`), a pending `$$`
// line and an open inline `<a `, `<pre>`, `<code>`, `<kbd>` or `<script>`, so those lex in full.
// Rendering: a block's lines depend only on the block, the next block's type and the render
// context, so a kept block reuses its lines while those stay the same.
// shortcut: a list grows as one block, re-lexed and re-rendered per delta; cut inside lists if
// answers made of one long list show up slow.

// Each stream's latest text, most recent first. One entry per Markdown being streamed and per
// parser: the answer, its thinking, the mermaid transformer's own lex.
const RECENT = 8;
const recent = []; // { parser, source, tokens }

/** `parser.lexer(source)`, then `trim(tokens)` if given; the tokens must not be mutated. */
export function lexMarkdown(parser, source, trim) {
	const exact = recent.findIndex((e) => e.parser === parser && e.source === source);
	if (exact >= 0) {
		const [e] = recent.splice(exact, 1);
		recent.unshift(e);
		return e.tokens;
	}
	let tokens;
	const prefix = recent.findIndex((e) => e.parser === parser && source.startsWith(e.source));
	if (prefix >= 0) {
		tokens = extend(parser, recent[prefix].tokens, source);
		recent.splice(prefix, 1);
	}
	tokens ??= parser.lexer(source);
	// Mutates the last token only, which `extend` never keeps.
	trim?.(tokens);
	recent.unshift({ parser, source, tokens });
	if (recent.length > RECENT) recent.length = RECENT;
	return tokens;
}

// A line opening display math that the LaTeX extension has not taken as a block: a later `$$`
// can still close it, over blank lines.
const PENDING_DOLLARS = /(?:^|\n)[ \t>]*(?:(?:[-*+]|\d{1,9}[.)])[ \t]+[ \t>]*)?\$\$/;
const pendingDollars = (t) => t.type !== "code" && t.type !== "latexBlock" && PENDING_DOLLARS.test(t.raw);
// marked's inline lexer carries `inLink` and `inRawBlock` from block to block. Only these tags set
// them, and each inline html token records them as they stand after it.
const STATE_TAG = /<(?:a |(?:pre|code|kbd|script)[\s>])/i;
const setsState = (t) =>
	(t.type === "html" && t.block === false && (t.inLink || t.inRawBlock)) ||
	[t.tokens, t.items, t.header, ...(t.rows ?? [])].some((c) => Array.isArray(c) && c.some(setsState));

function extend(parser, old, source) {
	if (source.includes("]:") || source.includes("\r")) return undefined;
	let k = old.length - 2;
	while (k >= 1 && old[k - 1].type !== "space") k--;
	if (k < 1) return undefined;
	const kept = old.slice(0, k);
	if (source.includes("$$") && kept.some(pendingDollars)) return undefined;
	if (STATE_TAG.test(source) && kept.some(setsState)) return undefined;
	let offset = 0;
	for (const t of kept) offset += t.raw.length;
	const tokens = kept.concat(parser.lexer(source.slice(offset)));
	// marked can merge a continuation into a token with a `\n` the source lacks.
	let length = 0;
	for (const t of tokens) length += t.raw.length;
	return length === source.length ? tokens : undefined;
}

const rendered = new WeakMap(); // token -> { next, context, lines }

/** `render()`, the lines of `token` before `next`, or what it returned for the same arguments. */
export function markdownBlock(token, next, context, render) {
	const r = rendered.get(token);
	if (r !== undefined && r.next === next && r.context === context) return r.lines;
	const lines = render();
	rendered.set(token, { next, context, lines });
	return lines;
}

// Pi's theme object, replaced whole on each change. Its colours reach a block through closures
// (`highlightCode` above all) that a probe below cannot see.
const PI_THEME = Symbol.for("@earendil-works/pi-coding-agent:theme");
const themeIds = new WeakMap();
let lastThemeId = 0;
const STYLES = ["heading", "link", "linkUrl", "code", "codeBlock", "codeBlockBorder", "quote", "quoteBorder", "hr", "listBullet", "bold", "italic", "strikethrough", "underline"];
const probe = (f) => (typeof f === "function" ? f("\0") : "");

/** Everything besides the tokens that `markdown`'s block lines depend on, as a string. */
export function markdownContext(markdown, width, hyperlinks) {
	const { theme, defaultTextStyle: style, options } = markdown;
	const pi = globalThis[PI_THEME];
	let piTheme = 0;
	if (typeof pi === "object" && pi !== null) {
		piTheme = themeIds.get(pi);
		if (piTheme === undefined) themeIds.set(pi, (piTheme = ++lastThemeId));
	}
	return [
		width,
		markdown.paddingX,
		hyperlinks,
		piTheme,
		options.renderLatex !== false,
		!!options.preserveBackslashEscapes,
		!!options.preserveOrderedListMarkers,
		typeof theme.highlightCode,
		theme.codeBlockIndent,
		...STYLES.map((k) => probe(theme[k])),
		probe(style?.color),
		probe(style?.bgColor),
		!!style?.bold,
		!!style?.italic,
		!!style?.strikethrough,
		!!style?.underline,
	].join("\u0001");
}
