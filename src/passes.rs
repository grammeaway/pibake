//! Passes: Rust rewrites of the few module sources they target, made before esbuild runs.
//!
//! The bundle reads a rewritten file instead of the stage's (`bundle/build.mjs`), so esbuild's
//! own tree shaking drops whatever a rewrite stops importing. Passes on esbuild's output could
//! not: nothing marks its `__commonJS(...)` wrappers as pure. A pass edits source text at the
//! spans oxc finds, so everything outside its edits stays byte-identical.

mod trim_highlight;

use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::Program;
use oxc::span::SourceType;
use serde::{Deserialize, Serialize};

pub use trim_highlight::DEFAULT_LANGUAGES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    /// Registers only `[trim] highlight_languages` with highlight.js.
    TrimHighlight,
}

impl Pass {
    pub fn name(self) -> &'static str {
        match self {
            Pass::TrimHighlight => "trim_highlight",
        }
    }
}

pub const DEFAULT_PASSES: [Pass; 1] = [Pass::TrimHighlight];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Options {
    pub passes: Vec<Pass>,
    pub highlight_languages: Vec<String>,
}

impl Options {
    pub fn none() -> Self {
        Self {
            passes: vec![],
            highlight_languages: vec![],
        }
    }
}

/// One rewritten file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Relative to the stage.
    pub file: String,
    pub what: String,
    pub bytes_removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassReport {
    pub pass: Pass,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Stage-relative file → its rewritten text. Files no pass changed are absent.
    pub overrides: BTreeMap<String, String>,
    pub reports: Vec<PassReport>,
}

/// Runs `options.passes` in order over the stage's sources; each sees its predecessors' output.
pub fn run(stage: &Path, options: &Options) -> Result<Output> {
    let mut sources = Sources {
        stage,
        changed: BTreeMap::new(),
    };
    for (i, pass) in options.passes.iter().enumerate() {
        if options.passes[..i].contains(pass) {
            bail!("pass {} is listed twice in [build] passes", pass.name());
        }
    }
    let mut reports = vec![];
    for pass in &options.passes {
        let changes = match pass {
            Pass::TrimHighlight => trim_highlight::run(&mut sources, &options.highlight_languages)
                .context("pass trim_highlight")?,
        };
        reports.push(PassReport {
            pass: *pass,
            changes,
        });
    }
    Ok(Output {
        overrides: sources.changed,
        reports,
    })
}

/// The stage's files as the passes so far left them.
pub struct Sources<'a> {
    stage: &'a Path,
    changed: BTreeMap<String, String>,
}

impl Sources<'_> {
    pub fn read(&self, rel: &str) -> Result<String> {
        if let Some(text) = self.changed.get(rel) {
            return Ok(text.clone());
        }
        fs::read_to_string(self.stage.join(rel))
            .with_context(|| format!("reading {rel} in the stage {}", self.stage.display()))
    }

    /// Records `text` for `rel` unless `rel` already holds it; returns the bytes removed, or
    /// `None` when unchanged.
    pub fn replace(&mut self, rel: &str, text: String) -> Result<Option<usize>> {
        let old = self.read(rel)?;
        if old == text {
            return Ok(None);
        }
        let removed = old.len().saturating_sub(text.len());
        self.changed.insert(rel.to_owned(), text);
        Ok(Some(removed))
    }

    /// The stage-relative directory of package `name` as Node resolves it from `from_dir`.
    pub fn resolve_package(&self, from_dir: &str, name: &str) -> Result<String> {
        let mut dir = Some(Path::new(from_dir));
        while let Some(d) = dir {
            let candidate = d.join("node_modules").join(name);
            if self.stage.join(&candidate).join("package.json").is_file() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
            dir = d.parent();
        }
        bail!("{name} is not installed where {from_dir} would find it")
    }
}

/// Replaces each span of `text` with its text; spans must not overlap.
pub fn splice(text: &str, edits: &mut [(Range<usize>, String)]) -> Result<String> {
    edits.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, with) in edits.iter() {
        if range.start < at || range.end < range.start || range.end > text.len() {
            bail!("edit {range:?} overlaps another or lies outside the text");
        }
        out.push_str(&text[at..range.start]);
        out.push_str(with);
        at = range.end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Parses `text`; any syntax error fails, since a pass must not guess at a broken file.
pub fn parse<'a>(
    allocator: &'a Allocator,
    text: &'a str,
    source_type: SourceType,
    rel: &str,
) -> Result<Program<'a>> {
    let ret = oxc::parser::Parser::new(allocator, text, source_type).parse();
    if let Some(e) = ret.diagnostics.first() {
        bail!("{rel} does not parse: {e}");
    }
    Ok(ret.program)
}

/// `range` widened over the line break that follows it, so a removed statement leaves no blank line.
pub fn with_line_break(text: &str, range: Range<usize>) -> Range<usize> {
    let end = if text[range.end..].starts_with("\r\n") {
        range.end + 2
    } else if text[range.end..].starts_with('\n') {
        range.end + 1
    } else {
        range.end
    };
    range.start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_replaces_in_order_and_rejects_overlaps() {
        let mut edits = vec![(6..11, "there".to_owned()), (0..5, "HI".to_owned())];
        assert_eq!(splice("hello world!", &mut edits).unwrap(), "HI there!");
        assert_eq!(splice("abc", &mut []).unwrap(), "abc");
        let mut overlap = vec![(0..3, String::new()), (2..4, String::new())];
        assert!(splice("abcdef", &mut overlap).is_err());
        let mut outside = vec![(2..9, String::new())];
        assert!(splice("abc", &mut outside).is_err());
    }

    #[test]
    fn line_break_widening() {
        assert_eq!(with_line_break("a;\nb;", 0..2), 0..3);
        assert_eq!(with_line_break("a;\r\nb;", 0..2), 0..4);
        assert_eq!(with_line_break("a;", 0..2), 0..2);
    }

    #[test]
    fn duplicate_pass_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            passes: vec![Pass::TrimHighlight, Pass::TrimHighlight],
            highlight_languages: vec![],
        };
        let err = format!("{:#}", run(dir.path(), &options).unwrap_err());
        assert!(err.contains("listed twice"), "{err}");
    }

    #[test]
    fn resolve_package_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sources {
            stage: dir.path(),
            changed: BTreeMap::new(),
        };
        fs::create_dir_all(dir.path().join("node_modules/hl")).unwrap();
        fs::write(dir.path().join("node_modules/hl/package.json"), "{}").unwrap();
        assert_eq!(
            s.resolve_package("node_modules/@s/p/dist/utils", "hl")
                .unwrap(),
            "node_modules/hl"
        );
        fs::create_dir_all(dir.path().join("node_modules/@s/p/node_modules/hl")).unwrap();
        fs::write(
            dir.path()
                .join("node_modules/@s/p/node_modules/hl/package.json"),
            "{}",
        )
        .unwrap();
        assert_eq!(
            s.resolve_package("node_modules/@s/p/dist/utils", "hl")
                .unwrap(),
            "node_modules/@s/p/node_modules/hl"
        );
        assert!(s.resolve_package("x", "missing").is_err());
    }
}
