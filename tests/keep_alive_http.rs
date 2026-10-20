//! `keep_alive_http`: a prompt after 5 s of idle reuses the provider connection, where stock Pi
//! opens a new one. Needs real `pi`, `node` and npm (network); behind PIBAKE_E2E.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{bundled_launcher, e2e_enabled, pibake, write};
use pibake::fake_provider::{self, FakeProvider};
use serde_json::{Value, json};

/// Two RPC prompts `gap` apart; returns the connections the provider accepted for them.
fn connections_for_two_prompts(pi: &str, home: &Path, agent: &Path, gap: Duration) -> usize {
    let fake = FakeProvider::keep_alive().unwrap();
    let models =
        json!({ "providers": { "fake": fake_provider::provider_entry(&fake.base_url()) } });
    write(&agent.join("models.json"), &models.to_string());
    let mut words = pi.split(' ');
    let mut child = Command::new(words.next().unwrap())
        .args(words)
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--provider",
            "fake",
            "--model",
        ])
        .arg(fake_provider::Script::Hello.model_id())
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    for turn in 0..2 {
        if turn > 0 {
            std::thread::sleep(gap);
        }
        let prompt = json!({ "id": format!("turn_{turn}"), "type": "prompt", "message": "hi" });
        writeln!(stdin, "{prompt}").unwrap();
        loop {
            let line = lines.next().expect("Pi exited mid-session").unwrap();
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["type"] == "response" && event["success"] == false {
                panic!("{pi}: {event}");
            }
            if event["type"] == "agent_settled" {
                break;
            }
        }
    }
    drop(stdin);
    assert!(child.wait().unwrap().success(), "{pi}");
    assert_eq!(fake.request_count(), 2, "{pi}");
    fake.connections()
}

#[test]
fn e2e_a_prompt_after_idle_reuses_the_connection() {
    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (agent, home) = (root.path().join("agent"), root.path().join("home"));
    std::fs::create_dir_all(&home).unwrap();
    write(
        &agent.join("settings.json"),
        "{\"retry\": {\"enabled\": false}}",
    );
    let project = root.path().join("project");
    write(
        &project.join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", agent.display()),
    );
    pibake(&project, &["lock"], true);
    let stdout = pibake(&project, &["bundle"], true);
    assert!(
        stdout.contains("pass keep_alive_http: node_modules/@earendil-works/pi-coding-agent/dist/core/http-dispatcher.js: keeps idle HTTP connections for 120 s"),
        "{stdout}"
    );
    let bundle = format!("node {}", bundled_launcher(&stdout).display());
    // undici's default keeps an idle socket for 4 s.
    let gap = Duration::from_secs(5);
    assert_eq!(connections_for_two_prompts(&bundle, &home, &agent, gap), 1);
    assert_eq!(connections_for_two_prompts("pi", &home, &agent, gap), 2);
}
