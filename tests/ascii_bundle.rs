//! `ascii_bundle`'s regex escapes against V8: the original and the rewritten literal match the
//! same strings in every flag mode. Needs `node`, so the test sits behind PIBAKE_E2E.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use hegel::TestCase;
use hegel::generators as gs;
use oxc::span::SourceType;
use serde_json::{Value, json};

const DRIVER: &str = r#"
import { createInterface } from "node:readline";

const compile = (literal) => new Function(`return ${literal};`)();
const exec = (re, input) => {
	const m = re.exec(input);
	return m && JSON.stringify({ m: [...m], index: m.index, groups: m.groups });
};
for await (const line of createInterface({ input: process.stdin })) {
	const { original, rewritten, inputs } = JSON.parse(line);
	let reply = { ok: true };
	let re;
	try {
		re = compile(original);
	} catch {
		reply = { skip: true };
	}
	if (re) {
		try {
			const ours = compile(rewritten);
			for (const units of inputs) {
				const input = String.fromCharCode(...units);
				const [a, b] = [exec(re, input), exec(ours, input)];
				if (a !== b || re.flags !== ours.flags) {
					reply = { ok: false, input: units, original: a, rewritten: b };
					break;
				}
			}
		} catch (e) {
			reply = { ok: false, error: String(e) };
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
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ascii_bundle");
        std::fs::create_dir_all(&dir).unwrap();
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

/// ASCII, BMP (`ſ` and the Kelvin sign case-fold to ASCII under `iu`) and astral characters.
const CHARS: [&str; 12] = [
    "a", "s", "k", "-", "é", "–", "中", "ſ", "\u{212A}", "»", "😀", "𝒳",
];
/// Group names: `𝒳` is astral, so a non-`u` escape writes it as a surrogate pair.
const NAMES: [&str; 4] = ["n", "é", "中", "𝒳"];

fn char_(tc: &TestCase) -> &'static str {
    tc.draw(gs::sampled_from(CHARS.to_vec()))
}

/// `u` or `v`, and `v` alone: what the generators keep to, so most patterns are valid.
#[derive(Clone, Copy)]
struct Mode {
    unicode: bool,
    sets: bool,
}

fn class_char(tc: &TestCase, mode: Mode) -> &'static str {
    match char_(tc) {
        "-" if mode.sets => "\\-",
        c => c,
    }
}

fn class(tc: &TestCase, mode: Mode) -> String {
    let mut s = String::from(if tc.draw(gs::booleans()) { "[^" } else { "[" });
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(4)) {
        match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 => s.push_str(&format!(
                "{}-{}",
                class_char(tc, mode),
                class_char(tc, mode)
            )),
            1 if !mode.unicode => s.push_str(&format!("\\{}", char_(tc))),
            2 if mode.sets => s.push_str(&format!("\\q{{{}{}|a}}", char_(tc), char_(tc))),
            _ => s.push_str(class_char(tc, mode)),
        }
    }
    s.push(']');
    s
}

fn atom(tc: &TestCase, mode: Mode, depth: usize) -> String {
    let atom = match tc.draw(gs::integers::<u8>().max_value(if depth >= 2 { 3 } else { 5 })) {
        0 => ".".to_owned(),
        // An identity escape, which `u` and `v` allow only for syntax characters.
        1 if !mode.unicode => format!("\\{}", char_(tc)),
        2 => class(tc, mode),
        4 => format!(
            "(?<{}>{})",
            tc.draw(gs::sampled_from(NAMES.to_vec())),
            pattern(tc, mode, depth + 1)
        ),
        5 => format!("(?:{})", pattern(tc, mode, depth + 1)),
        _ => char_(tc).to_owned(),
    };
    let quantifier = tc.draw(gs::sampled_from(vec!["", "", "", "*", "+", "?", "{2}"]));
    atom + quantifier
}

fn pattern(tc: &TestCase, mode: Mode, depth: usize) -> String {
    let alternatives = tc.draw(gs::integers::<usize>().min_value(1).max_value(2));
    (0..alternatives)
        .map(|_| {
            (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(4)))
                .map(|_| atom(tc, mode, depth))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Strings over `CHARS` as UTF-16, sliced anywhere, so lone surrogates occur too.
fn input(tc: &TestCase) -> Vec<u16> {
    let units: Vec<u16> = (0..tc.draw(gs::integers::<usize>().max_value(6)))
        .flat_map(|_| char_(tc).encode_utf16().collect::<Vec<_>>())
        .collect();
    let start = tc.draw(gs::integers::<usize>().max_value(units.len()));
    let end = tc.draw(
        gs::integers::<usize>()
            .min_value(start)
            .max_value(units.len()),
    );
    units[start..end].to_vec()
}

#[hegel::test(test_cases = 1000)]
fn e2e_rewritten_regexes_match_as_the_original(tc: TestCase) {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let flags = tc.draw(gs::sampled_from(vec!["", "i", "u", "iu", "v", "iv"]));
    let mode = Mode {
        unicode: flags.contains(['u', 'v']),
        sets: flags.contains('v'),
    };
    let original = format!("/{}/{flags}", pattern(&tc, mode, 0));
    let program = format!("x = {original};\n");
    let rewritten = match pibake::passes::ascii_bundle::run(&program, SourceType::mjs(), "t") {
        Ok(Some(e)) => {
            assert!(e.text.is_ascii(), "{}", e.text);
            Some(e.text["x = ".len()..e.text.len() - ";\n".len()].to_owned())
        }
        Ok(None) => Some(original.clone()),
        // The driver must then find the original invalid.
        Err(_) => None,
    };
    let inputs: Vec<Vec<u16>> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(6)))
        .map(|_| input(&tc))
        .collect();
    let mut d = driver().lock().unwrap_or_else(|e| e.into_inner());
    let line = json!({ "original": original, "rewritten": rewritten, "inputs": inputs });
    writeln!(d.stdin, "{line}").unwrap();
    let mut reply = String::new();
    d.stdout.read_line(&mut reply).unwrap();
    drop(d);
    let reply: Value = serde_json::from_str(&reply).expect("a reply from the driver");
    if reply["skip"] == true {
        tc.reject();
    }
    assert!(
        reply["ok"] == true,
        "{original} rewritten as {rewritten:?}: {reply:#}"
    );
}
