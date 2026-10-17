mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{bundled_launcher, e2e_enabled, pibake, pibake_with, write};

/// The executable with nothing from pibake's environment, `node` included.
fn run_bare(bin: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/nonexistent")
        .output()
        .unwrap()
}

/// A setup with an extension that reads its location, one with an `import.meta.main` self-check,
/// and one esbuild cannot bundle (it loads through jiti); built as `pi-test`, keeping only the fake
/// provider's API. `build` adds to `[build]`.
fn fixture_setup(root: &Path, build: &str) -> (PathBuf, PathBuf) {
    let agent = root.join("agent");
    write(&agent.join("settings.json"), "{}");
    write(
        &agent.join("extensions/located.js"),
        "import { fileURLToPath } from \"node:url\";\nexport default function (pi) {\n\tpi.registerCommand(\"located\", { description: fileURLToPath(import.meta.url), handler: async () => {} });\n}\n",
    );
    // In the executable, main.mjs is the main module; no bundled module may think it is.
    write(
        &agent.join("extensions/self_check.ts"),
        "if ((import.meta as any).main) console.error(\"self-check ran\");\nexport default function () {}\n",
    );
    // esbuild cannot resolve its import, so it loads through jiti inside the executable.
    write(
        &agent.join("extensions/broken.ts"),
        "export default function (pi: any) {\n\tpi.registerCommand(\"from-broken\", { description: \"via jiti\", handler: async () => { await import(\"pibake-no-such-module\"); } });\n}\n",
    );
    let project = root.join("project");
    write(
        &project.join("pibake.toml"),
        &format!(
            "[source]\nagent_dir = \"{}\"\n\n[build]\nname = \"pi-test\"\n{}\n[trim]\napis = [\"openai-completions\"]\n",
            agent.display(),
            build
        ),
    );
    (agent, project)
}

/// Real `pi`, `node` and npm (network): a fixture setup is built into an executable, which runs
/// without `node`, matches stock Pi and its node bundle, extracts its files once under
/// concurrent launches, rebuilds byte for byte, and installs. It keeps only the fake provider's API,
/// so asking for another names it in one line each at startup and on the request, without a trace.
#[test]
fn e2e_build_a_fixture_setup() {
    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (agent, project) = fixture_setup(root.path(), "");
    let stdout = pibake(&project, &["build"], true);
    let bin = project.join("target/pibake/bin/pi-test");
    assert!(
        stdout.contains(&format!("built {}", bin.display())),
        "{stdout}"
    );
    let launcher = bundled_launcher(&stdout);
    assert!(stdout.contains(" 1 failed\n"), "{stdout}");

    // Outside the prologue, the executable's code does not point into the stage.
    let stage_dir = fs::read_dir(project.join("target/pibake/stage"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let main = fs::read_to_string(launcher.with_file_name("main.mjs")).unwrap();
    assert_eq!(
        main.matches(stage_dir.to_str().unwrap()).count(),
        1,
        "{}",
        stage_dir.display()
    );

    let home = root.path().join("home");
    let version = run_bare(&bin, &home, &["--version"]);
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(version.contains(" (pibake "), "{version}");

    let google = Command::new(&bin)
        .args([
            "--provider",
            "google",
            "--model",
            "gemini-2.5-pro",
            "-p",
            "hi",
            "--no-session",
        ])
        .current_dir(root.path())
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/nonexistent")
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("PI_OFFLINE", "1")
        .env("GEMINI_API_KEY", "unused")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!google.status.success());
    assert_eq!(
        String::from_utf8_lossy(&google.stderr),
        "pi-test: this build left out the google-generative-ai API (--provider google); add it to [trim] apis in pibake.toml, then run pibake install\n\
         the google-generative-ai API was left out of this pibake build; add it to [trim] apis in pibake.toml\n"
    );

    // Launched together on a fresh cache, every run works and one extraction is kept.
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(&bin)
                .arg("--help")
                .env_clear()
                .env("HOME", &home)
                .env("PATH", "/nonexistent")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let cache: Vec<_> = fs::read_dir(home.join(".cache/pibake"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(cache.len(), 1, "{cache:?}");
    assert!(cache[0].join(".pibake-extracted").is_file());

    // With the stage out of the way, the executable runs from what it carries.
    let hidden = stage_dir.with_extension("hidden");
    fs::rename(&stage_dir, &hidden).unwrap();
    let verify = pibake(
        &project,
        &["verify", "--pi", bin.to_str().unwrap(), "--against", "pi"],
        true,
    );
    fs::rename(&hidden, &stage_dir).unwrap();
    assert!(verify.starts_with("same: "), "{verify}");
    // No `node` on the PATH; the reference is the node bundle, by absolute paths.
    let node = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("node"))
        .find(|p| p.is_file())
        .unwrap();
    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let against = format!("{} {}", node.display(), launcher.display());
    let verify = pibake_with(
        &project,
        &[
            "verify",
            "--pi",
            bin.to_str().unwrap(),
            "--against",
            &against,
        ],
        &[("PATH", empty.as_os_str())],
        true,
    );
    assert!(verify.starts_with("same: "), "{verify}");

    let first = fs::read(&bin).unwrap();
    pibake(&project, &["build"], true);
    assert!(
        fs::read(&bin).unwrap() == first,
        "rebuild changed the bytes"
    );

    let install_home = root.path().join("install-home");
    let installed = pibake_with(
        &project,
        &["install"],
        &[("HOME", install_home.as_os_str())],
        true,
    );
    let target = install_home.join(".local/bin/pi-test");
    assert!(
        installed.contains(&format!("installed {}", target.display())),
        "{installed}"
    );
    assert!(fs::read(&target).unwrap() == first);

    // An edited extension: one line on stderr and Pi still runs. The edit changes nothing Pi
    // reports, so `verify` still agrees: the line never reaches the model.
    let stderr = |envs: &[(&str, &str)]| {
        let out = Command::new(&bin)
            .arg("--help")
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/nonexistent")
            .env("PI_CODING_AGENT_DIR", &agent)
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("Usage"));
        String::from_utf8(out.stderr).unwrap()
    };
    assert_eq!(stderr(&[]), "");
    let located = agent.join("extensions/located.js");
    let source = fs::read_to_string(&located).unwrap();
    fs::write(&located, format!("{source}// edited\n")).unwrap();
    assert_eq!(
        stderr(&[]),
        "pi-test: built from an older setup (extensions/located.js edited); run pibake install\n"
    );
    assert_eq!(stderr(&[("PIBAKE_NO_STALENESS_CHECK", "1")]), "");
    let verify = pibake(
        &project,
        &["verify", "--pi", bin.to_str().unwrap(), "--against", "pi"],
        true,
    );
    assert!(verify.starts_with("same: "), "{verify}");
    fs::write(&located, source).unwrap();
    assert_eq!(stderr(&[]), "");
}

/// What `bin --help` writes to stdout with only `envs` (and `HOME`, `PATH`), in a pseudo-terminal
/// when `pty` (util-linux `script`, which merges stderr in).
fn help(
    bin: &str,
    path: &std::ffi::OsStr,
    home: &Path,
    envs: &[(&str, &str)],
    pty: bool,
) -> Vec<u8> {
    let mut cmd = if pty {
        let script = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|d| d.join("script"))
            .find(|p| p.is_file())
            .expect("util-linux script on PATH");
        let mut c = Command::new(script);
        c.args(["-qec", &format!("{bin} --help"), "/dev/null"]);
        c
    } else {
        let mut c = Command::new(bin);
        c.arg("--help");
        c
    };
    let out = cmd
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{bin}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// The fixture setup as a `sea_snapshot` executable: built from files extracted at build time,
/// it runs without `node` from any home and working directory, matches stock Pi, and reads what
/// a launch decides at launch (colour from the terminal and environment, `PI_TIMING`), not as
/// the build saw it.
#[test]
fn e2e_build_a_fixture_setup_as_a_snapshot() {
    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (agent, project) = fixture_setup(root.path(), "backend = \"sea_snapshot\"\n");
    let cache = root.path().join("cache");
    let env = [("XDG_CACHE_HOME", cache.as_os_str())];
    let stdout = pibake_with(&project, &["build"], &env, true);
    let bin = project.join("target/pibake/bin/pi-test");
    assert!(
        stdout.contains(&format!("built {}", bin.display())),
        "{stdout}"
    );
    assert!(bundled_launcher(&stdout).ends_with("main.cjs"), "{stdout}");
    assert!(stdout.contains(" 1 failed\n"), "{stdout}");
    let extracted: Vec<_> = fs::read_dir(cache.join("pibake"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(extracted.len(), 1, "{extracted:?}");
    let files = &extracted[0];
    assert!(files.join(".pibake-extracted").is_file());

    let home = root.path().join("home");
    let version = run_bare(&bin, &home, &["--version"]);
    assert!(String::from_utf8_lossy(&version.stdout).contains(" (pibake "));
    // The executable keeps using the files it was built with; a launch extracts nothing.
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(&bin)
                .arg("--help")
                .env_clear()
                .env("HOME", &home)
                .env("PATH", "/nonexistent")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!home.join(".cache").exists());
    // Removed, they come back from the executable.
    fs::remove_dir_all(files).unwrap();
    assert!(run_bare(&bin, &home, &["--help"]).status.success());
    assert!(files.join(".pibake-extracted").is_file());

    // In the harness's home, agent directory and working directory, none of them the build's.
    let stage_dir = fs::read_dir(project.join("target/pibake/stage"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let hidden = stage_dir.with_extension("hidden");
    fs::rename(&stage_dir, &hidden).unwrap();
    let verify = pibake_with(
        &project,
        &["verify", "--pi", bin.to_str().unwrap(), "--against", "pi"],
        &env,
        true,
    );
    fs::rename(&hidden, &stage_dir).unwrap();
    assert!(verify.starts_with("same: "), "{verify}");

    // Colour as stock Pi decides it at launch: from the environment and the terminal.
    let path = std::env::var_os("PATH").unwrap();
    let agent_dir = agent.to_str().unwrap();
    for (envs, pty) in [
        (&[][..], false),
        (&[("FORCE_COLOR", "0")][..], false),
        (&[("FORCE_COLOR", "3")][..], false),
        (&[("TERM", "dumb")][..], true),
        (&[("TERM", "xterm-256color")][..], true),
        (&[("TERM", "xterm-256color"), ("NO_COLOR", "1")][..], true),
    ] {
        let envs: Vec<(&str, &str)> = envs
            .iter()
            .copied()
            .chain([("PI_CODING_AGENT_DIR", agent_dir)])
            .collect();
        let ours = help(
            bin.to_str().unwrap(),
            "/nonexistent".as_ref(),
            &home,
            &envs,
            pty,
        );
        let stock = help("pi", &path, &home, &envs, pty);
        assert!(
            ours == stock,
            "{envs:?} pty={pty}:\n{}\n---\n{}",
            String::from_utf8_lossy(&ours),
            String::from_utf8_lossy(&stock)
        );
    }
    assert!(
        help(
            bin.to_str().unwrap(),
            "/nonexistent".as_ref(),
            &home,
            &[
                ("TERM", "xterm-256color"),
                ("PI_CODING_AGENT_DIR", agent_dir)
            ],
            true
        )
        .windows(4)
        .any(|w| w == b"\x1b[1m"),
        "no bold in a terminal"
    );

    let timings = |envs: &[(&str, &str)]| {
        let out = Command::new(&bin)
            .args(["--mode", "rpc", "--no-session"])
            .current_dir(root.path())
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/nonexistent")
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("PI_OFFLINE", "1")
            .envs(envs.iter().copied())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).contains("Startup Timings")
    };
    assert!(timings(&[("PI_TIMING", "1")]));
    assert!(!timings(&[]));

    let located = agent.join("extensions/located.js");
    let source = fs::read_to_string(&located).unwrap();
    fs::write(&located, format!("{source}// edited\n")).unwrap();
    let stale = Command::new(&bin)
        .arg("--help")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/nonexistent")
        .env("PI_CODING_AGENT_DIR", &agent)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&stale.stderr),
        "pi-test: built from an older setup (extensions/located.js edited); run pibake install\n"
    );
}
