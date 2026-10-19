//! `lazy_http_dispatcher`: `pibake:http-dispatcher` on its own, and a bundle against stock Pi
//! on a request through a proxy and one cut off by the idle timeout. Needs `node`; the bundle
//! tests also real `pi` and npm (network). All behind PIBAKE_E2E.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{bundled_launcher, e2e_enabled, pibake, write};
use pibake::fake_provider::{self, FakeProvider};

/// Each case imports a fresh instance of the module, with a `configure` that records its calls.
const CASES: &str = r#"
import assert from "node:assert/strict";

let instance = 0;
const fresh = () => import(`./http_dispatcher.mjs?${instance++}`);
const recorder = (calls) => (timeoutMs) => {
	calls.push({ timeoutMs, proxy: process.env.HTTP_PROXY });
	globalThis.Headers = "undici Headers";
};
const globals = ["Headers", "PibakeMissing"];
const nodeHeaders = globalThis.Headers;
const reset = () => {
	globalThis.Headers = nodeHeaders;
	delete globalThis.PibakeMissing;
	delete process.env.HTTP_PROXY;
};

// Nothing happens until a global is read; then the last call's timeout and proxy are used.
{
	const m = await fresh();
	const calls = [];
	process.env.HTTP_PROXY = "http://first";
	m.deferHttpDispatcher(recorder(calls), 1, globals);
	process.env.HTTP_PROXY = "http://second";
	m.deferHttpDispatcher(recorder(calls), 2, globals);
	process.env.HTTP_PROXY = "http://later";
	assert.deepEqual(calls, []);
	assert.equal(Object.keys(globalThis).includes("Headers"), false);
	assert.equal(globalThis.Headers, "undici Headers");
	assert.deepEqual(calls, [{ timeoutMs: 2, proxy: "http://second" }]);
	assert.equal(process.env.HTTP_PROXY, "http://later");
	assert.equal("PibakeMissing" in globalThis, false);
	// After the first use, a call runs at once.
	m.deferHttpDispatcher(recorder(calls), 3, globals);
	assert.deepEqual(calls.at(-1), { timeoutMs: 3, proxy: "http://later" });
	reset();
}

// A write materialises first, then lands; so does the wrapper's undici load.
{
	const m = await fresh();
	const calls = [];
	m.deferHttpDispatcher(recorder(calls), 1, globals);
	globalThis.PibakeMissing = "mine";
	assert.equal(calls.length, 1);
	assert.equal(globalThis.PibakeMissing, "mine");
	reset();

	const w = await fresh();
	w.materializeHttpDispatcher();
	w.deferHttpDispatcher(recorder(calls), 4, globals);
	assert.equal(calls.length, 1);
	w.materializeHttpDispatcher();
	assert.deepEqual(calls.at(-1), { timeoutMs: 4, proxy: undefined });
	w.materializeHttpDispatcher();
	assert.equal(calls.length, 2);
	reset();
}

// A global redefined before the first use keeps its definition.
{
	const m = await fresh();
	const calls = [];
	m.deferHttpDispatcher(() => calls.push("configured"), 1, globals);
	Object.defineProperty(globalThis, "PibakeMissing", { configurable: true, value: "defined" });
	assert.equal(globalThis.Headers, nodeHeaders);
	assert.deepEqual(calls, ["configured"]);
	assert.equal(globalThis.PibakeMissing, "defined");
	reset();
}

// A global that cannot be replaced: stock's work runs at once.
{
	const m = await fresh();
	const calls = [];
	Object.defineProperty(globalThis, "PibakeFixed", { configurable: false, value: 1 });
	m.deferHttpDispatcher(recorder(calls), 5, [...globals, "PibakeFixed"]);
	assert.deepEqual(calls, [{ timeoutMs: 5, proxy: undefined }]);
	assert.equal(Object.getOwnPropertyDescriptor(globalThis, "Headers").get, undefined);
	reset();
}
console.log("ok");
"#;

#[test]
fn e2e_defers_until_first_use_with_the_last_call() {
    if !e2e_enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("http_dispatcher.mjs"),
        pibake::bundle::HTTP_DISPATCHER,
    );
    write(&dir.path().join("cases.mjs"), CASES);
    let out = Command::new("node")
        .arg(dir.path().join("cases.mjs"))
        .output()
        .unwrap();
    assert!(
        out.status.success() && out.stdout == b"ok\n",
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An HTTP proxy that tunnels `CONNECT` and records each target.
fn connect_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let seen = targets.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(client.try_clone().unwrap());
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    if reader.read_until(b'\n', &mut head).unwrap_or(0) == 0 {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut client = client;
                let Some(target) = head
                    .strip_prefix("CONNECT ")
                    .and_then(|l| l.split(' ').next())
                else {
                    let _ = client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                };
                seen.lock().unwrap().push(target.to_owned());
                let upstream = TcpStream::connect(target).unwrap();
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .unwrap();
                let (mut up_read, mut up_write) = (upstream.try_clone().unwrap(), upstream);
                let mut down_write = client;
                let buffered = reader.buffer().to_vec();
                up_write.write_all(&buffered).unwrap();
                let mut reader = reader.into_inner();
                let up = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut reader, &mut up_write);
                    let _ = up_write.shutdown(Shutdown::Write);
                });
                let _ = std::io::copy(&mut up_read, &mut down_write);
                let _ = down_write.shutdown(Shutdown::Write);
                let _ = up.join();
            });
        }
    });
    (port, targets)
}

/// A provider that answers with event-stream headers, then sends nothing until the client
/// closes.
fn stalling_provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
            });
        }
    });
    port
}

/// One print turn in a cleared environment: offline, `home` as home, `agent` as agent dir.
fn print_turn(pi: &str, home: &Path, agent: &Path, provider: &str) -> (Output, Duration) {
    let mut words = pi.split(' ');
    let start = Instant::now();
    let out = Command::new(words.next().unwrap())
        .args(words)
        .args(["-p", "--no-session", "--provider", provider, "--model"])
        .args([fake_provider::Script::Hello.model_id(), "hi"])
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .output()
        .unwrap();
    (out, start.elapsed())
}

/// Real `pi`, `node` and npm (network): with the pass on (the default), a print turn goes
/// through the proxy in `httpProxy` and fails on `httpIdleTimeoutMs` as in stock Pi.
#[test]
fn e2e_proxy_and_idle_timeout_behave_as_stock() {
    if !e2e_enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (agent, home) = (root.path().join("agent"), root.path().join("home"));
    std::fs::create_dir_all(&home).unwrap();
    let fake = FakeProvider::start().unwrap();
    let (proxy, targets) = connect_proxy();
    let stall = stalling_provider();
    let models = serde_json::json!({ "providers": {
        "fake": fake_provider::provider_entry(&fake.base_url()),
        "stall": fake_provider::provider_entry(&format!("http://127.0.0.1:{stall}/v1")),
    }});
    write(&agent.join("models.json"), &models.to_string());
    let project = root.path().join("project");
    write(
        &project.join("pibake.toml"),
        &format!("[source]\nagent_dir = \"{}\"\n", agent.display()),
    );
    let settings = |extra: &str| {
        write(
            &agent.join("settings.json"),
            &format!("{{\"retry\": {{\"enabled\": false}}{extra}}}"),
        )
    };
    settings("");
    pibake(&project, &["lock"], true);
    let stdout = pibake(&project, &["bundle"], true);
    assert!(
        stdout.contains("pass lazy_http_dispatcher: node_modules/@earendil-works/pi-coding-agent/dist/core/http-dispatcher.js: makes the HTTP dispatcher on first use of fetch, "),
        "{stdout}"
    );
    let bundle = format!("node {}", bundled_launcher(&stdout).display());
    let fake_target = format!("127.0.0.1:{}", fake.port());

    settings(&format!(", \"httpProxy\": \"http://127.0.0.1:{proxy}\""));
    let mut tunnels = vec![];
    for pi in [bundle.as_str(), "pi"] {
        let (out, _) = print_turn(pi, &home, &agent, "fake");
        assert!(
            out.status.success(),
            "{pi}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "Hello from pibake_fake.",
            "{pi}"
        );
        tunnels.push(std::mem::take(&mut *targets.lock().unwrap()));
    }
    assert_eq!(tunnels[0], tunnels[1]);
    assert!(
        !tunnels[0].is_empty() && tunnels[0].iter().all(|t| *t == fake_target),
        "{tunnels:?}"
    );

    settings(", \"httpIdleTimeoutMs\": 1000");
    let results: Vec<(Output, Duration)> = [bundle.as_str(), "pi"]
        .into_iter()
        .map(|pi| print_turn(pi, &home, &agent, "stall"))
        .collect();
    let text = |o: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    let (ours, stock) = (&results[0], &results[1]);
    assert_eq!(ours.0.status.code(), stock.0.status.code());
    assert_eq!(text(&ours.0), text(&stock.0));
    // undici reports the body timeout as `terminated`; the default timeout would take 300 s.
    assert!(text(&stock.0).contains("terminated"), "{}", text(&stock.0));
    for (_, took) in &results {
        assert!(
            (Duration::from_secs(1)..Duration::from_secs(20)).contains(took),
            "{took:?}"
        );
    }
}
