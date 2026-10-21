use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const SECRET: &str = "PIBAKE-PLANTED-SECRET-7f3a9c";

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

/// A scratch home holding a copy of `agent_dir` and a `pibake.toml` pointing at it.
struct Scratch {
    home: tempfile::TempDir,
}

impl Scratch {
    fn with_agent_dir(source: Option<&Path>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let agent = home.path().join("agent");
        match source {
            Some(src) => copy_dir(src, &agent),
            None => fs::create_dir(&agent).unwrap(),
        }
        fs::write(
            home.path().join("pibake.toml"),
            "[source]\nagent_dir = \"~/agent\"\n",
        )
        .unwrap();
        Self { home }
    }

    fn agent(&self) -> PathBuf {
        self.home.path().join("agent")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pibake"))
            .args(["--config", "pibake.toml"])
            .args(args)
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .output()
            .unwrap()
    }

    fn inspect_json(&self) -> Value {
        let out = self.run(&["inspect", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn package<'a>(inventory: &'a Value, source: &str) -> &'a Value {
    inventory["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["source"] == source)
        .unwrap_or_else(|| panic!("no package {source}"))
}

#[test]
fn inventory_of_the_fixture() {
    let scratch = Scratch::with_agent_dir(Some(&fixture()));
    let inv = scratch.inspect_json();
    let agent = scratch.agent();

    assert_eq!(inv["agent_dir"], json!(agent));
    assert_eq!(inv["packages"].as_array().unwrap().len(), 5);

    let manifest_pkg = package(&inv, "npm:@scope/manifest-pkg@1.2.0");
    assert_eq!(
        manifest_pkg["parsed"],
        json!({"kind": "npm", "spec": "@scope/manifest-pkg@1.2.0", "name": "@scope/manifest-pkg", "version": "1.2.0"})
    );
    assert_eq!(manifest_pkg["identity"], "npm:@scope/manifest-pkg");
    assert_eq!(manifest_pkg["filter"], Value::Null);
    assert_eq!(
        manifest_pkg["install_path"],
        json!(agent.join("npm/node_modules/@scope/manifest-pkg"))
    );
    assert_eq!(
        manifest_pkg["installed"],
        json!({
            "version": "1.2.0",
            "manifest": "pi",
            "extensions": ["src/index.ts", "dist/index.js"],
            "skills": ["./skills"],
            "prompts": [],
            "themes": [],
            "unresolved": []
        })
    );

    let conventional = package(&inv, "npm:conventional-pkg");
    assert_eq!(
        conventional["filter"],
        json!({"extensions": ["extensions/a.ts"], "skills": []})
    );
    assert_eq!(conventional["installed"]["manifest"], "conventional");
    assert_eq!(
        conventional["installed"]["extensions"],
        json!([
            "extensions/a.ts",
            "extensions/b.js",
            "extensions/sub/index.ts"
        ])
    );
    assert_eq!(conventional["installed"]["skills"], json!(["skills"]));
    assert_eq!(conventional["installed"]["themes"], json!(["themes"]));

    let git = package(&inv, "git:github.com/owner/repo@v1");
    assert_eq!(git["identity"], "git:github.com/owner/repo");
    assert_eq!(git["parsed"]["ref"], "v1");
    assert_eq!(
        git["install_path"],
        json!(agent.join("git/github.com/owner/repo"))
    );
    assert_eq!(git["installed"]["version"], "2.0.0");
    assert_eq!(git["installed"]["extensions"], json!([]));
    assert_eq!(
        git["installed"]["unresolved"],
        json!([{"entry": "./ext/*.ts", "reason": "glob"}, {"entry": "!ext/skip.ts", "reason": "pattern"}])
    );

    let local = package(&inv, "./local-pkg");
    assert_eq!(
        local["identity"],
        format!("local:{}", agent.join("local-pkg").display())
    );
    assert_eq!(local["installed"]["version"], Value::Null);
    assert_eq!(local["installed"]["extensions"], json!(["main.js"]));
    assert_eq!(local["installed"]["prompts"], json!(["./prompts"]));

    assert_eq!(package(&inv, "npm:not-installed")["installed"], Value::Null);

    assert_eq!(
        inv["settings_resources"],
        json!({"extensions": ["-builtin:mcp"]})
    );
    assert_eq!(
        inv["local"],
        json!({
            "extensions": ["extensions/dir-ext/index.ts", "extensions/dir-pkg/entry.ts", "extensions/one.ts", "extensions/two.js"],
            "skills": ["local-skill"],
            "prompts": ["review.md"],
            "themes": ["theme.json"],
            "agents": ["Explore.md"]
        })
    );
    assert_eq!(
        inv["config"],
        json!([
            "AGENTS.md",
            "CLAUDE.md",
            "SYSTEM.md",
            "agents",
            "extensions",
            "local-pkg",
            "mcp.json",
            "models.json",
            "prompts",
            "settings.json",
            "skills",
            "themes"
        ])
    );
    assert_eq!(inv["install_roots"], json!(["git", "npm"]));
    assert_eq!(
        inv["state"],
        json!([
            "auth.json",
            "bin",
            "crashes.json",
            "models-store.json",
            "sessions"
        ])
    );
}

#[test]
fn state_is_never_read_and_secrets_never_printed() {
    let scratch = Scratch::with_agent_dir(Some(&fixture()));
    let agent = scratch.agent();
    // Unreadable state proves pibake never opens it: any read would fail the run.
    let lock = |p: &Path, mode| fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    lock(&agent.join("auth.json"), 0o000);
    lock(&agent.join("sessions"), 0o000);

    for args in [&["inspect", "--json"][..], &["inspect"][..]] {
        let out = scratch.run(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {stderr}");
        assert!(
            stdout.contains("auth.json"),
            "{args:?}: auth.json listed by name"
        );
        assert!(
            !stdout.contains(SECRET) && !stderr.contains(SECRET),
            "{args:?} leaked the secret"
        );
    }

    lock(&agent.join("sessions"), 0o755);
    lock(&agent.join("auth.json"), 0o600);
}

#[test]
fn missing_settings_means_no_packages() {
    let scratch = Scratch::with_agent_dir(None);
    fs::write(scratch.agent().join("AGENTS.md"), "x").unwrap();
    let inv = scratch.inspect_json();
    assert_eq!(inv["packages"], json!([]));
    assert_eq!(inv["config"], json!(["AGENTS.md"]));
    assert_eq!(inv["state"], json!([]));
}

#[test]
fn null_packages_mean_none_as_in_pi() {
    let scratch = Scratch::with_agent_dir(None);
    fs::write(
        scratch.agent().join("settings.json"),
        "{\"packages\": null}",
    )
    .unwrap();
    assert_eq!(scratch.inspect_json()["packages"], json!([]));
}

#[test]
fn the_first_entry_of_a_package_wins_as_in_pi() {
    let scratch = Scratch::with_agent_dir(None);
    fs::write(
        scratch.agent().join("settings.json"),
        r#"{"packages": ["npm:x@1", "git:github.com/o/r", "npm:x@2", "https://github.com/o/r.git"]}"#,
    )
    .unwrap();
    let sources: Vec<Value> = scratch.inspect_json()["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["source"].clone())
        .collect();
    assert_eq!(sources, [json!("npm:x@1"), json!("git:github.com/o/r")]);
}

#[test]
fn settings_with_a_bom_parse() {
    let scratch = Scratch::with_agent_dir(None);
    fs::write(
        scratch.agent().join("settings.json"),
        "\u{feff}{\"packages\": [\"npm:x\"]}",
    )
    .unwrap();
    assert_eq!(scratch.inspect_json()["packages"][0]["identity"], "npm:x");
}

#[test]
fn broken_inputs_fail_loudly() {
    let cases = [
        ("{not json", "parsing"),
        ("[]", "not a JSON object"),
        ("{\"packages\": {}}", "not an array"),
        (
            "{\"packages\": [{\"source\": \"npm:x\", \"extensions\": \"a\"}]}",
            "packages[0]",
        ),
        ("{\"packages\": [\"git:owner/repo\"]}", "hosted-git-info"),
    ];
    for (settings, expected) in cases {
        let scratch = Scratch::with_agent_dir(None);
        fs::write(scratch.agent().join("settings.json"), settings).unwrap();
        let out = scratch.run(&["inspect", "--json"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{settings}");
        assert!(stderr.contains(expected), "{settings}: {stderr}");
    }
}

#[test]
fn missing_agent_dir_and_bad_config_fail() {
    let scratch = Scratch::with_agent_dir(None);
    fs::remove_dir(scratch.agent()).unwrap();
    let out = scratch.run(&["inspect"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not exist"));

    fs::write(
        scratch.home.path().join("pibake.toml"),
        "[source]\nagentdir = \"x\"\n",
    )
    .unwrap();
    let out = scratch.run(&["inspect"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("agentdir"));
}

/// Oracle: Pi's own resolver (`DefaultPackageManager.resolve`) on the same copy finds the same
/// extension entry points, for every package pibake resolves fully. Needs `node` and Pi.
#[test]
fn extension_entries_match_pis_resolver() {
    if std::env::var_os("PIBAKE_E2E").is_none() {
        eprintln!("skipped: set PIBAKE_E2E=1");
        return;
    }
    let pi = pi_package_dir();
    let scratch = Scratch::with_agent_dir(Some(&fixture()));
    let agent = scratch.agent();
    let cwd = scratch.home.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    let script = scratch.home.path().join("resolve.mjs");
    fs::write(
        &script,
        format!(
            r#"import {{ DefaultPackageManager }} from "{core}/package-manager.js";
import {{ SettingsManager }} from "{core}/settings-manager.js";
const [agentDir, cwd] = process.argv.slice(2);
const settingsManager = SettingsManager.create(cwd, agentDir);
const pm = new DefaultPackageManager({{ cwd, agentDir, settingsManager }});
const resolved = await pm.resolve(async () => "skip");
console.log(JSON.stringify(resolved.extensions.map((e) => e.path)));
"#,
            core = pi.join("dist/core").display()
        ),
    )
    .unwrap();
    let out = Command::new("node")
        .arg(&script)
        .arg(&agent)
        .arg(&cwd)
        .env("HOME", scratch.home.path())
        .env("PI_OFFLINE", "1")
        .output()
        .expect("node runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut from_pi: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();

    let inv = scratch.inspect_json();
    let mut from_pibake = vec![];
    for p in inv["packages"].as_array().unwrap() {
        let Some(installed) = p["installed"].as_object() else {
            continue;
        };
        let root = PathBuf::from(p["install_path"].as_str().unwrap());
        let entries = installed["extensions"].as_array().unwrap();
        if installed["unresolved"].as_array().unwrap().is_empty() {
            from_pibake.extend(entries.iter().map(|e| root.join(e.as_str().unwrap())));
        } else {
            // Partly resolved: pibake's entries must be a subset; drop the package from the comparison.
            for e in entries {
                assert!(from_pi.contains(&root.join(e.as_str().unwrap()).display().to_string()));
            }
            from_pi.retain(|f| !Path::new(f).starts_with(&root));
        }
    }
    let local = inv["local"]["extensions"].as_array().unwrap();
    from_pibake.extend(local.iter().map(|e| agent.join(e.as_str().unwrap())));
    let mut from_pibake: Vec<String> = from_pibake
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    from_pi.sort();
    from_pibake.sort();
    assert_eq!(from_pibake, from_pi);
}

/// `PIBAKE_PI_PACKAGE`, else the package behind `pi` on `PATH`.
fn pi_package_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PIBAKE_PI_PACKAGE") {
        return dir.into();
    }
    let out = Command::new("sh")
        .args(["-c", "command -v pi"])
        .output()
        .unwrap();
    let bin = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    let mut dir = fs::canonicalize(&bin).unwrap_or_else(|_| panic!("pi not found on PATH"));
    while !dir.join("package.json").is_file() {
        dir = dir.parent().expect("pi package root").to_path_buf();
    }
    dir
}
