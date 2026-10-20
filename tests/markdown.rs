//! `incremental_markdown`'s rewrite of pi-tui's `Markdown` against the installed stock one, on
//! streamed text rendered as Pi does: a new component per delta, with width and theme changes
//! between deltas. Needs `node` and the installed Pi, so the tests sit behind PIBAKE_E2E.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use hegel::TestCase;
use hegel::generators as gs;
use pibake::passes::{self, Options, Pass};
use serde_json::{Value, json};

const DRIVER: &str = r#"
// Serves the rewritten `markdown.js` as if it sat next to stock's, so both share pi-tui's other
// modules and marked.
import { registerHooks } from "node:module";
import { readFileSync } from "node:fs";
const dir = new URL("./", import.meta.url);
const { stock, ours } = JSON.parse(readFileSync(new URL("paths.json", dir), "utf8"));
const oursUrl = new URL("markdown.pibake.js", stock).href;
registerHooks({
	resolve(specifier, context, next) {
		if (specifier === "pibake:stock") return { url: stock, shortCircuit: true };
		if (specifier === "pibake:ours") return { url: oursUrl, shortCircuit: true };
		if (specifier === "pibake:markdown") return { url: new URL("markdown.mjs", dir).href, shortCircuit: true };
		if (specifier === "pibake:marked") return next("marked", { ...context, parentURL: stock });
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url === oursUrl) return { format: "module", source: readFileSync(ours, "utf8"), shortCircuit: true };
		return next(url, context);
	},
});
const { createInterface } = await import("node:readline");
const { isDeepStrictEqual } = await import("node:util");
const { Markdown: Stock } = await import("pibake:stock");
const { Markdown: Ours } = await import("pibake:ours");
const { lexMarkdown } = await import("pibake:markdown");
const { Marked } = await import("pibake:marked");
const { setCapabilityOverrides } = await import(new URL("../terminal-image.js", stock).href);

// Colours read from Pi's theme global, as Pi's own theme closures do. The last two themes differ
// only in the syntax colour, which only `highlightCode` shows.
const KEY = Symbol.for("@earendil-works/pi-coding-agent:theme");
const THEMES = [
	{ h: 31, l: 32, c: 33, s: 34, t: 37, bg: 41 },
	{ h: 35, l: 36, c: 37, s: 34, t: 37, bg: 42 },
	{ h: 35, l: 36, c: 37, s: 94, t: 37, bg: 42 },
];
// An extension's theme instead keeps its colours to itself, out of the global's reach.
let palette;
// The third and fourth palettes differ from the first only in a style's colour, the fifth from
// the fourth only in the theme's.
const PALETTES = [
	{ h: 91, l: 92, c: 93, s: 94, t: 90, bg: 101 },
	{ h: 95, l: 96, c: 97, s: 94, t: 90, bg: 102 },
	{ h: 91, l: 92, c: 93, s: 94, t: 90, bg: 103 },
	{ h: 91, l: 92, c: 93, s: 94, t: 97, bg: 101 },
	{ h: 95, l: 96, c: 97, s: 94, t: 97, bg: 101 },
];
let ext = false;
const g = () => (ext ? palette : globalThis[KEY]);
const fg = (k) => (t) => `\x1b[${g()[k]}m${t}\x1b[39m`;
const sgr = (on, off) => (t) => `\x1b[${on}m${t}\x1b[${off}m`;
const theme = (highlight, indent) => ({
	heading: fg("h"), link: fg("l"), linkUrl: fg("c"), code: fg("c"), codeBlock: fg("c"),
	codeBlockBorder: fg("h"), quote: fg("l"), quoteBorder: fg("h"), hr: fg("h"), listBullet: fg("l"),
	bold: sgr(1, 22), italic: sgr(3, 23), strikethrough: sgr(9, 29), underline: sgr(4, 24),
	...(highlight ? { highlightCode: (code) => code.split("\n").map(fg("s")) } : {}),
	...(indent ? { codeBlockIndent: "    " } : {}),
});
// A new object per component, as Pi makes it.
const style = (s) =>
	s && {
		...(s.color ? { color: fg("t") } : {}),
		...(s.bg ? { bgColor: (t) => `\x1b[${g().bg}m${t}\x1b[49m` } : {}),
		bold: s.bold,
		italic: s.italic,
		strikethrough: s.strike,
		underline: s.underline,
	};
const plain = new Marked();
const strip = (t) => JSON.parse(JSON.stringify(t));

for await (const line of createInterface({ input: process.stdin })) {
	const c = JSON.parse(line);
	let width = c.width;
	globalThis[KEY] = { ...THEMES[0] };
	palette = PALETTES[0];
	setCapabilityOverrides({ hyperlinks: c.hyperlinks });
	let reply = { ok: true, frames: 0 };
	const ends = [...c.cuts, c.text.length];
	const check = (f, src) => {
		ext = c.ext_theme;
		const options = { preserveOrderedListMarkers: c.ordered_markers, preserveBackslashEscapes: c.backslashes, renderLatex: c.latex };
		const make = (M) => new M(src, c.padding_x, c.padding_y, theme(c.highlight, c.indent), style(c.style), options);
		const stock = make(Stock).render(width);
		const ours = make(Ours).render(width);
		reply.frames++;
		if (!isDeepStrictEqual(stock, ours)) {
			const at = stock.findIndex((l, i) => l !== ours[i]);
			reply = { ok: false, frame: f, width, src, line: at, stock: stock.slice(Math.max(0, at - 2), at + 3), ours: ours.slice(Math.max(0, at - 2), at + 3) };
			return false;
		}
		if (!isDeepStrictEqual(strip(lexMarkdown(plain, src)), strip(plain.lexer(src)))) {
			reply = { ok: false, frame: f, src, lex: "the plain lex differs" };
			return false;
		}
		return true;
	};
	frames: for (let f = 0; f < ends.length; f++) {
		for (const [at, kind, v] of c.events) {
			if (at !== f) continue;
			if (kind === "width") width = v;
			if (kind === "theme") globalThis[KEY] = { ...THEMES[v] };
			if (kind === "palette") palette = PALETTES[v];
			if (kind === "set") Object.assign(c, v);
			if (kind === "hyperlinks") setCapabilityOverrides({ hyperlinks: v });
		}
		const src = c.trim ? c.text.slice(0, ends[f]).trim() : c.text.slice(0, ends[f]);
		if (c.trim && !src) continue;
		if (!check(f, src)) break;
		for (const [at, kind, v] of c.events) {
			if (at !== f || kind !== "again") continue;
			if (v >= 0) globalThis[KEY] = { ...THEMES[v] };
			if (!check(f, src)) break frames;
		}
	}
	process.stdout.write(JSON.stringify(reply) + "\n");
}
"#;

struct Driver {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One node process for every case of the test binary: the module caches carry over between
/// cases, as they do between messages.
fn driver() -> &'static Mutex<Driver> {
    static DRIVER_PROCESS: OnceLock<Mutex<Driver>> = OnceLock::new();
    DRIVER_PROCESS.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("markdown");
        std::fs::create_dir_all(&dir).unwrap();
        let (stock, ours) = rewritten_markdown();
        let ours_path = dir.join("markdown.pibake.js");
        std::fs::write(&ours_path, ours).unwrap();
        let paths = json!({
            "stock": format!("file://{}", stock.display()),
            "ours": ours_path,
        });
        std::fs::write(dir.join("paths.json"), paths.to_string()).unwrap();
        std::fs::write(dir.join("markdown.mjs"), pibake::bundle::MARKDOWN).unwrap();
        std::fs::write(dir.join("driver.mjs"), DRIVER).unwrap();
        let mut child = Command::new("node")
            .arg(dir.join("driver.mjs"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("node");
        Mutex::new(Driver {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            _child: child,
        })
    })
}

/// Stock `markdown.js` of the Pi on PATH, and the pass's rewrite of it.
fn rewritten_markdown() -> (PathBuf, String) {
    let pi = pibake::harness::PiCommand::parse("pi")
        .unwrap()
        .package_dir()
        .expect("pi on PATH");
    // The install root, whose node_modules holds Pi, read as a stage.
    let stage: &Path = pi.ancestors().nth(3).unwrap();
    let out = passes::run(
        stage,
        &Options {
            passes: vec![Pass::IncrementalMarkdown],
            ..Options::none()
        },
        &common::no_setup(stage),
    )
    .unwrap();
    let (rel, text) = out
        .overrides
        .into_iter()
        .find(|(rel, _)| rel.ends_with("pi-tui/dist/components/markdown.js"))
        .expect("the pass rewrites markdown.js");
    (stage.join(rel), text)
}

/// Renders the prefixes of `case["text"]` ending at its cuts with both; panics on the first
/// difference. Returns the frames compared.
fn stream(case: Value) -> u64 {
    let mut d = driver().lock().unwrap_or_else(|e| e.into_inner());
    // ASCII only: readline also ends lines at characters like U+2028.
    let mut line = String::new();
    for c in case.to_string().chars() {
        if c.is_ascii() {
            line.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                line.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    writeln!(d.stdin, "{line}").unwrap();
    let mut reply = String::new();
    d.stdout.read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).expect("a reply from the driver");
    assert!(reply["ok"] == true, "differs from stock: {reply:#}");
    reply["frames"].as_u64().unwrap()
}

const WORDS: [&str; 33] = [
    "foo",
    "bar",
    "baz",
    "日本語",
    "表",
    "x",
    "1.",
    "-",
    "*",
    "_",
    "#",
    "|",
    ">",
    "`",
    "$",
    "\\",
    "~",
    "a\tb",
    "😀",
    "[x]",
    "<b>",
    "]:",
    "a long word that wraps",
    "café",
    "a\r\nb",
    "<a href=x>",
    "</a>",
    "<code>",
    "</code>",
    "<pre>",
    "</pre>",
    "<kbd>",
    "<script>",
];

fn word(tc: &TestCase) -> String {
    if tc.draw(gs::integers::<u8>().max_value(15)) == 0 {
        return tc.draw(gs::text().max_size(8));
    }
    tc.draw(gs::sampled_from(WORDS.to_vec())).to_owned()
}

fn inline(tc: &TestCase) -> String {
    let parts: Vec<String> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(5)))
        .map(|_| {
            let w = word(tc);
            match tc.draw(gs::integers::<u8>().max_value(12)) {
                0 => format!("**{w}**"),
                1 => format!("*{w}*"),
                2 => format!("_{w}_"),
                3 => format!("`{w}`"),
                4 => format!("~~{w}~~"),
                5 => format!("[{w}](http://e.x/{w})"),
                6 => "[ref]".to_owned(),
                7 => "$x^2$".to_owned(),
                8 => "\\(a+b\\)".to_owned(),
                9 => "https://e.x/a".to_owned(),
                _ => w,
            }
        })
        .collect();
    parts.join(" ")
}

/// Every construct the lex map covers (`performance_plans/progress.md`, p8); `{}` is inline text.
const BLOCKS: [&str; 52] = [
    "{}",
    "{}\n{}",
    "# {}",
    "### {}",
    "{}\n===",
    "{}\n---",
    "{}\n-",
    "- {}\n- {}",
    "1. {}\n2. {}",
    "1) {}\n\n2) {}",
    "- [ ] {}\n- [x] {}",
    "- {}\n  - {}\n- {}",
    "- {}\n\n  {}",
    "-\n  {}",
    "1. {}\n   {}",
    "* {}\n{}",
    "- {}\n  ```\n  code\n  ```",
    "1. {}\n\n\n    {}",
    "> {}\n> {}",
    "> {}\n{}",
    "> > {}",
    "> - {}",
    "```\nlet x = 1;\n```",
    "```rust\nfn main() {}\n\n  x\n```",
    "~~~js\nconst a = 1;",
    "````python\nx\n```\n````",
    "```mermaid\ngraph TD; A-->B\n```",
    "```\n``",
    "    code\n    x = 1",
    "| a | b |\n|---|:-:|\n| {} | {} |",
    "| a |\n|---|\n{}",
    "---",
    "***",
    "- - -",
    "<div>\n{}\n</div>",
    "<!-- c -->",
    "<pre>\na\n\nb\n</pre>",
    "<details>\n<summary>x</summary>\n\n{}\n\n</details>",
    "$$\n\\frac{a}{b}\n$$",
    "$$ x^2 $$",
    "\\[\n\\sum x\n\\]",
    "$$\n\\int",
    "\\[ a",
    "{}\n1. {}",
    "{}\n- {}",
    "{}\n> {}",
    "{}\n```\n{}",
    "{}\n$$\n{}",
    "{}\n<div>\n{}",
    "[ref]: http://d.ef/x",
    "[foo]: http://d.ef/y \"t\"",
    "\t{}\tb",
];

fn document(tc: &TestCase) -> String {
    let mut text = String::new();
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(8)) {
        let mut block = tc.draw(gs::sampled_from(BLOCKS.to_vec())).to_owned();
        while let Some(at) = block.find("{}") {
            block.replace_range(at..at + 2, &inline(tc));
        }
        text.push_str(&block);
        text.push_str(tc.draw(gs::sampled_from(vec![
            "\n", "\n\n", "\n\n", "\n\n\n", "\n \n", "", "\r\n\r\n",
        ])));
    }
    text
}

fn cuts(tc: &TestCase, len: usize) -> Vec<usize> {
    if tc.draw(gs::booleans()) {
        let step = match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 | 1 => 1,
            _ => tc.draw(gs::integers::<usize>().min_value(2).max_value(24)),
        };
        return (step..len).step_by(step).collect();
    }
    let mut cuts: Vec<usize> = tc.draw(
        gs::vecs(
            gs::integers::<usize>()
                .min_value(1)
                .max_value(len.max(2) - 1),
        )
        .max_size(200),
    );
    cuts.sort_unstable();
    cuts.dedup();
    cuts.retain(|&c| c < len);
    cuts
}

/// One of the settings a Markdown component is made with, or the terminal's hyperlinks, drawn.
fn setting(tc: &TestCase, which: u8) -> (&'static str, Value) {
    let flag = |tc: &TestCase| json!(tc.draw(gs::booleans()));
    match which {
        0 => (
            "padding_x",
            json!(tc.draw(gs::integers::<u8>().max_value(2))),
        ),
        1 => (
            "padding_y",
            json!(tc.draw(gs::integers::<u8>().max_value(1))),
        ),
        2 => (
            "style",
            if tc.draw(gs::booleans()) {
                Value::Null
            } else {
                json!({
                    "color": tc.draw(gs::booleans()), "bg": tc.draw(gs::booleans()),
                    "bold": tc.draw(gs::booleans()), "italic": tc.draw(gs::booleans()),
                    "strike": tc.draw(gs::booleans()), "underline": tc.draw(gs::booleans()),
                })
            },
        ),
        3 => ("highlight", flag(tc)),
        4 => ("ext_theme", flag(tc)),
        5 => ("ordered_markers", flag(tc)),
        6 => ("backslashes", flag(tc)),
        7 => ("latex", flag(tc)),
        8 => ("indent", flag(tc)),
        _ => ("hyperlinks", flag(tc)),
    }
}

const SETTINGS: u8 = 10;

/// Any document of every construct, streamed in deltas of any size, rendered as an answer
/// (trimmed) or as thinking (not), with any settings, changed between deltas along with the
/// width, Pi's theme, an extension theme's palette and re-renders: every frame's lines equal
/// stock's.
#[hegel::test(test_cases = 400)]
fn e2e_every_frame_renders_as_stock(tc: TestCase) {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let text = document(&tc);
    let len = text.encode_utf16().count();
    let cuts = cuts(&tc, len);
    let frames = cuts.len() + 1;
    let events: Vec<Value> = (0..tc.draw(gs::integers::<usize>().max_value(4)))
        .map(|_| {
            let at = tc.draw(gs::integers::<usize>().max_value(frames - 1));
            match tc.draw(gs::integers::<u8>().max_value(4)) {
                0 => json!([
                    at,
                    "width",
                    tc.draw(gs::integers::<u32>().min_value(4).max_value(120))
                ]),
                1 => json!([at, "theme", tc.draw(gs::integers::<u8>().max_value(2))]),
                2 => json!([at, "palette", tc.draw(gs::integers::<u8>().max_value(4))]),
                3 => match setting(&tc, tc.draw(gs::integers::<u8>().max_value(SETTINGS - 1))) {
                    ("hyperlinks", v) => json!([at, "hyperlinks", v]),
                    (k, v) => json!([at, "set", { k: v }]),
                },
                _ => json!([
                    at,
                    "again",
                    tc.draw(gs::integers::<i8>().min_value(-1).max_value(2))
                ]),
            }
        })
        .collect();
    let mut case = json!({
        "text": text,
        "cuts": cuts,
        "trim": tc.draw(gs::booleans()),
        "width": tc.draw(gs::integers::<u32>().min_value(4).max_value(120)),
        "events": events,
    });
    for which in 0..SETTINGS {
        let (k, v) = setting(&tc, which);
        case[k] = v;
    }
    stream(case);
}

/// The bench's answer streamed in its 24-byte deltas, every frame compared, a resize midway.
#[test]
fn e2e_the_long_answer_renders_as_stock() {
    if !common::e2e_enabled() {
        return;
    }
    let text = pibake::fake_provider::markdown_answer(6 * 1024, "pibake-answer-end");
    let cuts: Vec<usize> = (24..text.encode_utf16().count()).step_by(24).collect();
    let middle = cuts.len() / 2;
    let frames = stream(json!({
        "text": text, "cuts": cuts, "trim": true, "width": 100, "padding_x": 1, "padding_y": 0,
        "style": null, "indent": false, "highlight": true, "ext_theme": false, "ordered_markers": false, "backslashes": false, "latex": true, "hyperlinks": false,
        "events": [[middle, "width", 60], [middle, "again", 1]],
    }));
    assert_eq!(frames as usize, cuts.len() + 2);
}

/// Documents where a construct reaches back over a blank line (the lex map's findings), streamed
/// a character at a time, trimmed and not.
#[test]
fn e2e_reaching_back_renders_as_stock() {
    if !common::e2e_enabled() {
        return;
    }
    let texts = [
        "1) `x` ~~a~~\n    _b_\n\n2) https://e.x/a\n\n3) c\n\nd\n\n4. e\n\n- f\n\n  g\n\n- h",
        "$$ x^2 $$\n\n$$\n\\int\n\n$$\n\\frac{a}{b}\n$$\n~~~js\n  indented\n~~~\n \n~~~rust\n\n~~~\n````rust\nlet x = 1;",
        "\\[\n\\sum x\n\\]\n\n$$\n\\int\n\n$$\n\\frac{a}{b}\n$$\n\n$$",
        "<a href=x> a\n\nb https://e.x/a\n\nc </a>\n\nd https://e.x/b\n\n<code> e\n\nf `g`",
        "a [ref] b and [foo]\n\npara\n\n[ref]: http://d.ef/x\n\nmore\n\n[foo]: http://d.ef/y \"t\"\n\nend",
        "> a\nb\n\n> c\n\nd\n===\n\ne\n\n---\n\n<pre>\nf\n\ng\n</pre>\n\nh",
    ];
    for text in texts {
        for trim in [true, false] {
            let cuts: Vec<usize> = (1..text.encode_utf16().count()).collect();
            stream(json!({
                "text": text, "cuts": cuts, "trim": trim, "width": 40, "padding_x": 1,
                "padding_y": 0, "style": null, "indent": false, "highlight": true, "ext_theme": false,
                "ordered_markers": false, "backslashes": false, "latex": true, "hyperlinks": false,
                "events": [],
            }));
        }
    }
}

/// A document that shows every setting, streamed while each setting changes in turn: a block
/// rendered before a change and kept after it renders as stock does after it.
#[test]
fn e2e_settings_changed_mid_stream_render_as_stock() {
    if !common::e2e_enabled() {
        return;
    }
    let section = "## Head `code`\n\nText **bold** *it* ~~del~~ \\* [link](https://e.x/a) $x^2$ a long line that has to wrap somewhere\n\n3. three\n3. four\n\n```rust\nfn main() {}\n```\n\n$$\nx^2\n$$\n\n> quote\n\n| a | b |\n|---|---|\n| c | d |\n\n";
    let text = section.repeat(4);
    let cuts: Vec<usize> = (7..text.encode_utf16().count()).step_by(7).collect();
    let style = |flags: &[&str]| {
        let mut s = json!({ "color": true });
        for f in flags {
            s[*f] = json!(true);
        }
        json!(["set", { "style": s }])
    };
    // Each changes one thing the block lines depend on.
    let changes = [
        json!(["set", { "padding_x": 2 }]),
        style(&[]),
        json!(["palette", 3]),
        json!(["palette", 4]),
        style(&["italic"]),
        style(&["italic", "bold"]),
        style(&["italic", "bold", "strike"]),
        style(&["italic", "bold", "strike", "underline"]),
        style(&["italic", "bold", "strike", "underline", "bg"]),
        json!(["palette", 2]),
        json!(["set", { "highlight": false }]),
        json!(["set", { "indent": true }]),
        json!(["set", { "ordered_markers": true }]),
        json!(["set", { "backslashes": true }]),
        json!(["set", { "latex": false }]),
        json!(["hyperlinks", true]),
        json!(["set", { "padding_y": 1 }]),
        json!(["set", { "ext_theme": false }]),
        json!(["set", { "highlight": true }]),
        json!(["theme", 1]),
        json!(["theme", 2]),
    ];
    // From a quarter in, where earlier sections are kept blocks.
    let start = cuts.len() / 4;
    let step = (cuts.len() - start) / (changes.len() + 1);
    let events: Vec<Value> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| json!([start + (i + 1) * step, c[0], c[1]]))
        .collect();
    stream(json!({
        "text": text, "cuts": cuts, "trim": true, "width": 50, "padding_x": 0, "padding_y": 0,
        "style": null, "indent": false, "highlight": true, "ext_theme": true, "ordered_markers": false,
        "backslashes": false, "latex": true, "hyperlinks": false, "events": events,
    }));
}
