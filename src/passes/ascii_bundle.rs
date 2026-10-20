//! `ascii_bundle`: esbuild's output made pure ASCII. Node holds a SEA main as a one-byte string
//! only if it is ASCII, and as UTF-16 (twice the bytes) otherwise. esbuild escapes strings,
//! templates and identifiers but not comments or regex literals, so this escapes those.
//!
//! Unlike the other passes it runs after esbuild, on the bundle itself (`bundle.rs`). Columns
//! after an escape on its line no longer match esbuild's source map, which no executable carries.

use anyhow::{Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::RegExpLiteral;
use oxc::ast_visit::Visit;
use oxc::span::{SourceType, Span};

use super::splice;

#[derive(Debug)]
pub struct Escaped {
    pub text: String,
    pub comments: usize,
    pub regexes: usize,
}

/// `text` with every comment and regex literal escaped to ASCII; `None` if it is ASCII already.
/// Fails if a non-ASCII character is left elsewhere, naming its line.
pub fn run(text: &str, source_type: SourceType, rel: &str) -> Result<Option<Escaped>> {
    if text.is_ascii() {
        return Ok(None);
    }
    let allocator = Allocator::default();
    let program = super::parse(&allocator, text, source_type, rel)?;
    let range = |span: Span| span.start as usize..span.end as usize;
    let mut edits = vec![];
    let mut comments = 0;
    for comment in &program.comments {
        let s = &text[range(comment.span)];
        if !s.is_ascii() {
            comments += 1;
            edits.push((range(comment.span), escape_comment(s)));
        }
    }
    let mut regexes = Regexes(vec![]);
    regexes.visit_program(&program);
    let regex_count = regexes.0.len();
    for span in regexes.0 {
        edits.push((range(span), escape_regex_literal(&text[range(span)])));
    }
    let out = splice(text, &mut edits)?;
    if let Some(at) = out.find(|c: char| !c.is_ascii()) {
        let line = out[..at].matches('\n').count() + 1;
        let c = out[at..].chars().next().unwrap_or_default();
        bail!(
            "{rel}:{line}: {c:?} (U+{:04X}) is outside any comment or regex literal, so it cannot be escaped",
            c as u32
        );
    }
    Ok(Some(Escaped {
        text: out,
        comments,
        regexes: regex_count,
    }))
}

/// Spans of the regex literals that hold a non-ASCII character.
struct Regexes(Vec<Span>);

impl<'a> Visit<'a> for Regexes {
    fn visit_reg_exp_literal(&mut self, it: &RegExpLiteral<'a>) {
        if !it.regex.pattern.text.is_ascii() {
            self.0.push(it.span);
        }
    }
}

/// Comments have no meaning, so the escape only needs to stay readable.
fn escape_comment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{{{:X}}}", c as u32));
        }
    }
    out
}

/// `/pattern/flags` with each non-ASCII character as an escape of the same code point (`u`, `v`)
/// or the same UTF-16 code units (otherwise): the same pattern in every position.
pub fn escape_regex_literal(literal: &str) -> String {
    let end = literal
        .rfind('/')
        .expect("a regex literal ends its pattern with /");
    let (pattern, flags) = (&literal[1..end], &literal[end..]);
    let unicode = flags.contains('u') || flags.contains('v');
    let mut out = String::with_capacity(literal.len() + 16);
    out.push('/');
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            // `\é` is an identity escape (non-`u` only), so the escape replaces both.
            '\\' => match chars.next() {
                Some(n) if !n.is_ascii() => escape_regex_char(&mut out, n, unicode),
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push('\\'),
            },
            c if c.is_ascii() => out.push(c),
            c => escape_regex_char(&mut out, c, unicode),
        }
    }
    out.push_str(flags);
    out
}

fn escape_regex_char(out: &mut String, c: char, unicode: bool) {
    if unicode {
        out.push_str(&format!("\\u{{{:X}}}", c as u32));
    } else {
        for unit in c.encode_utf16(&mut [0; 2]) {
            out.push_str(&format!("\\u{unit:04X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape(text: &str) -> String {
        run(text, SourceType::mjs(), "t.mjs").unwrap().unwrap().text
    }

    #[test]
    fn ascii_input_is_left_alone() {
        assert!(
            run("let a = /x/;", SourceType::mjs(), "t")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn comments_get_code_point_escapes() {
        let out = run(
            "// request’s URL\nlet a = 1; /*! Wärting 😀 */\n",
            SourceType::mjs(),
            "t",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out.text,
            "// request\\u{2019}s URL\nlet a = 1; /*! W\\u{E4}rting \\u{1F600} */\n"
        );
        assert_eq!((out.comments, out.regexes), (2, 0));
    }

    #[test]
    fn regexes_by_flag_mode() {
        assert_eq!(escape("/[-–—]/g;"), "/[-\\u2013\\u2014]/g;");
        assert_eq!(escape("/[-–—]/gu;"), "/[-\\u{2013}\\u{2014}]/gu;");
        assert_eq!(escape("/[\\q{é}]/v;"), "/[\\q{\\u{E9}}]/v;");
        assert_eq!(escape("/^[#🔗]$/;"), "/^[#\\uD83D\\uDD17]$/;");
        assert_eq!(escape("/^[#🔗]$/u;"), "/^[#\\u{1F517}]$/u;");
        assert_eq!(escape("/a\\»b\\\\»/;"), "/a\\u00BBb\\\\\\u00BB/;");
    }

    #[test]
    fn regexes_in_cjs_and_strings_left_to_esbuild() {
        let out = run("x = /é/i.test('é');", SourceType::cjs(), "t").unwrap_err();
        assert!(format!("{out:#}").contains("t:1: 'é' (U+00E9)"), "{out:#}");
        let out = run("x = /é/i;\n// ü\n", SourceType::cjs(), "t")
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "x = /\\u00E9/i;\n// \\u{FC}\n");
    }

    #[test]
    fn leftover_names_its_line() {
        let err = run("let a;\nlet b = `→`;\n", SourceType::mjs(), "main.mjs").unwrap_err();
        assert!(
            format!("{err:#}").contains("main.mjs:2: '→' (U+2192)"),
            "{err:#}"
        );
    }
}
