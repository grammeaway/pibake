//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

pub fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// git isolated from the user's config (signing, hooks), so commits work anywhere.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

pub fn commit(repo: &Path, file: &str, content: &str) -> String {
    write(&repo.join(file), content);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", file]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Every path under the given top-level entries, with type, size, mtime and link target.
/// Symlinks are recorded, not followed.
pub fn fingerprint(dir: &Path, names: &[String]) -> Vec<String> {
    fn walk(path: &Path, root: &Path, out: &mut Vec<String>) {
        let meta = fs::symlink_metadata(path).unwrap();
        let link = fs::read_link(path).ok();
        out.push(format!(
            "{} {:?} {} {}.{} {:?}",
            path.strip_prefix(root).unwrap().display(),
            meta.file_type(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            link
        ));
        if meta.is_dir() {
            let mut children: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                walk(&child, root, out);
            }
        }
    }
    let mut out = Vec::new();
    for name in names {
        walk(&dir.join(name), dir, &mut out);
    }
    out
}

pub fn all_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `pibake.lock` and `pibake.package-lock.json` in `dir`, consistent with each other.
pub fn write_lock_files(dir: &Path, npm_lock: &serde_json::Value, git: Vec<pibake::lock::GitPin>) {
    use pibake::lock::{LOCK_FILE, Lock, NPM_LOCK_FILE, NodePin, NpmLockPin, PiPin, sha256_hex};
    let bytes = serde_json::to_vec_pretty(npm_lock).unwrap();
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(NPM_LOCK_FILE), &bytes).unwrap();
    let lock = Lock {
        pi: PiPin {
            version: "1.3.0".into(),
            integrity: "sha512-pi".into(),
        },
        node: NodePin {
            version: "26.11.1".into(),
        },
        npm_lock: NpmLockPin {
            sha256: sha256_hex(&bytes),
        },
        git,
    };
    fs::write(dir.join(LOCK_FILE), lock.render()).unwrap();
}

/// An npm lock as `pibake lock` writes it: Pi, `pi-btw`, and esbuild with one optional
/// platform binary that no host installs.
pub fn sample_npm_lock() -> serde_json::Value {
    use pibake::lock::{ESBUILD_VERSION, PI_PACKAGE};
    serde_json::json!({
        "name": "pibake-stage",
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "pibake-stage",
                "dependencies": {PI_PACKAGE: "1.3.0", "pi-btw": "0.8.0"},
                "devDependencies": {"esbuild": ESBUILD_VERSION},
            },
            format!("node_modules/{PI_PACKAGE}"): {"version": "1.3.0"},
            "node_modules/pi-btw": {"version": "0.8.0"},
            "node_modules/esbuild": {"version": ESBUILD_VERSION, "dev": true},
            "node_modules/@esbuild/aix-ppc64": {"version": ESBUILD_VERSION, "dev": true, "optional": true},
        },
    })
}

pub fn e2e_enabled() -> bool {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        eprintln!("skipped: set PIBAKE_E2E=1");
        return false;
    }
    true
}

/// Runs the pibake binary in `dir`; panics with its output unless it exits as expected.
pub fn pibake(dir: &Path, args: &[&str], expect_success: bool) -> String {
    pibake_with(dir, args, &[], expect_success)
}

/// As `pibake`, with environment variables set.
pub fn pibake_with(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
    expect_success: bool,
) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pibake"))
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.success(),
        expect_success,
        "pibake {args:?}:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

pub fn bundled_launcher(stdout: &str) -> std::path::PathBuf {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("bundled ") || l.starts_with("bundle up to date: "))
        .unwrap_or_else(|| panic!("no bundle path in:\n{stdout}"));
    std::path::PathBuf::from(line.rsplit(' ').next().unwrap())
}

/// highlight.js's `lib/index.js` (10.7.3 shape) registering `names`.
pub fn highlight_index_js(names: &[&str]) -> String {
    let mut s = String::from("var hljs = require('./core');\n\n");
    for n in names {
        s.push_str(&format!(
            "hljs.registerLanguage('{n}', require('./languages/{n}'));\n"
        ));
    }
    s.push_str("\nmodule.exports = hljs;\n");
    s
}

fn grammar_binding(name: &str) -> String {
    format!("lang_{}", name.replace('-', "_"))
}

/// Pi 1.3.0's `dist/utils/syntax-highlight.js` shape, registering `eager` at startup.
pub fn syntax_highlight_js(eager: &[&str]) -> String {
    let mut s = String::from("import hljs from \"highlight.js/lib/core.js\";\n");
    for n in eager {
        s.push_str(&format!(
            "import {} from \"highlight.js/lib/languages/{n}.js\";\n",
            grammar_binding(n)
        ));
    }
    s.push_str("import { decodeHtmlEntityAt } from \"./html.js\";\nconst eagerLanguages = {\n");
    for n in eager {
        s.push_str(&format!("    \"{n}\": {},\n", grammar_binding(n)));
    }
    s.push_str(
        "};\nfor (const [name, language] of Object.entries(eagerLanguages)) {\n    hljs.registerLanguage(name, language);\n}\nexport function loadAllHighlightLanguages() {\n    return import(\"highlight.js/lib/index.js\");\n}\nexport const decode = decodeHtmlEntityAt;\n",
    );
    s
}

pub const HIGHLIGHT_INDEX: &str = "node_modules/highlight.js/lib/index.js";
pub const SYNTAX_HIGHLIGHT: &str =
    "node_modules/@earendil-works/pi-coding-agent/dist/utils/syntax-highlight.js";

/// The two files `trim_highlight` rewrites, in a stage.
pub fn write_highlight_stage(stage: &Path, index: &[&str], eager: &[&str]) {
    write(
        &stage.join("node_modules/highlight.js/package.json"),
        r#"{"name": "highlight.js", "version": "10.7.3"}"#,
    );
    write(&stage.join(HIGHLIGHT_INDEX), &highlight_index_js(index));
    write(&stage.join(SYNTAX_HIGHLIGHT), &syntax_highlight_js(eager));
}
