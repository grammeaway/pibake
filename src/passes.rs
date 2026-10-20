//! Passes: Rust rewrites of the few module sources they target, made before esbuild runs
//! (`ascii_bundle` alone rewrites esbuild's output).
//!
//! The bundle reads a rewritten file instead of the stage's (`bundle/build.mjs`), so esbuild's
//! own tree shaking drops whatever a rewrite stops importing. Passes on esbuild's output could
//! not: nothing marks its `__commonJS(...)` wrappers as pure. A pass edits source text at the
//! spans oxc finds, so everything outside its edits stays byte-identical.
//!
//! A pass that bakes a setting into the build records it as `Frozen`; the bundle's entry checks
//! the live setup against it at startup (`bundle/frozen.mjs`).

pub mod ascii_bundle;
mod incremental_markdown;
mod incremental_tool_json;
mod lazy_http_dispatcher;
pub mod lazy_modules;
pub mod snapshot_safety;
mod trim_apis;
mod trim_highlight;

use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oxc::allocator::Allocator;
use oxc::ast::ast::Program;
use oxc::span::SourceType;
use serde::{Deserialize, Serialize};

pub use trim_apis::left_out_message;
pub use trim_highlight::DEFAULT_LANGUAGES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    /// Registers only `[trim] highlight_languages` with highlight.js.
    TrimHighlight,
    /// Stubs pi-ai's stream APIs outside `[trim] apis` (default: derived from the setup).
    TrimApis,
    /// Parses streamed tool-call arguments incrementally instead of re-parsing each delta.
    IncrementalToolJson,
    /// Lexes and renders streamed Markdown from its last unchanged block instead of in full.
    IncrementalMarkdown,
    /// Makes Pi's HTTP dispatcher on first use instead of at startup, and loads undici then.
    LazyHttpDispatcher,
    /// Evaluates `lazy_modules::FILES` on first use instead of at startup; not on the snapshot.
    LazyModules,
    /// Defers module code that reads launch state; added by `backend = "sea_snapshot"`.
    SnapshotSafety,
    /// Escapes the bundle's comments and regex literals to ASCII, so Node holds it as one byte
    /// per character instead of two.
    AsciiBundle,
}

impl Pass {
    pub fn name(self) -> &'static str {
        match self {
            Pass::TrimHighlight => "trim_highlight",
            Pass::TrimApis => "trim_apis",
            Pass::IncrementalToolJson => "incremental_tool_json",
            Pass::IncrementalMarkdown => "incremental_markdown",
            Pass::LazyHttpDispatcher => "lazy_http_dispatcher",
            Pass::LazyModules => "lazy_modules",
            Pass::SnapshotSafety => "snapshot_safety",
            Pass::AsciiBundle => "ascii_bundle",
        }
    }
}

pub const DEFAULT_PASSES: [Pass; 7] = [
    Pass::TrimHighlight,
    Pass::TrimApis,
    Pass::IncrementalToolJson,
    Pass::IncrementalMarkdown,
    Pass::LazyHttpDispatcher,
    Pass::LazyModules,
    Pass::AsciiBundle,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Options {
    pub passes: Vec<Pass>,
    pub highlight_languages: Vec<String>,
    /// `None`: derived from the setup.
    pub apis: Option<Vec<String>>,
    /// Runs `snapshot_safety` after `passes`.
    pub snapshot: bool,
}

impl Options {
    pub fn none() -> Self {
        Self {
            passes: vec![],
            highlight_languages: vec![],
            apis: None,
            snapshot: false,
        }
    }
}

/// What a pass may read besides the stage: the setup the build is for.
#[derive(Debug, Clone)]
pub struct Setup {
    pub agent_dir: PathBuf,
    /// Stage-relative directories of extension code (packages and local `extensions/`).
    pub extension_dirs: Vec<String>,
}

/// A setting a pass baked into the build, for the startup check in `bundle/frozen.mjs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frozen {
    Apis {
        /// The `pibake.toml` setting to change.
        setting: String,
        left_out: Vec<String>,
        /// Built-in provider → the stream APIs its catalog models use.
        providers: BTreeMap<String, Vec<String>>,
    },
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
    /// What the pass decided and why, beyond its file changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Stage-relative file → its rewritten text. Files no pass changed are absent.
    pub overrides: BTreeMap<String, String>,
    pub reports: Vec<PassReport>,
    pub frozen: Vec<Frozen>,
    /// Rewritten files the bundle may leave out, as another pass made them unreachable.
    pub optional: Vec<String>,
    /// Files the bundle evaluates on first use (`lazy_modules`).
    pub lazy: Vec<lazy_modules::LazyFile>,
}

/// Runs `options.passes` in order over the stage's sources; each sees its predecessors' output.
pub fn run(stage: &Path, options: &Options, setup: &Setup) -> Result<Output> {
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
    let mut frozen = vec![];
    let mut optional = vec![];
    let mut lazy = vec![];
    let snapshot = options.snapshot.then_some(Pass::SnapshotSafety);
    for pass in options.passes.iter().chain(&snapshot) {
        let (changes, notes) = match pass {
            Pass::TrimHighlight => (
                trim_highlight::run(&mut sources, &options.highlight_languages)
                    .context("pass trim_highlight")?,
                vec![],
            ),
            Pass::TrimApis => {
                let out = trim_apis::run(&mut sources, options.apis.as_deref(), setup)
                    .context("pass trim_apis")?;
                frozen.push(out.frozen);
                (out.changes, out.notes)
            }
            Pass::IncrementalToolJson => {
                let out = incremental_tool_json::run(&mut sources)
                    .context("pass incremental_tool_json")?;
                optional.extend(out.optional);
                (out.changes, vec![])
            }
            Pass::IncrementalMarkdown => (
                incremental_markdown::run(&mut sources).context("pass incremental_markdown")?,
                vec![],
            ),
            Pass::LazyHttpDispatcher => (
                lazy_http_dispatcher::run(&mut sources).context("pass lazy_http_dispatcher")?,
                vec![],
            ),
            Pass::SnapshotSafety => (
                snapshot_safety::run(&mut sources).context("pass snapshot_safety")?,
                vec![],
            ),
            Pass::LazyModules if options.snapshot => (
                vec![],
                vec!["skipped: the snapshot holds these modules evaluated, and one evaluated after the resume would miss snapshot_safety's initialisers".to_owned()],
            ),
            Pass::LazyModules => {
                let (changes, files) =
                    lazy_modules::run(&sources).context("pass lazy_modules")?;
                lazy = files;
                (changes, vec![])
            }
            // Runs on esbuild's output, in `bundle.rs`.
            Pass::AsciiBundle => continue,
        };
        reports.push(PassReport {
            pass: *pass,
            changes,
            notes,
        });
    }
    Ok(Output {
        overrides: sources.changed,
        reports,
        frozen,
        optional,
        lazy,
    })
}

/// The stage's files as the passes so far left them.
pub struct Sources<'a> {
    stage: &'a Path,
    changed: BTreeMap<String, String>,
}

impl Sources<'_> {
    pub fn stage(&self) -> &Path {
        self.stage
    }

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
            ..Options::none()
        };
        let setup = Setup {
            agent_dir: dir.path().into(),
            extension_dirs: vec![],
        };
        let err = format!("{:#}", run(dir.path(), &options, &setup).unwrap_err());
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
