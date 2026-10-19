//! `lazy_modules`: a bundle with its stand-ins against one without, and against stock Pi on a
//! markdown reply in interactive mode. Needs real `pi`, `node` and npm (network): behind
//! PIBAKE_E2E.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{bundled_launcher, e2e_enabled, pibake, write};
use pibake::bench::Screen;
use pibake::fake_provider;
use pibake::passes::lazy_modules::FILES;
use pibake::process::{Supervised, openpty};

const REPLY: &str =
    "# Lazy heading\n\nSome **bold words** and `inline code`.\n\n- first item\n- last item\n";

/// An OpenAI-compatible provider that streams `REPLY` to every request.
fn markdown_provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    serde_json::json!({ "id": "md", "object": "chat.completion.chunk", "created": 0, "model": "hello",
                        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
                };
                let events = [
                    chunk(
                        serde_json::json!({ "role": "assistant", "content": REPLY }),
                        serde_json::Value::Null,
                    ),
                    chunk(serde_json::json!({}), "stop".into()),
                ];
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for e in events {
                    out.push_str(&format!("data: {e}\n\n"));
                }
                out.push_str("data: [DONE]\n\n");
                let mut stream = stream;
                let _ = stream.write_all(out.as_bytes());
            });
        }
    });
    port
}

/// Interactive Pi in a pty, sent one message on its command line; returns everything it wrote
/// until the reply's last line was drawn, and the text its frames drew.
fn interactive_reply(pi: &str, home: &Path, agent: &Path) -> (Vec<u8>, String) {
    let (master, tty) = openpty(100, 40).unwrap();
    let mut words = pi.split(' ');
    let mut cmd = Command::new(words.next().unwrap());
    cmd.args(words)
        .args(["--no-session", "--provider", "md", "--model", "hello"])
        .arg("Show some markdown")
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .env("TERM", "xterm-256color")
        .stderr(fs::File::create(home.join("stderr.log")).unwrap());
    let run = Supervised::spawn_in_pty(cmd, tty, Duration::from_secs(60)).unwrap();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let sink = raw.clone();
    let mut reader = master;
    std::thread::spawn(move || {
        let mut buf = [0; 64 * 1024];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let text = |raw: &[u8]| {
        let mut screen = Screen::default();
        screen.feed(raw, Duration::ZERO);
        String::from_utf8_lossy(&screen.text).into_owned()
    };
    while !text(&raw.lock().unwrap()).contains("last item") {
        assert!(
            Instant::now() < deadline,
            "{pi}: no reply drawn; stderr: {}",
            fs::read_to_string(home.join("stderr.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(run);
    let raw = raw.lock().unwrap().clone();
    let drawn = text(&raw);
    (raw, drawn)
}

/// The bytes that draw `word`: from the escape sequence before it to the word's end.
fn styled<'a>(raw: &'a [u8], word: &str) -> &'a [u8] {
    let at = raw
        .windows(word.len())
        .position(|w| w == word.as_bytes())
        .unwrap_or_else(|| panic!("{word} not drawn"));
    let start = raw[..at].iter().rposition(|&b| b == 0x1b).unwrap();
    &raw[start..at + word.len()]
}

#[test]
fn e2e_lazy_modules_bundle_and_render_as_stock() {
    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (agent, home) = (root.path().join("agent"), root.path().join("home"));
    fs::create_dir_all(&home).unwrap();
    let port = markdown_provider();
    let models = serde_json::json!({ "providers": {
        "md": fake_provider::provider_entry(&format!("http://127.0.0.1:{port}/v1")),
    }});
    write(&agent.join("models.json"), &models.to_string());
    write(&agent.join("settings.json"), "{}");
    let project = root.path().join("project");
    let source = format!("[source]\nagent_dir = \"{}\"\n", agent.display());
    write(&project.join("pibake.toml"), &source);
    write(
        &project.join("off.toml"),
        &format!(
            "{source}[build]\npasses = [\"trim_highlight\", \"trim_apis\", \"incremental_tool_json\", \"lazy_http_dispatcher\", \"ascii_bundle\"]\n"
        ),
    );
    pibake(&project, &["lock"], true);
    let stdout = pibake(&project, &["bundle"], true);
    let on = bundled_launcher(&stdout);
    let off = bundled_launcher(&pibake(&project, &["--config", "off.toml", "bundle"], true));
    let main = |launcher: &Path| fs::read_to_string(launcher.with_file_name("main.mjs")).unwrap();
    let (on_main, off_main) = (main(&on), main(&off));
    for (package, path) in FILES {
        let file = format!("node_modules/{package}/{path}");
        assert!(
            stdout.contains(&format!(
                "pass lazy_modules: {file}: evaluated on first use\n"
            )),
            "{stdout}"
        );
        let stand_in = format!("\n// pibake-lazy:{file}\n");
        assert!(on_main.contains(&stand_in), "{file}");
        assert!(!off_main.contains(&stand_in), "{file}");
    }
    // A module outside the list, importing none of it, is bundled as before.
    let section = |text: &str, file: &str| -> String {
        let start = text.find(&format!("\n// {file}\n")).unwrap();
        let end = text[start + 1..].find("\n// ").unwrap() + start + 1;
        text[start..end].to_owned()
    };
    let keys = "node_modules/@earendil-works/pi-tui/dist/keys.js";
    assert_eq!(section(&on_main, keys), section(&off_main, keys));

    let ours = format!("node {}", on.display());
    let (stock_raw, stock_text) = interactive_reply("pi", &home, &agent);
    let (our_raw, our_text) = interactive_reply(&ours, &home, &agent);
    for text in [&stock_text, &our_text] {
        for drawn in ["Lazy heading", "bold words", "inline code", "first item"] {
            assert!(text.contains(drawn), "{drawn} not in {text}");
        }
        assert!(!text.contains("**bold words**"), "{text}");
    }
    for word in ["Lazy heading", "bold words", "inline code"] {
        let stock = styled(&stock_raw, word);
        assert!(
            our_raw.windows(stock.len()).any(|w| w == stock),
            "{word}: stock draws {:?}",
            String::from_utf8_lossy(stock)
        );
    }
}
