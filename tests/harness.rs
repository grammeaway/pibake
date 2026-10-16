use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pibake::harness::{Harness, PiCommand};
use pibake::{inventory, paths};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_dir")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Every path under the given top-level entries, with type, size, mtime and link target.
/// Symlinks are recorded, not followed.
fn fingerprint(dir: &Path, names: &[String]) -> Vec<String> {
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

fn all_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn harness_copies_config_links_packages_and_skips_state() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("agent");
    copy_dir(&fixture(), &source);
    let real_home = scratch.path().join("home");
    fs::create_dir_all(real_home.join(".agents/skills")).unwrap();
    fs::write(
        source.join("models.json"),
        r#"{"providers":{"local":{"api":"openai-completions","apiKey":"!pass show x","models":[{"id":"m"}]}}}"#,
    )
    .unwrap();
    let before = fingerprint(&source, &all_names(&source));
    let inv = inventory::read(&source, &real_home).unwrap();

    let harness = Harness::new(&source, &real_home, "http://127.0.0.1:1/v1").unwrap();
    let agent = &harness.agent_dir;
    assert_eq!(agent, &harness.home.join(".pi/agent"));
    assert!(harness.cwd.is_dir() && !harness.cwd.starts_with(&harness.home));
    assert_eq!(
        fs::read_link(harness.home.join(".agents")).unwrap(),
        real_home.join(".agents")
    );

    let staged: BTreeSet<String> = all_names(agent).into_iter().collect();
    let expected: BTreeSet<String> = inv
        .config
        .iter()
        .chain(&inv.install_roots)
        .cloned()
        .chain(["models.json".to_string()])
        .collect();
    assert_eq!(staged, expected);
    for state in &inv.state {
        assert!(
            !agent.join(state).exists(),
            "state entry {state} was staged"
        );
    }
    assert!(inv.state.contains(&"auth.json".to_string()));

    for name in &staged {
        let (from, to) = (source.join(name), agent.join(name));
        if name == "models.json" {
            continue;
        }
        if from.is_dir() {
            assert_eq!(fs::read_link(&to).unwrap(), from, "{name} is linked");
        } else {
            assert!(
                !fs::symlink_metadata(&to).unwrap().is_symlink(),
                "{name} is copied"
            );
            assert_eq!(fs::read(&to).unwrap(), fs::read(&from).unwrap());
        }
    }
    let models: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(agent.join("models.json")).unwrap()).unwrap();
    assert_eq!(
        models["providers"]["pibake_fake"]["baseUrl"],
        "http://127.0.0.1:1/v1"
    );
    let source_models: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(source.join("models.json")).unwrap()).unwrap();
    assert_eq!(
        models["providers"]["local"],
        source_models["providers"]["local"]
    );

    let root = harness.root().to_path_buf();
    drop(harness);
    assert!(!root.exists(), "harness removed on drop");
    assert_eq!(
        fingerprint(&source, &all_names(&source)),
        before,
        "source changed"
    );
}

#[test]
fn harness_commands_run_with_a_cleared_environment() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("agent");
    copy_dir(&fixture(), &source);
    let harness = Harness::new(&source, scratch.path(), "http://127.0.0.1:1/v1").unwrap();
    let env = PiCommand::parse("env").unwrap();
    let out = harness
        .command(&env, "env.log")
        .unwrap()
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let vars: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
    assert_eq!(get("HOME"), Some(harness.home.to_str().unwrap()));
    assert_eq!(
        get("PI_CODING_AGENT_DIR"),
        Some(harness.agent_dir.to_str().unwrap())
    );
    assert_eq!(get("PI_OFFLINE"), Some("1"));
    let allowed = [
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "HOME",
        "PI_CODING_AGENT_DIR",
        "PI_OFFLINE",
        "PI_SKIP_VERSION_CHECK",
        "PI_TELEMETRY",
    ];
    for (name, _) in &vars {
        assert!(
            allowed.contains(name),
            "{name} leaked into the Pi environment"
        );
    }
}

/// Runs stock Pi against the live agent directory, so it is opt-in. Reads `~/.pi/agent`;
/// asserts nothing under its config and install roots changed.
#[test]
fn e2e_verify_stock_pi_against_itself_and_without_extensions() {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        eprintln!("skipped: set PIBAKE_E2E=1");
        return;
    }
    let home = paths::home_dir().unwrap();
    let live = home.join(".pi/agent");
    let inv = inventory::read(&live, &home).unwrap();
    let watched: Vec<String> = inv
        .config
        .iter()
        .chain(&inv.install_roots)
        .cloned()
        .collect();
    let before = fingerprint(&live, &watched);
    let cwd = tempfile::tempdir().unwrap();
    let verify = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_pibake"))
            .arg("verify")
            .args(args)
            .current_dir(cwd.path())
            .output()
            .unwrap()
    };

    let same = verify(&["--pi", "pi", "--against", "pi"]);
    let stdout = String::from_utf8_lossy(&same.stdout);
    assert!(
        same.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&same.stderr)
    );
    assert!(stdout.starts_with("same: "), "{stdout}");

    let bare = verify(&["--pi", "pi", "--against", "pi -ne"]);
    let report = String::from_utf8_lossy(&bare.stdout);
    assert_eq!(
        bare.status.code(),
        Some(1),
        "{report}\n{}",
        String::from_utf8_lossy(&bare.stderr)
    );
    let missing = report
        .lines()
        .find(|l| l.starts_with("only in `pi`: "))
        .unwrap_or_else(|| panic!("no tools named as missing:\n{report}"));
    let names: Vec<&str> = missing["only in `pi`: ".len()..].split(", ").collect();
    assert!(
        !names.contains(&"read"),
        "built-in tools stay with -ne: {missing}"
    );

    assert_eq!(fingerprint(&live, &watched), before, "~/.pi/agent changed");
}
