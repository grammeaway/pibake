mod common;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::Result;
use common::{commit, git, write};
use pibake::inventory;
use pibake::lock::{
    ESBUILD_VERSION, Host, LOCK_FILE, Lock, Locker, NPM_LOCK_FILE, Outcome, PI_PACKAGE,
};
use serde_json::{Value, json};

fn install_npm(agent: &Path, name: &str, version: &str) {
    write(
        &agent
            .join("npm/node_modules")
            .join(name)
            .join("package.json"),
        &json!({"name": name, "version": version}).to_string(),
    );
}

/// An agent dir with two npm packages, a git checkout and a local package, plus a stock-Pi stand-in.
struct Setup {
    root: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        write(
            &agent.join("settings.json"),
            &json!({"packages": [
                "npm:pi-btw",
                {"source": "npm:@s/dep@^1", "extensions": []},
                "git:example.com/o/r@main",
                "./local-pkg"
            ]})
            .to_string(),
        );
        install_npm(&agent, "pi-btw", "0.8.0");
        install_npm(&agent, "@s/dep", "1.2.3");
        write(&agent.join("npm/package-lock.json"), "{\"seed\": true}\n");
        let repo = agent.join("git/example.com/o/r");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        commit(&repo, "package.json", "{}");
        write(&agent.join("local-pkg/package.json"), "{}");
        write(
            &root.path().join("pi/package.json"),
            &json!({"name": PI_PACKAGE, "version": "1.3.0"}).to_string(),
        );
        fs::create_dir(root.path().join("project")).unwrap();
        Self { root }
    }

    fn agent(&self) -> PathBuf {
        self.root.path().join("agent")
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn repo(&self) -> PathBuf {
        self.agent().join("git/example.com/o/r")
    }

    fn host(&self) -> Host {
        Host {
            pi_dir: self.root.path().join("pi"),
            pi_version: "1.3.0".into(),
            node_version: "26.11.1".into(),
        }
    }

    /// Runs the locker with a stand-in for npm; returns the outcome and the warnings.
    fn lock(&self, update: bool, npm_runs: &Cell<usize>) -> Result<(Outcome, Vec<String>)> {
        let fake_npm = |dir: &Path| -> Result<()> {
            npm_runs.set(npm_runs.get() + 1);
            assert_eq!(
                fs::read_to_string(dir.join("package-lock.json")).unwrap(),
                "{\"seed\": true}\n",
                "the installed npm lock is the seed"
            );
            let package: Value =
                serde_json::from_str(&fs::read_to_string(dir.join("package.json"))?)?;
            let deps = package["dependencies"].as_object().unwrap();
            let dev = package["devDependencies"].as_object().unwrap();
            let mut packages = serde_json::Map::new();
            packages.insert(
                String::new(),
                json!({"name": package["name"], "dependencies": deps, "devDependencies": dev}),
            );
            for (name, version) in deps {
                packages.insert(
                    format!("node_modules/{name}"),
                    json!({"version": version, "integrity": format!("sha512-{name}")}),
                );
            }
            for (name, version) in dev {
                packages.insert(
                    format!("node_modules/{name}"),
                    json!({"version": version, "dev": true}),
                );
            }
            let lock = json!({"name": package["name"], "lockfileVersion": 3, "packages": packages});
            fs::write(
                dir.join("package-lock.json"),
                format!("{}\n", serde_json::to_string_pretty(&lock)?),
            )?;
            Ok(())
        };
        let host = self.host();
        let project = self.project();
        let agent = self.agent();
        let locker = Locker {
            agent_dir: &agent,
            dir: &project,
            host: &host,
            npm: &fake_npm,
        };
        let inventory = inventory::read(&agent, Path::new("/nonexistent-home"))?;
        let mut warnings = vec![];
        let outcome = locker.run(&inventory, update, &mut |w| warnings.push(w))?;
        Ok((outcome, warnings))
    }

    fn lock_files(&self) -> Vec<(Vec<u8>, SystemTime)> {
        [LOCK_FILE, NPM_LOCK_FILE]
            .iter()
            .map(|f| {
                let p = self.project().join(f);
                (
                    fs::read(&p).unwrap(),
                    fs::metadata(&p).unwrap().modified().unwrap(),
                )
            })
            .collect()
    }
}

fn drift(outcome: Outcome) -> Vec<String> {
    match outcome {
        Outcome::Drift(items) => items,
        _ => panic!("expected drift"),
    }
}

#[test]
fn creates_both_files_then_leaves_them_alone() {
    let setup = Setup::new();
    let npm_runs = Cell::new(0);
    let (outcome, warnings) = setup.lock(false, &npm_runs).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let Outcome::Written(files) = outcome else {
        panic!("expected a write")
    };
    assert_eq!(
        files,
        [
            setup.project().join(NPM_LOCK_FILE),
            setup.project().join(LOCK_FILE)
        ]
    );

    let lock = Lock::parse(&fs::read_to_string(setup.project().join(LOCK_FILE)).unwrap()).unwrap();
    assert_eq!(lock.pi.version, "1.3.0");
    assert_eq!(lock.pi.integrity, format!("sha512-{PI_PACKAGE}"));
    assert_eq!(lock.node.version, "26.11.1");
    assert_eq!(lock.git.len(), 1);
    assert_eq!(lock.git[0].name, "example.com/o/r");
    assert_eq!(lock.git[0].url, "https://example.com/o/r");
    assert_eq!(
        lock.git[0].commit,
        git(&setup.repo(), &["rev-parse", "HEAD"])
    );
    let npm_lock: Value =
        serde_json::from_slice(&fs::read(setup.project().join(NPM_LOCK_FILE)).unwrap()).unwrap();
    assert_eq!(
        npm_lock["packages"][""]["dependencies"],
        json!({PI_PACKAGE: "1.3.0", "@s/dep": "1.2.3", "pi-btw": "0.8.0"})
    );
    assert_eq!(
        npm_lock["packages"][""]["devDependencies"],
        json!({"esbuild": ESBUILD_VERSION})
    );

    let before = setup.lock_files();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (outcome, _) = setup.lock(false, &npm_runs).unwrap();
    assert!(matches!(outcome, Outcome::UpToDate));
    assert_eq!(setup.lock_files(), before);
    assert_eq!(npm_runs.get(), 1, "an up-to-date lock runs no npm");

    let (outcome, _) = setup.lock(true, &npm_runs).unwrap();
    assert!(matches!(outcome, Outcome::Written(ref f) if f.is_empty()));
    assert_eq!(setup.lock_files(), before);
}

#[test]
fn drift_is_reported_without_writing_and_update_repins() {
    let setup = Setup::new();
    let npm_runs = Cell::new(0);
    setup.lock(false, &npm_runs).unwrap();
    let before = setup.lock_files();

    install_npm(&setup.agent(), "pi-btw", "0.8.1");
    let old = git(&setup.repo(), &["rev-parse", "HEAD"]);
    let new = commit(&setup.repo(), "index.js", "");
    let items = drift(setup.lock(false, &npm_runs).unwrap().0);
    assert_eq!(
        items,
        [
            "npm pi-btw: locked 0.8.0, installed 0.8.1".to_owned(),
            format!(
                "git example.com/o/r: locked {old} (https://example.com/o/r), installed {new} (https://example.com/o/r)"
            ),
        ]
    );
    assert_eq!(setup.lock_files(), before);

    let (outcome, _) = setup.lock(true, &npm_runs).unwrap();
    assert!(matches!(outcome, Outcome::Written(ref f) if f.len() == 2));
    let lock = Lock::parse(&fs::read_to_string(setup.project().join(LOCK_FILE)).unwrap()).unwrap();
    assert_eq!(lock.git[0].commit, new);
    assert!(matches!(
        setup.lock(false, &npm_runs).unwrap().0,
        Outcome::UpToDate
    ));
}

#[test]
fn edited_or_missing_lock_files_are_drift() {
    let setup = Setup::new();
    let npm_runs = Cell::new(0);
    setup.lock(false, &npm_runs).unwrap();
    let npm_path = setup.project().join(NPM_LOCK_FILE);
    let original = fs::read(&npm_path).unwrap();

    fs::write(&npm_path, [&original[..], b" "].concat()).unwrap();
    let items = drift(setup.lock(false, &npm_runs).unwrap().0);
    assert!(
        items[0].starts_with("pibake.package-lock.json changed"),
        "{items:?}"
    );

    fs::remove_file(&npm_path).unwrap();
    assert_eq!(
        drift(setup.lock(false, &npm_runs).unwrap().0),
        ["pibake.package-lock.json is missing"]
    );

    fs::write(&npm_path, &original).unwrap();
    fs::remove_file(setup.project().join(LOCK_FILE)).unwrap();
    assert_eq!(
        drift(setup.lock(false, &npm_runs).unwrap().0),
        ["pibake.lock is missing"]
    );

    // A broken pibake.lock is an error to fix, or to replace with --update.
    fs::write(setup.project().join(LOCK_FILE), "[pi]\n").unwrap();
    assert!(setup.lock(false, &npm_runs).is_err());
    assert!(setup.lock(true, &npm_runs).is_ok());
    assert!(matches!(
        setup.lock(false, &npm_runs).unwrap().0,
        Outcome::UpToDate
    ));
}

/// A lock written before pibake pinned esbuild, or by a pibake pinning another version.
#[test]
fn an_esbuild_pin_other_than_pibakes_is_drift() {
    let setup = Setup::new();
    let npm_runs = Cell::new(0);
    setup.lock(false, &npm_runs).unwrap();
    let npm_path = setup.project().join(NPM_LOCK_FILE);
    let lock_path = setup.project().join(LOCK_FILE);
    let rewrite = |dev: Value| {
        let mut npm_lock: Value = serde_json::from_slice(&fs::read(&npm_path).unwrap()).unwrap();
        npm_lock["packages"][""]["devDependencies"] = dev;
        let bytes = serde_json::to_vec_pretty(&npm_lock).unwrap();
        fs::write(&npm_path, &bytes).unwrap();
        let mut lock = Lock::parse(&fs::read_to_string(&lock_path).unwrap()).unwrap();
        lock.npm_lock.sha256 = pibake::lock::sha256_hex(&bytes);
        fs::write(&lock_path, lock.render()).unwrap();
    };

    rewrite(json!({}));
    assert_eq!(
        drift(setup.lock(false, &npm_runs).unwrap().0),
        [format!(
            "esbuild: locked none, pibake uses {ESBUILD_VERSION}"
        )]
    );
    rewrite(json!({"esbuild": "0.1.0"}));
    assert_eq!(
        drift(setup.lock(false, &npm_runs).unwrap().0),
        [format!(
            "esbuild: locked 0.1.0, pibake uses {ESBUILD_VERSION}"
        )]
    );
    setup.lock(true, &npm_runs).unwrap();
    assert!(matches!(
        setup.lock(false, &npm_runs).unwrap().0,
        Outcome::UpToDate
    ));
}

#[test]
fn a_declared_package_must_be_installed() {
    let setup = Setup::new();
    fs::remove_dir_all(setup.agent().join("npm/node_modules/@s/dep")).unwrap();
    let err = setup.lock(false, &Cell::new(0)).err().unwrap();
    assert!(
        format!("{err:#}").contains("npm:@s/dep@^1 is in settings.json but not installed"),
        "{err:#}"
    );
}

#[test]
fn git_packages_must_be_checkouts_and_local_changes_warn() {
    let setup = Setup::new();
    write(&setup.repo().join("package.json"), "{\"changed\": true}");
    let (_, warnings) = setup.lock(false, &Cell::new(0)).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("local changes"), "{warnings:?}");

    // Untracked files (Pi's own `npm install` leaves a package-lock.json) are not changes.
    let setup = Setup::new();
    write(&setup.repo().join("package-lock.json"), "{}");
    let (_, warnings) = setup.lock(false, &Cell::new(0)).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    fs::remove_dir_all(setup.repo().join(".git")).unwrap();
    let err = setup.lock(true, &Cell::new(0)).err().unwrap();
    assert!(format!("{err:#}").contains("not a git checkout"), "{err:#}");
}

#[test]
fn the_lock_follows_npm_not_the_request() {
    let setup = Setup::new();
    let host = setup.host();
    let (project, agent) = (setup.project(), setup.agent());
    let wrong_npm = |dir: &Path| -> Result<()> {
        let lock = json!({"packages": {"": {"dependencies": {PI_PACKAGE: "1.3.0"}}}});
        fs::write(dir.join("package-lock.json"), lock.to_string())?;
        Ok(())
    };
    let locker = Locker {
        agent_dir: &agent,
        dir: &project,
        host: &host,
        npm: &wrong_npm,
    };
    let inventory = inventory::read(&agent, Path::new("/nonexistent-home")).unwrap();
    let err = locker.run(&inventory, false, &mut |_| {}).err().unwrap();
    assert!(format!("{err:#}").contains("does not list"), "{err:#}");
    assert!(!project.join(LOCK_FILE).exists());
}

/// Real `pi`, `node` and npm (network): a fixture setup with `pi-btw` installed at 0.8.0.
#[test]
fn e2e_lock_with_npm_detects_a_version_bump() {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        eprintln!("skipped: set PIBAKE_E2E=1");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    write(
        &agent.join("settings.json"),
        r#"{"packages": ["npm:pi-btw"]}"#,
    );
    install_npm(&agent, "pi-btw", "0.8.0");
    write(
        &root.path().join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", agent.display()),
    );
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_pibake"))
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };
    let mtimes = || -> BTreeMap<&str, SystemTime> {
        [LOCK_FILE, NPM_LOCK_FILE]
            .into_iter()
            .map(|f| {
                let m = fs::metadata(root.path().join(f)).unwrap();
                (f, m.modified().unwrap())
            })
            .collect()
    };
    let locked_btw = || -> Value {
        let npm_lock: Value =
            serde_json::from_slice(&fs::read(root.path().join(NPM_LOCK_FILE)).unwrap()).unwrap();
        npm_lock["packages"]["node_modules/pi-btw"]["version"].clone()
    };

    let (code, text) = run(&["lock"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(locked_btw(), "0.8.0");
    let first = mtimes();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (code, text) = run(&["lock"]);
    assert_eq!((code, text.trim()), (Some(0), "lock up to date"));
    assert_eq!(mtimes(), first);

    install_npm(&agent, "pi-btw", "0.7.1");
    let (code, text) = run(&["lock"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(
        text.contains("npm pi-btw: locked 0.8.0, installed 0.7.1"),
        "{text}"
    );
    assert_eq!(mtimes(), first);

    let (code, text) = run(&["lock", "--update"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(locked_btw(), "0.7.1");
    let (code, text) = run(&["lock"]);
    assert_eq!((code, text.trim()), (Some(0), "lock up to date"));
}
