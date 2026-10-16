use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `~` and `~/…` only, like Pi's `normalizePath`; `~user` is left alone.
pub fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    match input.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(input),
    }
}

/// Port of Pi's `resolvePath(input, baseDir)` (`dist/utils/paths.js`).
pub fn resolve(base: &Path, input: &str, home: &Path) -> PathBuf {
    let trimmed = input.trim();
    // shortcut: `file://` URLs are stripped, not percent-decoded; decode if a setup ever uses an escaped file URL.
    let path = match trimmed.strip_prefix("file://") {
        Some(rest) => PathBuf::from(rest),
        None => expand_tilde(trimmed, home),
    };
    normalize(&base.join(path))
}

/// Lexical normalisation, as Node's `path.resolve` does: no symlinks followed.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` relative to `root` with `/` separators, or absolute when outside it.
pub fn display_rel(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/u"));
        assert_eq!(expand_tilde("~/a/b", home), PathBuf::from("/home/u/a/b"));
        assert_eq!(expand_tilde("~other/a", home), PathBuf::from("~other/a"));
        assert_eq!(expand_tilde("a/~", home), PathBuf::from("a/~"));
    }

    #[test]
    fn resolve_matches_node_semantics() {
        let home = Path::new("/home/u");
        let base = Path::new("/agent");
        assert_eq!(resolve(base, " ./pkg ", home), PathBuf::from("/agent/pkg"));
        assert_eq!(resolve(base, "../x/./y", home), PathBuf::from("/x/y"));
        assert_eq!(resolve(base, "/abs/p", home), PathBuf::from("/abs/p"));
        assert_eq!(resolve(base, "~/p", home), PathBuf::from("/home/u/p"));
        assert_eq!(resolve(base, "file:///f/p", home), PathBuf::from("/f/p"));
        assert_eq!(resolve(base, "/../..", home), PathBuf::from("/"));
    }

    #[test]
    fn display_rel_inside_and_outside() {
        let root = Path::new("/r");
        assert_eq!(display_rel(root, Path::new("/r/a/b.ts")), "a/b.ts");
        assert_eq!(display_rel(root, Path::new("/r")), ".");
        assert_eq!(display_rel(root, Path::new("/elsewhere/c")), "/elsewhere/c");
    }
}
