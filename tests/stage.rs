mod common;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use common::{commit, git, sample_npm_lock, write, write_lock_files};
use pibake::inventory;
use pibake::lock::{ESBUILD_VERSION, GitPin, NPM_LOCK_FILE, PI_PACKAGE};
use pibake::stage::{Inputs, MARKER, NPM_CI, Outcome, Stager};
use serde_json::{Value, json};

/// A project with lock files, an agent dir with local extensions, and a git remote whose
/// locked commit is behind its HEAD.
struct Fixture {
    root: tempfile::TempDir,
    locked_commit: String,
}

/// The checkout directory's name, and npm's arguments.
type NpmCall = (String, Vec<String>);

#[derive(Clone, Copy, PartialEq)]
enum FakeNpm {
    Installs,
    Fails,
    InstallsWrongVersion,
}

impl Fixture {
    fn new(git_package_json: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        write(&agent.join("settings.json"), "{}");
        write(
            &agent.join("extensions/init.ts"),
            "export default () => {};\n",
        );
        write(
            &agent.join("extensions/sub/index.ts"),
            "export default 1;\n",
        );
        let remote = root.path().join("remote");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "-q"]);
        commit(&remote, "package.json", git_package_json);
        let locked_commit = commit(&remote, "index.js", "locked\n");
        commit(&remote, "index.js", "newer\n");
        let fixture = Self {
            root,
            locked_commit,
        };
        fixture.write_lock(sample_npm_lock());
        fixture
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn write_lock(&self, npm_lock: Value) {
        let pin = GitPin {
            name: "example.com/o/r".into(),
            url: self.path("remote").display().to_string(),
            commit: self.locked_commit.clone(),
        };
        write_lock_files(&self.path("project"), &npm_lock, vec![pin]);
    }

    fn inputs(&self) -> Result<Inputs> {
        let inventory = inventory::read(&self.path("agent"), Path::new("/nonexistent-home"))?;
        Inputs::read(&self.path("project"), &inventory)
    }

    fn stage_root(&self) -> PathBuf {
        self.path("project/target/pibake/stage")
    }

    /// Stages with a stand-in npm; returns the outcome, npm's calls and the log.
    fn stage(&self, npm: FakeNpm) -> (Result<Outcome>, Vec<NpmCall>, Vec<String>) {
        let calls = RefCell::new(vec![]);
        let fake = |dir: &Path, args: &[&str]| -> Result<()> {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            calls
                .borrow_mut()
                .push((name, args.iter().map(|a| a.to_string()).collect()));
            if args != NPM_CI {
                return Ok(());
            }
            if npm == FakeNpm::Fails {
                bail!("npm failed");
            }
            let lock: Value = serde_json::from_slice(&fs::read(dir.join("package-lock.json"))?)?;
            let package: Value = serde_json::from_slice(&fs::read(dir.join("package.json"))?)?;
            // As npm ci does: package.json and the lock's root must agree.
            for field in ["dependencies", "devDependencies"] {
                if package[field] != lock["packages"][""][field] {
                    bail!("package.json {field} out of sync with the lock");
                }
            }
            for (path, entry) in lock["packages"].as_object().unwrap() {
                if path.is_empty() || entry["optional"] == true {
                    continue;
                }
                let mut version = entry["version"].as_str().unwrap().to_owned();
                if npm == FakeNpm::InstallsWrongVersion && path == "node_modules/pi-btw" {
                    version = "0.0.1".into();
                }
                write(
                    &dir.join(path).join("package.json"),
                    &json!({"version": version}).to_string(),
                );
            }
            Ok(())
        };
        let root = self.stage_root();
        let stager = Stager {
            root: &root,
            npm: &fake,
        };
        let mut log = vec![];
        let outcome = self
            .inputs()
            .and_then(|inputs| stager.run(&inputs, &mut |m| log.push(m)));
        (outcome, calls.into_inner(), log)
    }

    fn stage_root_entries(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.stage_root()) else {
            return vec![];
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn staged(outcome: Result<Outcome>) -> PathBuf {
    match outcome.unwrap() {
        Outcome::Built(dir) => dir,
        Outcome::Cached(dir) => panic!("expected a fresh stage, got the cached {}", dir.display()),
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn stages_the_lock_then_reuses_it() {
    let f = Fixture::new("{}");
    let hash = f.inputs().unwrap().hash().to_owned();
    let (outcome, calls, _) = f.stage(FakeNpm::Installs);
    let dir = staged(outcome);
    assert_eq!(dir, f.stage_root().join(&hash));
    assert_eq!(
        f.stage_root_entries(),
        std::slice::from_ref(&hash),
        "no temp dir left"
    );

    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].1, NPM_CI);
    let package = read_json(&dir.join("package.json"));
    assert_eq!(package["private"], true);
    assert_eq!(
        package["dependencies"],
        json!({PI_PACKAGE: "1.3.0", "pi-btw": "0.8.0"})
    );
    assert_eq!(
        package["devDependencies"],
        json!({"esbuild": ESBUILD_VERSION})
    );
    assert_eq!(
        fs::read(dir.join("package-lock.json")).unwrap(),
        fs::read(f.path("project").join(NPM_LOCK_FILE)).unwrap()
    );
    assert_eq!(
        read_json(&dir.join("node_modules/esbuild/package.json"))["version"],
        ESBUILD_VERSION
    );

    let checkout = dir.join("git/example.com/o/r");
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), f.locked_commit);
    assert_eq!(
        fs::read_to_string(checkout.join("index.js")).unwrap(),
        "locked\n"
    );

    assert_eq!(
        fs::read_to_string(dir.join("local/extensions/init.ts")).unwrap(),
        "export default () => {};\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("local/extensions/sub/index.ts")).unwrap(),
        "export default 1;\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join(MARKER)).unwrap(),
        format!("{hash}\n")
    );

    let (outcome, calls, _) = f.stage(FakeNpm::Installs);
    assert!(matches!(outcome.unwrap(), Outcome::Cached(d) if d == dir));
    assert!(calls.is_empty(), "a cache hit runs no npm: {calls:?}");
}

#[test]
fn any_changed_input_gets_its_own_stage() {
    let f = Fixture::new("{}");
    let first = staged(f.stage(FakeNpm::Installs).0);

    write(
        &f.path("agent/extensions/sub/index.ts"),
        "export default 2;\n",
    );
    let second = staged(f.stage(FakeNpm::Installs).0);
    assert_ne!(first, second);
    assert!(first.join(MARKER).is_file(), "older stages are kept");

    write(&f.path("agent/extensions/new.ts"), "");
    let third = staged(f.stage(FakeNpm::Installs).0);

    let mut npm_lock = sample_npm_lock();
    npm_lock["packages"]["node_modules/pi-btw"]["integrity"] = json!("sha512-other");
    f.write_lock(npm_lock);
    let fourth = staged(f.stage(FakeNpm::Installs).0);
    let all = [&first, &second, &third, &fourth];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn a_failed_stage_leaves_nothing_behind() {
    let f = Fixture::new("{}");
    let (outcome, _, _) = f.stage(FakeNpm::Fails);
    assert!(outcome.is_err());
    assert!(
        f.stage_root_entries().is_empty(),
        "{:?}",
        f.stage_root_entries()
    );

    let (outcome, _, _) = f.stage(FakeNpm::InstallsWrongVersion);
    let err = format!("{:#}", outcome.err().unwrap());
    assert!(
        err.contains("node_modules/pi-btw: 0.0.1, locked 0.8.0"),
        "{err}"
    );
    assert!(
        f.stage_root_entries().is_empty(),
        "{:?}",
        f.stage_root_entries()
    );

    staged(f.stage(FakeNpm::Installs).0);
}

#[test]
fn a_stage_dir_without_its_marker_is_rebuilt() {
    let f = Fixture::new("{}");
    let hash = f.inputs().unwrap().hash().to_owned();
    write(&f.stage_root().join(&hash).join("partial"), "");
    let (outcome, _, log) = f.stage(FakeNpm::Installs);
    let dir = staged(outcome);
    assert!(!dir.join("partial").exists());
    assert!(log.iter().any(|l| l.contains("incomplete")), "{log:?}");
}

#[test]
fn git_dependencies_install_as_pi_installs_them() {
    let deps = r#"{"dependencies": {"left-pad": "1.3.0"}, "devDependencies": {"x": "1"}}"#;
    let pi_flags = [
        "--ignore-scripts",
        "--omit=dev",
        "--legacy-peer-deps",
        "--no-audit",
        "--no-fund",
    ];

    let f = Fixture::new(deps);
    let (outcome, calls, log) = f.stage(FakeNpm::Installs);
    staged(outcome);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1].0, "r");
    assert_eq!(calls[1].1[0], "install");
    assert_eq!(calls[1].1[1..], pi_flags);
    assert!(
        log.iter()
            .any(|l| l.contains("resolves its dependencies fresh")),
        "{log:?}"
    );

    let f = Fixture::new(deps);
    let remote = f.path("remote");
    git(&remote, &["checkout", "-q", &f.locked_commit]);
    commit(&remote, "package-lock.json", "{}");
    let with_lock = git(&remote, &["rev-parse", "HEAD"]);
    let f = Fixture {
        locked_commit: with_lock,
        ..f
    };
    f.write_lock(sample_npm_lock());
    let (outcome, calls, _) = f.stage(FakeNpm::Installs);
    staged(outcome);
    assert_eq!(calls[1].1[0], "ci");
    assert_eq!(calls[1].1[1..], pi_flags);

    // A package.json without dependencies needs no npm (Pi's run would only write a lockfile).
    let f = Fixture::new(r#"{"devDependencies": {"x": "1"}}"#);
    let (outcome, calls, _) = f.stage(FakeNpm::Installs);
    staged(outcome);
    assert_eq!(calls.len(), 1, "{calls:?}");
}

#[test]
fn inputs_must_be_a_consistent_current_lock() {
    let err = |f: &Fixture| format!("{:#}", f.inputs().err().unwrap());

    let f = Fixture::new("{}");
    fs::remove_file(f.path("project/pibake.lock")).unwrap();
    assert!(err(&f).contains("run `pibake lock`"), "{}", err(&f));

    let f = Fixture::new("{}");
    let npm_lock = f.path("project").join(NPM_LOCK_FILE);
    let mut bytes = fs::read(&npm_lock).unwrap();
    bytes.push(b'\n');
    fs::write(&npm_lock, bytes).unwrap();
    assert!(err(&f).contains("does not match"), "{}", err(&f));

    let f = Fixture::new("{}");
    let mut old = sample_npm_lock();
    old["packages"][""]
        .as_object_mut()
        .unwrap()
        .remove("devDependencies");
    f.write_lock(old);
    assert!(err(&f).contains("does not pin esbuild"), "{}", err(&f));

    let f = Fixture::new("{}");
    let pin = GitPin {
        name: "../escape".into(),
        url: "u".into(),
        commit: f.locked_commit.clone(),
    };
    write_lock_files(&f.path("project"), &sample_npm_lock(), vec![pin]);
    assert!(err(&f).contains("not a relative path"), "{}", err(&f));
}

#[test]
fn local_code_outside_extensions_is_refused_not_dropped() {
    let f = Fixture::new("{}");
    write(
        &f.path("agent/settings.json"),
        r#"{"extensions": ["builtin:mcp", "-builtin:codemode", "!extensions/x.ts"]}"#,
    );
    f.inputs().unwrap();

    write(
        &f.path("agent/settings.json"),
        r#"{"packages": ["./local-pkg"], "extensions": ["./more/ext.ts"]}"#,
    );
    let err = format!("{:#}", f.inputs().err().unwrap());
    assert!(err.contains("./local-pkg, ./more/ext.ts"), "{err}");
}

#[test]
fn symlinked_extension_files_are_read_through_and_dirs_refused() {
    let f = Fixture::new("{}");
    write(&f.path("elsewhere/real.ts"), "real\n");
    std::os::unix::fs::symlink(
        f.path("elsewhere/real.ts"),
        f.path("agent/extensions/link.ts"),
    )
    .unwrap();
    let dir = staged(f.stage(FakeNpm::Installs).0);
    assert_eq!(
        fs::read_to_string(dir.join("local/extensions/link.ts")).unwrap(),
        "real\n"
    );

    std::os::unix::fs::symlink(f.path("elsewhere"), f.path("agent/extensions/linked_dir")).unwrap();
    let err = format!("{:#}", f.inputs().err().unwrap());
    assert!(err.contains("symlinked directory"), "{err}");
}

/// Real `pi`, `node`, npm and git (network): locks and stages the live setup into a scratch
/// project, killing the first attempt midway. Reads `~/.pi/agent`; asserts it is unchanged.
#[test]
fn e2e_stage_the_live_setup() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if std::env::var_os("PIBAKE_E2E").is_none() {
        eprintln!("skipped: set PIBAKE_E2E=1");
        return;
    }
    let home = pibake::paths::home_dir().unwrap();
    let live = home.join(".pi/agent");
    let inv = inventory::read(&live, &home).unwrap();
    let watched: Vec<String> = inv
        .config
        .iter()
        .chain(&inv.install_roots)
        .cloned()
        .collect();
    let before = common::fingerprint(&live, &watched);

    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", live.display()),
    );
    let pibake = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pibake"));
        command.args(args).current_dir(root.path());
        command
    };
    let run = |command: &mut Command| {
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    };
    run(&mut pibake(&["lock"]));
    let stage_root = root.path().join("target/pibake/stage");
    let entries = || -> Vec<String> {
        fs::read_dir(&stage_root)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    // Killed once npm has started filling node_modules.
    let mut child = pibake(&["stage"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(300);
    while !entries()
        .iter()
        .any(|e| e.starts_with(".tmp-") && stage_root.join(e).join("node_modules").exists())
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "stage ended before it was killed"
        );
        assert!(Instant::now() < deadline, "npm never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();
    assert!(
        entries().iter().all(|e| e.starts_with(".tmp-")),
        "a killed stage left a stage dir: {:?}",
        entries()
    );

    let stdout = run(&mut pibake(&["stage"]));
    let dir = PathBuf::from(stdout.trim().strip_prefix("staged ").unwrap());
    assert!(dir.join(MARKER).is_file());
    let npm_lock = read_json(&root.path().join(NPM_LOCK_FILE));
    let lock = pibake::lock::Lock::parse(
        &fs::read_to_string(root.path().join(pibake::lock::LOCK_FILE)).unwrap(),
    )
    .unwrap();
    let root_entry = &npm_lock["packages"][""];
    let wanted: Vec<(&String, &Value)> = root_entry["dependencies"]
        .as_object()
        .unwrap()
        .iter()
        .chain(root_entry["devDependencies"].as_object().unwrap())
        .collect();
    let npm_packages = inv
        .packages
        .iter()
        .filter(|p| matches!(p.parsed, pibake::package::Source::Npm { .. }))
        .count();
    assert_eq!(
        wanted.len(),
        npm_packages + 2,
        "the npm packages, Pi and esbuild"
    );
    for (name, version) in wanted {
        let installed = read_json(&dir.join("node_modules").join(name).join("package.json"));
        assert_eq!(&installed["version"], version, "{name}");
    }
    assert_eq!(
        read_json(
            &dir.join("node_modules")
                .join(PI_PACKAGE)
                .join("package.json")
        )["version"],
        lock.pi.version
    );
    assert!(!lock.git.is_empty());
    for pin in &lock.git {
        let checkout = dir.join("git").join(&pin.name);
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), pin.commit);
    }
    for rel in &inv.local.extensions {
        assert_eq!(
            fs::read(dir.join("local").join(rel)).unwrap(),
            fs::read(live.join(rel)).unwrap(),
            "{rel}"
        );
    }

    // A cache hit with an npm that would fail if it ran.
    let shim = root.path().join("shim");
    write(&shim.join("npm"), "#!/bin/sh\nexit 97\n");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(shim.join("npm"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", shim.display(), std::env::var("PATH").unwrap());
    let start = Instant::now();
    let stdout = run(pibake(&["stage"]).env("PATH", path));
    let elapsed = start.elapsed();
    assert_eq!(
        stdout.trim(),
        format!("stage up to date: {}", dir.display())
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "cache hit took {elapsed:?}"
    );

    assert_eq!(
        common::fingerprint(&live, &watched),
        before,
        "~/.pi/agent changed"
    );
}
