//! Entries of `settings.json` `packages`, and their sources, as Pi parses them.
//! Ported from Pi's `dist/core/package-manager.js` (`parseSource`, `parseNpmSpec`,
//! `getPackageIdentity`), `dist/utils/git.js` (`parseGitUrl`) and `dist/utils/paths.js` (`isLocalPath`).

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::paths;

pub const RESOURCE_TYPES: [&str; 4] = ["extensions", "skills", "prompts", "themes"];

/// One `packages` entry. `filter` is `None` for the string form: Pi resolves an object
/// without filter keys differently from the bare string, so the form is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    pub source: String,
    pub filter: Option<Filter>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Filter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
}

impl Filter {
    pub fn patterns(&self, resource_type: &str) -> Option<&[String]> {
        match resource_type {
            "extensions" => self.extensions.as_deref(),
            "skills" => self.skills.as_deref(),
            "prompts" => self.prompts.as_deref(),
            "themes" => self.themes.as_deref(),
            _ => None,
        }
    }

    fn slot(&mut self, resource_type: &str) -> &mut Option<Vec<String>> {
        match resource_type {
            "extensions" => &mut self.extensions,
            "skills" => &mut self.skills,
            "prompts" => &mut self.prompts,
            "themes" => &mut self.themes,
            _ => unreachable!("not a resource type: {resource_type}"),
        }
    }
}

impl PackageEntry {
    /// Keys Pi does not read are ignored, as Pi ignores them. Values Pi would trip
    /// over (a non-array filter, a non-boolean `autoload`) are errors.
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = match value {
            Value::String(source) => {
                return Ok(Self {
                    source: source.clone(),
                    filter: None,
                });
            }
            Value::Object(object) => object,
            other => bail!("expected a string or an object, got {other}"),
        };
        let source = match object.get("source") {
            Some(Value::String(s)) => s.clone(),
            _ => bail!("object entry needs a string `source`"),
        };
        let mut filter = Filter::default();
        for resource_type in RESOURCE_TYPES {
            *filter.slot(resource_type) = match object.get(resource_type) {
                None => None,
                Some(value) => Some(string_array(value).ok_or_else(|| {
                    anyhow!("`{resource_type}` of {source} must be an array of strings")
                })?),
            };
        }
        filter.autoload = match object.get("autoload") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => bail!("`autoload` of {source} must be a boolean"),
        };
        Ok(Self {
            source,
            filter: Some(filter),
        })
    }

    pub fn to_json(&self) -> Value {
        let Some(filter) = &self.filter else {
            return Value::String(self.source.clone());
        };
        let mut object = Map::new();
        object.insert("source".into(), Value::String(self.source.clone()));
        if let Value::Object(fields) = serde_json::to_value(filter).expect("filter serialises") {
            object.extend(fields);
        }
        Value::Object(object)
    }
}

pub fn string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Npm {
        spec: String,
        name: String,
        version: Option<String>,
    },
    Git {
        repo: String,
        host: String,
        path: String,
        #[serde(rename = "ref")]
        git_ref: Option<String>,
    },
    Local {
        path: String,
    },
}

impl Source {
    /// Errors where Pi would hand the source to `hosted-git-info`, which pibake does not port,
    /// so a source never silently resolves to something other than what Pi loads.
    pub fn parse(source: &str) -> Result<Self> {
        if let Some(spec) = source.strip_prefix("npm:") {
            let spec = spec.trim();
            let (name, version) = parse_npm_spec(spec);
            // Its install path is `npm/node_modules/<name>`, which must stay there.
            if name.contains('\\') || name.split('/').any(|s| matches!(s, "" | "." | "..")) {
                bail!("npm package name {name:?} of `{source}` is not a package name");
            }
            return Ok(Self::Npm {
                spec: spec.to_owned(),
                name,
                version,
            });
        }
        if is_local_path(source) {
            return Ok(Self::Local {
                path: source.to_owned(),
            });
        }
        let trimmed = source.trim();
        let has_git_prefix = trimmed.starts_with("git:");
        let url = trimmed.strip_prefix("git:").unwrap_or(trimmed).trim();
        if !has_git_prefix && !has_git_protocol(url) {
            return Ok(Self::Local {
                path: source.to_owned(),
            });
        }
        parse_generic_git_url(url).ok_or_else(|| {
            anyhow!(
                "unsupported git source `{source}`: Pi would try hosted-git-info shorthands here, \
                 which pibake does not support; use the `git:<host>/<owner>/<repo>[@ref]` form"
            )
        })
    }

    /// Pi's `getPackageIdentity`: the same package under any version, ref or URL form.
    pub fn identity(&self, base_dir: &Path, home: &Path) -> String {
        match self {
            Self::Npm { name, .. } => format!("npm:{name}"),
            Self::Git { host, path, .. } => format!("git:{host}/{path}"),
            Self::Local { path } => {
                format!("local:{}", paths::resolve(base_dir, path, home).display())
            }
        }
    }

    /// Where Pi installs (npm, git) or finds (local) the package for user scope.
    /// shortcut: Pi falls back to a legacy global npm root when `npm/node_modules/<name>` is missing; not ported, add if a setup still uses global installs.
    pub fn install_path(&self, agent_dir: &Path, home: &Path) -> PathBuf {
        match self {
            Self::Npm { name, .. } => agent_dir.join("npm").join("node_modules").join(name),
            Self::Git { host, path, .. } => agent_dir.join("git").join(host).join(path),
            Self::Local { path } => paths::resolve(agent_dir, path, home),
        }
    }
}

fn is_local_path(value: &str) -> bool {
    const NON_LOCAL: [&str; 7] = [
        "npm:", "git:", "github:", "http:", "https:", "ssh:", "builtin:",
    ];
    let trimmed = value.trim();
    !NON_LOCAL.iter().any(|p| trimmed.starts_with(p))
}

/// `/^(https?|ssh|git):\/\//i`
fn has_git_protocol(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once("://") else {
        return false;
    };
    ["http", "https", "ssh", "git"]
        .iter()
        .any(|s| scheme.eq_ignore_ascii_case(s))
}

/// Port of `/^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$/`; no match yields the whole spec as the name.
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let start = usize::from(spec.starts_with('@'));
    let name_len = spec[start..].find('@').unwrap_or(spec.len() - start);
    let no_match = (spec.to_owned(), None);
    if name_len == 0 {
        return no_match;
    }
    let (name, rest) = spec.split_at(start + name_len);
    let Some(version) = rest.strip_prefix('@') else {
        return (name.to_owned(), None);
    };
    // JS `.` does not match line terminators, so such a version fails the whole match.
    let is_js_line_terminator = |c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    if version.is_empty() || version.contains(is_js_line_terminator) {
        return no_match;
    }
    (name.to_owned(), Some(version.to_owned()))
}

/// Pi's `splitRef`: `(repo, ref)`, the ref being whatever follows the first `@` in the repo path.
fn split_ref(url: &str) -> (String, Option<String>) {
    let split_path = |path: &str| -> Option<(String, String)> {
        let (repo_path, git_ref) = path.split_once('@')?;
        (!repo_path.is_empty() && !git_ref.is_empty())
            .then(|| (repo_path.to_owned(), git_ref.to_owned()))
    };
    if let Some((host, path)) = scp_like(url) {
        return match split_path(path) {
            Some((repo_path, git_ref)) => (format!("git@{host}:{repo_path}"), Some(git_ref)),
            None => (url.to_owned(), None),
        };
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        return match split_path(path.trim_start_matches('/')) {
            Some((repo_path, git_ref)) => {
                (format!("{scheme}://{authority}/{repo_path}"), Some(git_ref))
            }
            None => (url.to_owned(), None),
        };
    }
    match url.split_once('/') {
        Some((host, path)) => match split_path(path) {
            Some((repo_path, git_ref)) => (format!("{host}/{repo_path}"), Some(git_ref)),
            None => (url.to_owned(), None),
        },
        None => (url.to_owned(), None),
    }
}

/// `/^git@([^:]+):(.+)$/`
fn scp_like(url: &str) -> Option<(&str, &str)> {
    let (host, path) = url.strip_prefix("git@")?.split_once(':')?;
    (!host.is_empty() && !path.is_empty()).then_some((host, path))
}

/// Pi's `parseGenericGitUrl` plus `buildGitSource`.
// shortcut: URLs are split textually, without WHATWG normalisation (host case, percent-encoding); `%` is rejected instead. Port `new URL` semantics if a setup needs escaped URLs.
fn parse_generic_git_url(url: &str) -> Option<Source> {
    // hosted-git-info would take a `#` fragment as the ref.
    if url.contains('#') {
        return None;
    }
    let (repo_without_ref, git_ref) = split_ref(url);
    let (repo, host, path) = if let Some((host, path)) = scp_like(&repo_without_ref) {
        (repo_without_ref.clone(), host.to_owned(), path.to_owned())
    } else if ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|p| repo_without_ref.starts_with(p))
    {
        let rest = repo_without_ref.split_once("://")?.1;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host_port.split_once(':').map_or(host_port, |(h, _)| h);
        let path = path.split('?').next().unwrap_or("");
        (
            repo_without_ref.clone(),
            host.to_owned(),
            path.trim_start_matches('/').to_owned(),
        )
    } else {
        let (host, path) = repo_without_ref.split_once('/')?;
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        (
            format!("https://{repo_without_ref}"),
            host.to_owned(),
            path.to_owned(),
        )
    };
    if path.starts_with('/') {
        return None;
    }
    let path = path
        .strip_suffix(".git")
        .unwrap_or(&path)
        .trim_start_matches('/');
    let unsafe_part = |part: &str, allow_slash: bool| {
        part.contains(['\0', '\\', '%'])
            || part.starts_with('/')
            || (!allow_slash && part.contains('/'))
            || part.split('/').any(|s| s == "..")
    };
    let segments = path.split('/').count();
    // hosted-git-info reads a ref from GitHub's `/tree/<ref>` and cuts these hosts' paths to
    // `<user>/<project>`.
    let hosted_cut = [
        "github.com",
        "gist.github.com",
        "bitbucket.org",
        "git.sr.ht",
    ]
    .iter()
    .any(|h| host.eq_ignore_ascii_case(h));
    if host.is_empty()
        || path.is_empty()
        || segments < 2
        || (hosted_cut && segments > 2)
        || unsafe_part(&host, false)
        || unsafe_part(path, true)
    {
        return None;
    }
    Some(Source::Git {
        repo,
        host,
        path: path.to_owned(),
        git_ref,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git(repo: &str, host: &str, path: &str, git_ref: Option<&str>) -> Source {
        Source::Git {
            repo: repo.into(),
            host: host.into(),
            path: path.into(),
            git_ref: git_ref.map(Into::into),
        }
    }

    fn npm(spec: &str, name: &str, version: Option<&str>) -> Source {
        Source::Npm {
            spec: spec.into(),
            name: name.into(),
            version: version.map(Into::into),
        }
    }

    fn local(path: &str) -> Source {
        Source::Local { path: path.into() }
    }

    #[test]
    fn npm_specs() {
        let cases = [
            ("npm:pi-btw", npm("pi-btw", "pi-btw", None)),
            (
                "npm:pi-btw@0.8.0",
                npm("pi-btw@0.8.0", "pi-btw", Some("0.8.0")),
            ),
            ("npm: @s/p@^1 ", npm("@s/p@^1", "@s/p", Some("^1"))),
            ("npm:@s/p", npm("@s/p", "@s/p", None)),
            ("npm:p@", npm("p@", "p@", None)),
            ("npm:@", npm("@", "@", None)),
            ("npm:p@1@2", npm("p@1@2", "p", Some("1@2"))),
            ("npm:p@1\n2", npm("p@1\n2", "p@1\n2", None)),
        ];
        for (input, expected) in cases {
            assert_eq!(Source::parse(input).unwrap(), expected, "{input:?}");
        }
    }

    #[test]
    fn npm_names_that_are_not_relative_paths_are_errors() {
        for input in [
            "npm:",
            "npm:.",
            "npm:..",
            "npm:../x",
            "npm:@s/",
            "npm:@s/..",
            "npm:a\\b",
        ] {
            assert!(Source::parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn git_sources() {
        let cases = [
            (
                "git:github.com/DietrichGebert/ponytail",
                git(
                    "https://github.com/DietrichGebert/ponytail",
                    "github.com",
                    "DietrichGebert/ponytail",
                    None,
                ),
            ),
            (
                "git:github.com/o/r@v1",
                git("https://github.com/o/r", "github.com", "o/r", Some("v1")),
            ),
            (
                "https://gitlab.com/g/sub/r.git@abc123",
                git(
                    "https://gitlab.com/g/sub/r.git",
                    "gitlab.com",
                    "g/sub/r",
                    Some("abc123"),
                ),
            ),
            (
                "git:git@github.com:o/r.git@main",
                git("git@github.com:o/r.git", "github.com", "o/r", Some("main")),
            ),
            (
                "ssh://git@host.example:2222/o/r",
                git(
                    "ssh://git@host.example:2222/o/r",
                    "host.example",
                    "o/r",
                    None,
                ),
            ),
            (
                "git:localhost/o/r",
                git("https://localhost/o/r", "localhost", "o/r", None),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(Source::parse(input).unwrap(), expected, "{input:?}");
        }
    }

    #[test]
    fn unsafe_or_unsupported_git_sources_are_errors() {
        for input in [
            "git:github.com/only-owner",
            "git:o/r",
            "git:github:o/r",
            "git:github.com/o/../r",
            "git:github.com/o/r%2e%2e",
            "https://github.com/",
            // hosted-git-info takes these as refs or drops path segments; pibake does not port it.
            "git:github.com/o/r#v1",
            "https://github.com/o/r#v1",
            "git:github.com/o/r/tree/main",
            "https://GitHub.com/o/r/tree/main",
            "git:bitbucket.org/o/r/src",
            "git:git.sr.ht/~o/r/x",
            // Pi's generic parser matches the protocol case-sensitively.
            "git:HTTPS://example.com/o/r",
        ] {
            assert!(Source::parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn local_sources_follow_pi() {
        for input in [
            "./pkg",
            "../pkg",
            "/abs",
            "~/pkg",
            "file:///x",
            "pkg",
            "github:o/r",
            "http:x",
        ] {
            assert_eq!(Source::parse(input).unwrap(), local(input), "{input:?}");
        }
    }

    #[test]
    fn identity_ignores_version_ref_and_url_form() {
        let base = Path::new("/agent");
        let home = Path::new("/home/u");
        let id = |s: &str| Source::parse(s).unwrap().identity(base, home);
        assert_eq!(id("npm:@s/p@1.0.0"), id("npm:@s/p@2"));
        assert_eq!(id("npm:@s/p"), "npm:@s/p");
        assert_eq!(
            id("git:github.com/o/r@v1"),
            id("https://github.com/o/r.git")
        );
        assert_eq!(id("git:github.com/o/r"), id("git:git@github.com:o/r"));
        assert_eq!(id("./pkg"), "local:/agent/pkg");
        assert_eq!(id("~/pkg"), "local:/home/u/pkg");
    }

    #[test]
    fn install_paths() {
        let agent = Path::new("/agent");
        let home = Path::new("/home/u");
        let at = |s: &str| Source::parse(s).unwrap().install_path(agent, home);
        assert_eq!(
            at("npm:@s/p@1"),
            PathBuf::from("/agent/npm/node_modules/@s/p")
        );
        assert_eq!(
            at("git:github.com/o/r@v1"),
            PathBuf::from("/agent/git/github.com/o/r")
        );
        assert_eq!(at("../elsewhere"), PathBuf::from("/elsewhere"));
    }

    #[test]
    fn entry_forms() {
        let string = PackageEntry::from_json(&json!("npm:x")).unwrap();
        assert_eq!(string.filter, None);
        assert_eq!(string.to_json(), json!("npm:x"));

        let bare_object = PackageEntry::from_json(&json!({"source": "npm:x"})).unwrap();
        assert_eq!(bare_object.filter, Some(Filter::default()));
        assert_eq!(bare_object.to_json(), json!({"source": "npm:x"}));

        let filtered = PackageEntry::from_json(&json!({
            "source": "npm:x", "extensions": [], "skills": ["a", "!b"], "autoload": false, "ignored": 1
        }))
        .unwrap();
        let filter = filtered.filter.as_ref().unwrap();
        assert_eq!(filter.patterns("extensions"), Some(&[][..]));
        assert_eq!(
            filter.skills.as_deref(),
            Some(&["a".to_owned(), "!b".to_owned()][..])
        );
        assert_eq!(filter.prompts, None);
        assert_eq!(filter.autoload, Some(false));
        assert_eq!(
            filtered.to_json(),
            json!({"source": "npm:x", "extensions": [], "skills": ["a", "!b"], "autoload": false})
        );
    }

    #[test]
    fn malformed_entries_are_errors() {
        for value in [
            json!(1),
            json!(null),
            json!({}),
            json!({"source": 1}),
            json!({"source": "npm:x", "extensions": "a"}),
            json!({"source": "npm:x", "extensions": [1]}),
            json!({"source": "npm:x", "extensions": null}),
            json!({"source": "npm:x", "autoload": "no"}),
        ] {
            assert!(PackageEntry::from_json(&value).is_err(), "{value}");
        }
    }
}
