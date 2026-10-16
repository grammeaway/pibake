//! l7: the setup fingerprint (Rust, at build time) and its check (`bundle/staleness.mjs`, at
//! startup) on fixture setups. Tests running the JS need `node`, so they sit behind PIBAKE_E2E.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use common::{commit, e2e_enabled, git, write};
use hegel::TestCase;
use hegel::generators as gs;
use pibake::lock::Host;
use pibake::staleness::{self, Fingerprint};
use pibake::{bundle, inventory};
use serde_json::{Value, json};

struct Setup {
    _root: tempfile::TempDir,
    agent: PathBuf,
    home: PathBuf,
    host: Host,
}

impl Setup {
    /// npm packages (`name`, `version`), git packages (`host/owner/repo`), extension files.
    fn new(npm: &[(&str, &str)], git_repos: &[&str], extensions: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        let home = root.path().join("home");
        let pi_dir = root.path().join("pi");
        write(&pi_dir.join("package.json"), r#"{"version": "1.3.0"}"#);
        // Text, not `json!` (which sorts keys): `source` first, as Pi's docs write it, so
        // recorded and live key orders differ.
        let mut packages: Vec<String> = vec![];
        for (name, version) in npm {
            let dir = agent.join("npm/node_modules").join(name);
            write(
                &dir.join("package.json"),
                &json!({ "name": name, "version": version }).to_string(),
            );
            packages.push(json!(format!("npm:{name}")).to_string());
        }
        for repo in git_repos {
            let dir = agent.join("git").join(repo);
            fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", "main"]);
            commit(&dir, "index.ts", "export default () => {};\n");
            packages.push(format!(
                r#"{{"source": "git:{repo}", "extensions": ["index.ts"]}}"#
            ));
        }
        write(
            &agent.join("settings.json"),
            &format!(
                r#"{{"theme": "dark", "packages": [{}]}}"#,
                packages.join(", ")
            ),
        );
        for (path, content) in extensions {
            write(&agent.join("extensions").join(path), content);
        }
        Self {
            _root: root,
            agent,
            home,
            host: Host {
                pi_dir,
                pi_version: "1.3.0".into(),
                node_version: "26.11.1".into(),
            },
        }
    }

    fn fingerprint(&self) -> Fingerprint {
        let inventory = inventory::read(&self.agent, &self.home).unwrap();
        staleness::read(&inventory, &self.host).unwrap()
    }

    fn settings(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.agent.join("settings.json")).unwrap())
            .unwrap()
    }

    fn set_settings(&self, value: &Value) {
        fs::write(self.agent.join("settings.json"), value.to_string()).unwrap();
    }

    /// Rewrites an extension file with a later mtime, so the change never hides in one tick of
    /// the filesystem's clock.
    fn write_extension(&self, path: &str, content: &str) {
        let file = self.agent.join("extensions").join(path);
        let later = fs::metadata(&file)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::now())
            + Duration::from_secs(1);
        write(&file, content);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
}

// --- the Rust side ---

#[test]
fn records_packages_versions_heads_and_extension_files() {
    let setup = Setup::new(
        &[("alpha", "1.0.0"), ("@s/beta", "2.0.0")],
        &["github.com/o/r"],
        &[("a.ts", "a"), ("lib/b.ts", "bb")],
    );
    let head = git(
        &setup.agent.join("git/github.com/o/r"),
        &["rev-parse", "HEAD"],
    );
    let fp = setup.fingerprint();

    assert_eq!(
        fp.pi.path,
        setup.host.pi_dir.join("package.json").to_str().unwrap()
    );
    assert_eq!(fp.pi.version, "1.3.0");
    assert_eq!(fp.packages, setup.settings()["packages"]);
    let npm: Vec<_> = fp
        .npm
        .iter()
        .map(|p| (p.source.as_str(), p.path.as_str(), p.version.as_deref()))
        .collect();
    assert_eq!(
        npm,
        [
            ("npm:alpha", "npm/node_modules/alpha", Some("1.0.0")),
            ("npm:@s/beta", "npm/node_modules/@s/beta", Some("2.0.0")),
        ]
    );
    assert_eq!(fp.git.len(), 1);
    assert_eq!(fp.git[0].source, "git:github.com/o/r");
    assert_eq!(fp.git[0].path, "git/github.com/o/r");
    assert_eq!(fp.git[0].commit, head);

    let ext: Vec<_> = fp
        .extensions
        .iter()
        .map(|e| (e.path.as_str(), e.size, e.sha256.clone()))
        .collect();
    let sha = pibake::lock::sha256_hex;
    assert_eq!(ext, [("a.ts", 1, sha(b"a")), ("lib/b.ts", 2, sha(b"bb"))]);
    let meta = fs::metadata(setup.agent.join("extensions/a.ts")).unwrap();
    let since = meta
        .modified()
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    assert_eq!(fp.extensions[0].mtime_ns, since.as_nanos().to_string());
}

#[test]
fn absent_packages_and_extensions_record_as_empty() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    write(&agent.join("settings.json"), "{}");
    let host = Host {
        pi_dir: root.path().join("pi"),
        pi_version: "1.3.0".into(),
        node_version: "26.11.1".into(),
    };
    let inventory = inventory::read(&agent, root.path()).unwrap();
    let fp = staleness::read(&inventory, &host).unwrap();
    assert_eq!(fp.packages, Value::Null);
    assert!(fp.npm.is_empty() && fp.git.is_empty() && fp.extensions.is_empty());
}

#[test]
fn a_symlinked_extension_file_is_followed_and_a_symlinked_directory_refused() {
    let setup = Setup::new(&[], &[], &[("a.ts", "a")]);
    let outside = setup.agent.parent().unwrap().join("outside");
    write(&outside.join("shared.ts"), "shared");
    std::os::unix::fs::symlink(
        outside.join("shared.ts"),
        setup.agent.join("extensions/shared.ts"),
    )
    .unwrap();
    let fp = setup.fingerprint();
    assert_eq!(fp.extensions[1].path, "shared.ts");
    assert_eq!(fp.extensions[1].sha256, pibake::lock::sha256_hex(b"shared"));

    std::os::unix::fs::symlink(&outside, setup.agent.join("extensions/dir")).unwrap();
    let inventory = inventory::read(&setup.agent, &setup.home).unwrap();
    let err = staleness::read(&inventory, &setup.host).unwrap_err();
    assert!(
        format!("{err:#}").contains("symlinked directory"),
        "{err:#}"
    );
}

#[test]
fn a_git_package_that_is_not_a_checkout_is_an_error() {
    let setup = Setup::new(&[], &["github.com/o/r"], &[]);
    fs::remove_dir_all(setup.agent.join("git/github.com/o/r/.git")).unwrap();
    let inventory = inventory::read(&setup.agent, &setup.home).unwrap();
    let err = staleness::read(&inventory, &setup.host).unwrap_err();
    assert!(format!("{err:#}").contains("git:github.com/o/r"), "{err:#}");
}

// --- the JS side, against the Rust side ---

/// Runs `cmd` of a driver around the exact `pibake:staleness` text the bundle embeds.
fn js(cmd: &str, agent: &Path, recorded: &Fingerprint, envs: &[(&str, &str)]) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("staleness.mjs"), bundle::STALENESS).unwrap();
    fs::write(
        dir.path().join("driver.mjs"),
        r#"import { readFileSync } from "node:fs";
import { checkSetup, currentSetup, setupChanges } from "./staleness.mjs";
const [cmd, agent, file] = process.argv.slice(2);
const recorded = JSON.parse(readFileSync(file, "utf8"));
if (cmd === "current") process.stdout.write(JSON.stringify(currentSetup(agent, recorded)));
else if (cmd === "changes") process.stdout.write(JSON.stringify(setupChanges(recorded, currentSetup(agent, recorded))));
else checkSetup(agent, recorded);
"#,
    )
    .unwrap();
    let recorded_file = dir.path().join("recorded.json");
    fs::write(&recorded_file, serde_json::to_vec(recorded).unwrap()).unwrap();
    let out = Command::new("node")
        .arg(dir.path().join("driver.mjs"))
        .arg(cmd)
        .arg(agent)
        .arg(&recorded_file)
        .env_remove("PIBAKE_NO_STALENESS_CHECK")
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn changes(setup: &Setup, recorded: &Fingerprint) -> Vec<String> {
    serde_json::from_str(&js("changes", &setup.agent, recorded, &[]).0).unwrap()
}

/// The JS reading of the setup, with every extension file hashed (nothing recorded to reuse).
fn js_fingerprint(setup: &Setup, recorded: &Fingerprint) -> Fingerprint {
    let blind = Fingerprint {
        extensions: vec![],
        ..recorded.clone()
    };
    let mut fp: Fingerprint =
        serde_json::from_str(&js("current", &setup.agent, &blind, &[]).0).unwrap();
    fp.extensions.sort_by(|a, b| a.path.cmp(&b.path));
    fp
}

#[test]
fn e2e_js_reads_the_setup_as_rust_records_it() {
    if !e2e_enabled() {
        return;
    }
    let setup = Setup::new(
        &[("alpha", "1.0.0"), ("@s/beta", "2.0.0-rc.1")],
        &["github.com/o/r", "gitlab.com/g/sub/r"],
        &[("a.ts", "a"), ("lib/b.ts", "b"), ("empty.ts", "")],
    );
    let fp = setup.fingerprint();
    assert_eq!(js_fingerprint(&setup, &fp), fp);

    // HEAD through a packed ref, and detached.
    let repo = setup.agent.join("git/github.com/o/r");
    git(&repo, &["pack-refs", "--all"]);
    assert!(!repo.join(".git/refs/heads/main").exists());
    assert_eq!(js_fingerprint(&setup, &fp), fp);
    let first = commit(&repo, "next.ts", "x");
    git(&repo, &["checkout", "-q", "--detach", "HEAD"]);
    let fp = setup.fingerprint();
    assert_eq!(fp.git[0].commit, first);
    assert_eq!(js_fingerprint(&setup, &fp), fp);

    // No settings.json and no extensions/.
    fs::remove_file(setup.agent.join("settings.json")).unwrap();
    fs::remove_dir_all(setup.agent.join("extensions")).unwrap();
    let fp = setup.fingerprint();
    assert_eq!(fp.packages, Value::Null);
    assert_eq!(js_fingerprint(&setup, &fp), fp);
    assert_eq!(changes(&setup, &fp), Vec::<String>::new());
}

#[test]
fn e2e_each_trigger_is_named_and_an_unchanged_setup_is_silent() {
    if !e2e_enabled() {
        return;
    }
    let setup = Setup::new(
        &[("alpha", "1.0.0")],
        &["github.com/o/r"],
        &[("a.ts", "aaaa"), ("lib/b.ts", "b")],
    );
    let fp = setup.fingerprint();
    assert_eq!(changes(&setup, &fp), Vec::<String>::new());

    // Key order and other settings do not count.
    let mut settings = setup.settings();
    settings["packages"][1] = json!({ "extensions": ["index.ts"], "source": "git:github.com/o/r" });
    settings["theme"] = json!("light");
    setup.set_settings(&settings);
    assert_eq!(changes(&setup, &fp), Vec::<String>::new());

    // Package added.
    let mut added = settings.clone();
    added["packages"]
        .as_array_mut()
        .unwrap()
        .push(json!("npm:gamma"));
    setup.set_settings(&added);
    assert_eq!(changes(&setup, &fp), ["package added: npm:gamma"]);
    // A filter changed.
    let mut filtered = settings.clone();
    filtered["packages"][1]["extensions"] = json!([]);
    setup.set_settings(&filtered);
    assert_eq!(changes(&setup, &fp), ["settings.json packages changed"]);
    // Removed: named once, not also as a missing install.
    let mut removed = settings.clone();
    removed["packages"].as_array_mut().unwrap().remove(0);
    setup.set_settings(&removed);
    fs::remove_dir_all(setup.agent.join("npm/node_modules/alpha")).unwrap();
    assert_eq!(changes(&setup, &fp), ["package removed: npm:alpha"]);
    setup.set_settings(&settings);
    assert_eq!(changes(&setup, &fp), ["npm:alpha 1.0.0 -> missing"]);

    // Version bumped.
    write(
        &setup.agent.join("npm/node_modules/alpha/package.json"),
        r#"{"version": "1.1.0"}"#,
    );
    assert_eq!(changes(&setup, &fp), ["npm:alpha 1.0.0 -> 1.1.0"]);
    write(
        &setup.agent.join("npm/node_modules/alpha/package.json"),
        r#"{"version": "1.0.0"}"#,
    );

    // git HEAD moved.
    let repo = setup.agent.join("git/github.com/o/r");
    let moved = commit(&repo, "next.ts", "x");
    assert_eq!(
        changes(&setup, &fp),
        [format!(
            "git:github.com/o/r {} -> {}",
            &fp.git[0].commit[..12],
            &moved[..12]
        )]
    );
    git(&repo, &["reset", "-q", "--hard", &fp.git[0].commit]);
    assert_eq!(changes(&setup, &fp), Vec::<String>::new());

    // Local extension edited, same size; touched only; added; removed.
    setup.write_extension("a.ts", "AAAA");
    assert_eq!(changes(&setup, &fp), ["extensions/a.ts edited"]);
    setup.write_extension("a.ts", "aaaa");
    assert_eq!(changes(&setup, &fp), Vec::<String>::new());
    write(&setup.agent.join("extensions/lib/c.ts"), "c");
    fs::remove_file(setup.agent.join("extensions/lib/b.ts")).unwrap();
    assert_eq!(
        changes(&setup, &fp),
        ["extensions/lib/b.ts removed", "extensions/lib/c.ts added"]
    );
    fs::remove_file(setup.agent.join("extensions/lib/c.ts")).unwrap();
    write(&setup.agent.join("extensions/lib/b.ts"), "b");

    // The global Pi updated.
    write(
        &setup.host.pi_dir.join("package.json"),
        r#"{"version": "1.4.0"}"#,
    );
    assert_eq!(changes(&setup, &fp), ["pi 1.3.0 -> 1.4.0"]);
}

#[test]
fn e2e_the_check_prints_one_line_only_when_stale_and_not_when_skipped() {
    if !e2e_enabled() {
        return;
    }
    let setup = Setup::new(&[("alpha", "1.0.0")], &[], &[("a.ts", "a")]);
    let fp = setup.fingerprint();
    let check = |envs: &[(&str, &str)]| {
        let (stdout, stderr) = js("check", &setup.agent, &fp, envs);
        assert_eq!(stdout, "");
        stderr
    };
    assert_eq!(check(&[]), "");

    for (i, name) in ["b", "c", "d", "e"].iter().enumerate() {
        write(&setup.agent.join(format!("extensions/{name}.ts")), "x");
        let stderr = check(&[]);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(
            stderr.starts_with("node: built from an older setup (extensions/b.ts added"),
            "{stderr}"
        );
        assert!(stderr.ends_with("); run pibake install\n"), "{stderr}");
        assert_eq!(stderr.contains("1 more"), i == 3, "{stderr}");
    }
    assert_eq!(check(&[("PIBAKE_NO_STALENESS_CHECK", "1")]), "");

    // A setup the check cannot read: one line saying so, and Pi still starts.
    fs::write(setup.agent.join("settings.json"), "{ not json").unwrap();
    let stderr = check(&[]);
    assert!(
        stderr.starts_with("node: checking the setup failed: "),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

/// One change to a setup, and the phrases it must produce (empty: no change that matters).
#[derive(Debug, Clone, Copy)]
enum Mutation {
    None,
    TouchExtension,
    ReorderSettingsKeys,
    AddPackage,
    RemovePackage,
    BumpNpm,
    MoveGitHead,
    EditExtension,
    AddExtension,
    RemoveExtension,
    BumpPi,
}

const MUTATIONS: [Mutation; 11] = [
    Mutation::None,
    Mutation::TouchExtension,
    Mutation::ReorderSettingsKeys,
    Mutation::AddPackage,
    Mutation::RemovePackage,
    Mutation::BumpNpm,
    Mutation::MoveGitHead,
    Mutation::EditExtension,
    Mutation::AddExtension,
    Mutation::RemoveExtension,
    Mutation::BumpPi,
];

fn mutate(tc: &TestCase, setup: &Setup, fp: &Fingerprint, m: Mutation) -> Vec<String> {
    let pick = |n: usize| tc.draw(gs::integers::<usize>().max_value(n - 1));
    let ext = |fp: &Fingerprint| fp.extensions[pick(fp.extensions.len())].path.clone();
    match m {
        Mutation::None => vec![],
        Mutation::TouchExtension if !fp.extensions.is_empty() => {
            let path = ext(fp);
            let content = fs::read_to_string(setup.agent.join("extensions").join(&path)).unwrap();
            setup.write_extension(&path, &content);
            vec![]
        }
        Mutation::ReorderSettingsKeys => {
            let settings = setup.settings();
            let text = format!(
                "{{\"packages\": {}, \"theme\": \"other\"}}",
                settings["packages"]
            );
            fs::write(setup.agent.join("settings.json"), text).unwrap();
            vec![]
        }
        Mutation::AddPackage => {
            let mut settings = setup.settings();
            settings["packages"]
                .as_array_mut()
                .unwrap()
                .push(json!("npm:zz-new"));
            setup.set_settings(&settings);
            vec!["package added: npm:zz-new".into()]
        }
        Mutation::RemovePackage if !fp.npm.is_empty() => {
            let i = pick(fp.npm.len());
            let mut settings = setup.settings();
            settings["packages"].as_array_mut().unwrap().remove(i);
            setup.set_settings(&settings);
            vec![format!("package removed: {}", fp.npm[i].source)]
        }
        Mutation::BumpNpm if !fp.npm.is_empty() => {
            let p = &fp.npm[pick(fp.npm.len())];
            write(
                &setup.agent.join(&p.path).join("package.json"),
                r#"{"version": "9.9.9"}"#,
            );
            vec![format!(
                "{} {} -> 9.9.9",
                p.source,
                p.version.as_deref().unwrap()
            )]
        }
        Mutation::MoveGitHead if !fp.git.is_empty() => {
            let g = &fp.git[0];
            let moved = commit(&setup.agent.join(&g.path), "next.ts", "x");
            vec![format!(
                "{} {} -> {}",
                g.source,
                &g.commit[..12],
                &moved[..12]
            )]
        }
        Mutation::EditExtension if !fp.extensions.is_empty() => {
            let path = ext(fp);
            let file = setup.agent.join("extensions").join(&path);
            let mut content = fs::read(&file).unwrap();
            content.push(b'!');
            setup.write_extension(&path, &String::from_utf8(content).unwrap());
            vec![format!("extensions/{path} edited")]
        }
        Mutation::AddExtension => {
            write(&setup.agent.join("extensions/zz/new.ts"), "new");
            vec!["extensions/zz/new.ts added".into()]
        }
        Mutation::RemoveExtension if !fp.extensions.is_empty() => {
            let path = ext(fp);
            fs::remove_file(setup.agent.join("extensions").join(&path)).unwrap();
            vec![format!("extensions/{path} removed")]
        }
        Mutation::BumpPi => {
            write(
                &setup.host.pi_dir.join("package.json"),
                r#"{"version": "9.0.0"}"#,
            );
            vec!["pi 1.3.0 -> 9.0.0".into()]
        }
        // Nothing to apply it to in this setup.
        _ => vec![],
    }
}

/// Over random setups and one random change each: JS reads what Rust records, and the check
/// names exactly the change, or nothing when the change does not touch baked code.
#[hegel::test(test_cases = 40)]
fn e2e_the_check_names_exactly_what_changed(tc: TestCase) {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        return;
    }
    let names: Vec<String> = tc.draw(
        gs::vecs(gs::from_regex(r"[a-y][a-z0-9-]{0,6}"))
            .max_size(3)
            .unique(true),
    );
    let versions: Vec<String> = names
        .iter()
        .map(|_| tc.draw(gs::from_regex(r"[0-9]\.[0-9]{1,2}\.[0-9]")))
        .collect();
    let npm: Vec<(&str, &str)> = names
        .iter()
        .zip(&versions)
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let with_git = tc.draw(gs::booleans());
    let files: Vec<String> = tc.draw(
        gs::vecs(gs::from_regex(r"[a-y]{1,4}(/[a-z]{1,4})?\.ts"))
            .max_size(4)
            .unique(true),
    );
    let contents: Vec<String> = files
        .iter()
        .map(|_| tc.draw(gs::text().max_size(40)))
        .collect();
    let extensions: Vec<(&str, &str)> = files
        .iter()
        .zip(&contents)
        .map(|(f, c)| (f.as_str(), c.as_str()))
        .collect();
    let setup = Setup::new(
        &npm,
        if with_git { &["github.com/o/r"] } else { &[] },
        &extensions,
    );
    let fp = setup.fingerprint();
    assert_eq!(js_fingerprint(&setup, &fp), fp);

    let m = MUTATIONS[tc.draw(gs::integers::<usize>().max_value(MUTATIONS.len() - 1))];
    let expected = mutate(&tc, &setup, &fp, m);
    assert_eq!(changes(&setup, &fp), expected, "{m:?}");
}
