//! `pibake:tool-json` against pi-ai's own `parseStreamingJson`, the function it stands in for.
//! Needs `node` and the installed Pi, so the tests sit behind PIBAKE_E2E.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use hegel::TestCase;
use hegel::generators as gs;
use serde_json::{Value, json};

const DRIVER: &str = r#"
import { createInterface } from "node:readline";
import { isDeepStrictEqual } from "node:util";
import { parseStreamingJson } from "./json-parse.js";
import { streamToolJson } from "./tool_json.mjs";

const run = (f) => { try { return { value: f() }; } catch (e) { return { error: String(e) }; } };
for await (const line of createInterface({ input: process.stdin })) {
	const { text, cuts, rescans, every } = JSON.parse(line);
	const key = {};
	const stocked = [];
	let reply = { ok: true, stocked };
	let buf = "";
	let calls = 0;
	const counted = (t) => (calls++, parseStreamingJson(t));
	const ends = [...cuts, text.length];
	for (let i = 0, at = 0; i < ends.length; at = ends[i++]) {
		const delta = text.slice(at, ends[i]);
		buf += delta;
		const before = calls;
		const last = i === ends.length - 1;
		const stock = every || last ? run(() => parseStreamingJson(buf)) : undefined;
		const ours = run(() => streamToolJson(counted, key, buf, rescans.includes(i) ? null : delta));
		stocked.push(calls > before);
		if (stock && (stock.error !== ours.error || !isDeepStrictEqual(stock.value, ours.value))) {
			reply = { ok: false, at: i, buf, stock, ours };
			break;
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

/// One node process for every case of the test binary.
fn driver() -> &'static Mutex<Driver> {
    static DRIVER_PROCESS: OnceLock<Mutex<Driver>> = OnceLock::new();
    DRIVER_PROCESS.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tool_json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool_json.mjs"), pibake::bundle::TOOL_JSON).unwrap();
        std::fs::write(dir.join("driver.mjs"), DRIVER).unwrap();
        // json-parse.js imports partial-json, so it is linked where it can resolve it.
        let link = dir.join("json-parse.js");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(stock_json_parse(), &link).unwrap();
        let mut child = Command::new("node")
            .arg("--preserve-symlinks-main")
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

/// pi-ai's `dist/utils/json-parse.js` in the Pi on PATH.
fn stock_json_parse() -> PathBuf {
    let pi = pibake::harness::PiCommand::parse("pi")
        .unwrap()
        .package_dir()
        .expect("pi on PATH");
    pi.ancestors()
        .map(|d| d.join("node_modules/@earendil-works/pi-ai/dist/utils/json-parse.js"))
        .find(|p| p.is_file())
        .expect("pi-ai's json-parse.js next to Pi")
}

/// Streams `text` cut at the UTF-16 offsets `cuts`, passing no delta at the indices `rescans`;
/// panics on the first delta whose result differs from stock (only the last one unless `every`).
/// Returns, per delta, whether the incremental parser called stock.
fn stream(text: &str, cuts: &[usize], rescans: &[usize], every: bool) -> Vec<bool> {
    let mut d = driver().lock().unwrap_or_else(|e| e.into_inner());
    let line =
        json!({ "text": text, "cuts": cuts, "rescans": rescans, "every": every }).to_string();
    // ASCII only: readline also ends lines at characters like U+2028.
    let mut ascii = String::with_capacity(line.len());
    for c in line.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    writeln!(d.stdin, "{ascii}").unwrap();
    let mut reply = String::new();
    d.stdout.read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).expect("a reply from the driver");
    assert!(reply["ok"] == true, "differs from stock: {reply:#}");
    serde_json::from_value(reply["stocked"].clone()).unwrap()
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Raw characters and escapes that are valid inside a JSON string, including the whitespace
/// `String.prototype.trim` drops and surrogates written as escapes.
const STRING_PARTS: [&str; 30] = [
    "a",
    "Z",
    "0",
    " ",
    "  ",
    "\u{a0}",
    "\u{2028}",
    "\u{3000}",
    "\u{feff}",
    "é",
    "😀",
    "{",
    "}",
    "[",
    "]",
    ",",
    ":",
    "\\\"",
    "\\\\",
    "\\/",
    "\\b",
    "\\f",
    "\\n",
    "\\r",
    "\\t",
    "\\u00e9",
    "\\u0020",
    "\\ud83d\\ude00",
    "\\ud83d",
    "\\uDE00",
];

fn ws(tc: &TestCase) -> &'static str {
    tc.draw(gs::sampled_from(vec![
        "", "", "", " ", "\n", "\t", "\r\n", "  ",
    ]))
}

fn string_literal(tc: &TestCase, max_parts: usize) -> String {
    let mut s = String::from("\"");
    for _ in 0..tc.draw(gs::integers::<usize>().max_value(max_parts)) {
        if tc.draw(gs::integers::<u8>().max_value(9)) == 0 {
            let text = tc.draw(gs::text().max_size(6));
            let quoted = serde_json::to_string(&text).unwrap();
            s.push_str(&quoted[1..quoted.len() - 1]);
        } else {
            s.push_str(tc.draw(gs::sampled_from(STRING_PARTS.to_vec())));
        }
    }
    s.push('"');
    s
}

/// `quirks`: also `[ ]`, which stock misparses (see `tool_json.mjs`).
fn value(tc: &TestCase, depth: usize, quirks: bool) -> String {
    let kind = tc.draw(gs::integers::<u8>().max_value(if depth >= 3 { 2 } else { 4 }));
    match kind {
        0 => string_literal(tc, 12),
        1 => tc.draw(gs::from_regex(
            r"-?(0|[1-9][0-9]{0,3})(\.[0-9]{1,3})?([eE][+-]?[0-9]{1,2})?",
        )),
        2 => tc
            .draw(gs::sampled_from(vec!["true", "false", "null"]))
            .to_owned(),
        3 => {
            let items: Vec<String> = (0..tc.draw(gs::integers::<usize>().max_value(3)))
                .map(|_| format!("{}{}{}", ws(tc), value(tc, depth + 1, quirks), ws(tc)))
                .collect();
            if items.is_empty() && !quirks {
                return "[]".to_owned();
            }
            format!("[{}{}]", items.join(","), ws(tc))
        }
        _ => object(tc, depth, quirks, None),
    }
}

/// An object of random members; `last` (a key and a string literal) is written last.
fn object(tc: &TestCase, depth: usize, quirks: bool, last: Option<(&str, &str)>) -> String {
    let mut members: Vec<String> = (0..tc.draw(gs::integers::<usize>().max_value(3)))
        .map(|_| {
            let key = if tc.draw(gs::integers::<u8>().max_value(5)) == 0 {
                "\"a\"".to_owned()
            } else {
                string_literal(tc, 4)
            };
            format!(
                "{}{key}{}:{}{}{}",
                ws(tc),
                ws(tc),
                ws(tc),
                value(tc, depth + 1, quirks),
                ws(tc)
            )
        })
        .collect();
    if let Some((key, string)) = last {
        members.push(format!("{}{key}:{string}", ws(tc)));
    }
    format!("{{{}{}}}", members.join(","), ws(tc))
}

/// Text a model might send that stock repairs, rejects or parses unlike strict JSON.
const DAMAGE: [&str; 15] = [
    "\n",
    "\t",
    "\u{1}",
    "\\x",
    "\\u12",
    "\\",
    "\"",
    "}",
    ",,",
    "01",
    "tru",
    "NaN",
    "\"__proto__\": {\"p\": 1}, ",
    "\u{feff}",
    "x",
];

fn cuts(tc: &TestCase, text: &str) -> Vec<usize> {
    let len = utf16_len(text);
    if len < 2 {
        return vec![];
    }
    let mut cuts: Vec<usize> = tc.draw(
        gs::vecs(gs::integers::<usize>().min_value(1).max_value(len - 1)).max_size(len.min(400)),
    );
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

/// Any text, split anywhere (inside surrogate pairs too), some deltas passed as a rescan: every
/// intermediate result and every error equals stock's on the same prefix.
#[hegel::test(test_cases = 500)]
fn e2e_every_delta_equals_stock(tc: TestCase) {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let root = match tc.draw(gs::integers::<u8>().max_value(9)) {
        0 => value(&tc, 0, true),
        1 => {
            let items: Vec<String> = (0..tc.draw(gs::integers::<usize>().max_value(3)))
                .map(|_| value(&tc, 1, true))
                .collect();
            format!("[{}]", items.join(","))
        }
        _ => object(&tc, 0, true, None),
    };
    let mut text = format!("{}{root}{}", ws(&tc), ws(&tc));
    if tc.draw(gs::booleans()) {
        let bounds: Vec<usize> = (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .collect();
        let at = bounds[tc.draw(gs::integers::<usize>().max_value(bounds.len() - 1))];
        text.insert_str(at, tc.draw(gs::sampled_from(DAMAGE.to_vec())));
    }
    let cuts = cuts(&tc, &text);
    let rescans: Vec<usize> =
        tc.draw(gs::vecs(gs::integers::<usize>().max_value(cuts.len())).max_size(3));
    stream(&text, &cuts, &rescans, true);
}

/// A clean object whose last member is a long string: equal to stock, and of the deltas that end
/// inside that string only the first calls stock.
#[hegel::test(test_cases = 100)]
fn e2e_a_streamed_string_calls_stock_once(tc: TestCase) {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let mut long = String::from("\"");
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(20).max_value(300)) {
        long.push_str(tc.draw(gs::sampled_from(STRING_PARTS.to_vec())));
    }
    long.push('"');
    let key = string_literal(&tc, 3);
    let text = object(&tc, 0, false, Some((&key, &long)));
    let open = utf16_len(&text[..text.rfind(&long).unwrap()]);
    let close = open + utf16_len(&long) - 1;
    let cuts = cuts(&tc, &text);
    let stocked = stream(&text, &cuts, &[], true);
    let ends = cuts.iter().copied().chain([utf16_len(&text)]);
    let inside: Vec<bool> = ends
        .zip(&stocked)
        .filter(|(end, _)| (open + 1..=close).contains(end))
        .map(|(_, s)| *s)
        .collect();
    if let Some((first, rest)) = inside.split_first() {
        assert!(*first, "entering the string calls stock");
        assert!(
            rest.iter().all(|s| !s),
            "stock called again inside the string: {stocked:?}, text {text:?}, cuts {cuts:?}"
        );
    }
}

/// The bench's shape: a 200 KB `write` in 20-byte deltas calls stock a handful of times. Stock
/// on every prefix would take most of a minute, so only the final result is compared.
#[test]
fn e2e_a_big_write_calls_stock_a_handful_of_times() {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let content = pibake::fake_provider::big_write_content(pibake::fake_provider::BIG_WRITE_BYTES);
    let text = json!({ "path": "big_write.txt", "content": content }).to_string();
    let cuts: Vec<usize> = (1..text.len()).step_by(20).skip(1).collect();
    let stocked = stream(&text, &cuts, &[], false);
    let calls = stocked.iter().filter(|s| **s).count();
    assert!(
        calls <= 4,
        "{calls} stock calls over {} deltas",
        stocked.len()
    );
}
