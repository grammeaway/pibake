mod common;

use std::fs;
use std::path::Path;
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
    let agent = root.path().join("agent");
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
    let project = root.path().join("project");
    write(
        &project.join("pibake.toml"),
        &format!(
            "[source]\nagent_dir = \"{}\"\n\n[build]\nname = \"pi-test\"\n\n[trim]\napis = [\"openai-completions\"]\n",
            agent.display()
        ),
    );
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
