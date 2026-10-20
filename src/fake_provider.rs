//! A local OpenAI chat-completions endpoint that records requests and plays scripts.
//! The script is chosen by the request's model ID, so one server serves every run.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const PROVIDER: &str = "pibake_fake";
pub const BIG_WRITE_PATH: &str = "big_write.txt";
/// The default size of the `big_write` file.
pub const BIG_WRITE_BYTES: usize = 200 * 1024;
/// Real providers stream tool arguments in small pieces; the per-chunk cost is what l11 targets.
pub const DELTA_BYTES: usize = 20;
const HELLO_TEXT: &str = "Hello from pibake_fake.";
/// The text closing a scripted turn; the session scenarios watch the screen for it.
pub const DONE_TEXT: &str = "pibake-fake-done";
/// The answer to an extension's own model call, which must not look like the turn's end.
const SIDE_TEXT: &str = "pibake-fake-side";
/// Starts the answer to a request the script did not expect, e.g. a tool result missing its end.
pub const UNEXPECTED_TEXT: &str = "pibake-fake-unexpected";

/// The `long_answer` Markdown, streamed `ANSWER_DELTA_BYTES` per delta every `ANSWER_PACE`.
pub const LONG_ANSWER_BYTES: usize = 20 * 1024;
/// The last line of the long answer.
pub const LONG_ANSWER_END: &str = "pibake-answer-end";
const ANSWER_DELTA_BYTES: usize = 24;
const ANSWER_PACE: Duration = Duration::from_millis(20);

/// `seq 1 BIG_OUTPUT_LINES` prints exactly `BIG_OUTPUT_BYTES`.
pub const BIG_OUTPUT_LINES: u64 = 1_187_464;
pub const BIG_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Each `long_session` turn reads this file from the working directory, then answers.
pub const SESSION_FILE: &str = "session_read.md";
pub const SESSION_FILE_BYTES: usize = 16 * 1024;
const SESSION_FILE_END: &str = "pibake-file-end";
pub const SESSION_ANSWER_BYTES: usize = 2 * 1024;
/// The last line of each `long_session` answer.
pub const SESSION_TURN_END: &str = "pibake-turn-end";

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Hello,
    BigWrite,
    LongAnswer,
    BigOutput,
    LongSession,
}

impl Script {
    pub const ALL: [Script; 5] = [
        Script::Hello,
        Script::BigWrite,
        Script::LongAnswer,
        Script::BigOutput,
        Script::LongSession,
    ];

    pub fn model_id(self) -> &'static str {
        match self {
            Script::Hello => "hello",
            Script::BigWrite => "big_write",
            Script::LongAnswer => "long_answer",
            Script::BigOutput => "big_output",
            Script::LongSession => "long_session",
        }
    }

    /// The pause between two streamed events.
    fn pace(self) -> Duration {
        match self {
            Script::LongAnswer => ANSWER_PACE,
            _ => Duration::ZERO,
        }
    }

    fn from_model_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.model_id() == id)
    }
}

/// The `models.json` provider entry pointing Pi at `base_url`.
pub fn provider_entry(base_url: &str) -> Value {
    let models: Vec<Value> = Script::ALL
        .iter()
        .map(|s| json!({ "id": s.model_id(), "name": s.model_id(), "reasoning": false }))
        .collect();
    json!({
        "baseUrl": base_url,
        "api": "openai-completions",
        "apiKey": "pibake",
        "models": models,
    })
}

/// Deterministic ASCII text, exactly `bytes` long, with the characters JSON must escape.
pub fn big_write_content(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes + 128);
    let mut line = 0u32;
    while out.len() < bytes {
        out.push_str(&format!(
            "{line:06}: let path = \"C:\\\\tmp\\\\{line}\";\tlog(`{{\"n\": {line}}}`);\n"
        ));
        line += 1;
    }
    out.truncate(bytes);
    out
}

/// Deterministic Markdown exactly `bytes` long (at least 256): sections with a heading, a
/// paragraph with inline styles and non-ASCII text, a list, a table and a code block in one of
/// four languages, then a filler paragraph and `end` as the last line.
pub fn markdown_answer(bytes: usize, end: &str) -> String {
    const LANGUAGES: [(&str, &str); 4] = [
        (
            "rust",
            "fn main() {\n    let n: u64 = (1..=10).sum();\n    println!(\"{n}\");\n}",
        ),
        (
            "python",
            "def total(xs):\n    return sum(x * 2 for x in xs if x % 3)\n\nprint(total(range(10)))",
        ),
        (
            "javascript",
            "const total = (xs) => xs.reduce((a, x) => a + x, 0);\nconsole.log(`sum: ${total([1, 2, 3])}`);",
        ),
        (
            "bash",
            "for f in *.md; do\n  wc -l \"$f\" | awk '{print $1}'\ndone",
        ),
    ];
    let tail = format!("\n\n{end}\n");
    let mut out = String::with_capacity(bytes);
    for n in 0.. {
        let (lang, code) = LANGUAGES[n % LANGUAGES.len()];
        let section = format!(
            "## Section {n}: {lang}\n\n\
             This paragraph has **bold**, *italic* and `inline code`, a [link](https://example.com/{n}), \
             and text that is not ASCII: caf\u{e9}, na\u{ef}ve, \u{2192}, \u{65e5}\u{672c}.\n\n\
             - first item {n}\n- second item with `code`\n- third item\n\n\
             | key | value | note |\n|-----|-------|------|\n| a{n} | {n} | first |\n| b{n} | {} | second |\n\n\
             ```{lang}\n{code}\n```\n\n",
            n * 2
        );
        if out.len() + section.len() + 64 + tail.len() > bytes {
            break;
        }
        out.push_str(&section);
    }
    let filler = "Filler words keep the answer at its exact size. ";
    while out.len() + tail.len() < bytes {
        out.push_str(filler);
    }
    out.truncate(bytes - tail.len());
    out.push_str(&tail);
    out
}

/// Deterministic ASCII lines exactly `bytes` long (at least 64), ending with `SESSION_FILE_END`.
pub fn session_file_content(bytes: usize) -> String {
    let tail = format!("{SESSION_FILE_END}\n");
    let mut out = String::with_capacity(bytes);
    let mut line = 0u32;
    while out.len() + tail.len() < bytes {
        out.push_str(&format!(
            "{line:04}: the session reads this file on every turn.\n"
        ));
        line += 1;
    }
    out.truncate(bytes - tail.len() - 1);
    out.push('\n');
    out.push_str(&tail);
    out
}

/// Splits `s` into pieces of at most `max_bytes`, on char boundaries. `max_bytes` must be ≥ 4.
pub fn chunk_str(s: &str, max_bytes: usize) -> Vec<&str> {
    assert!(max_bytes >= 4, "a chunk must fit any UTF-8 char");
    let mut chunks = Vec::with_capacity(s.len() / max_bytes + 1);
    let mut rest = s;
    while !rest.is_empty() {
        let mut end = max_bytes.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        chunks.push(head);
        rest = tail;
    }
    chunks
}

enum Reply {
    Text(String),
    ToolCall { name: &'static str, args: Value },
}

/// A message's text: its `content` as a string, or its text parts joined.
pub fn content_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// The text of the request's last message when it is a tool result.
fn tool_result(body: &Value) -> Option<String> {
    let last = body["messages"].as_array()?.last()?;
    (last["role"] == "tool").then(|| content_text(last))
}

fn reply_for(script: Script, body: &Value, content: &str) -> Reply {
    let offers = |tool: &str| {
        body["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["function"]["name"] == tool))
    };
    let text = |s: &str| Reply::Text(s.to_string());
    // Extensions make their own model calls (e.g. an observer); only the agent's turn, which
    // offers the built-in tools, gets the script.
    let result = tool_result(body);
    match script {
        Script::Hello => text(HELLO_TEXT),
        Script::BigWrite if !offers("write") => text(SIDE_TEXT),
        Script::BigWrite => match result {
            None => Reply::ToolCall {
                name: "write",
                args: json!({ "path": BIG_WRITE_PATH, "content": content }),
            },
            Some(_) => text(DONE_TEXT),
        },
        Script::LongAnswer if !offers("write") => text(SIDE_TEXT),
        Script::LongAnswer => Reply::Text(markdown_answer(LONG_ANSWER_BYTES, LONG_ANSWER_END)),
        Script::BigOutput if !offers("bash") => text(SIDE_TEXT),
        Script::BigOutput => match result {
            None => Reply::ToolCall {
                name: "bash",
                args: json!({ "command": format!("seq 1 {BIG_OUTPUT_LINES}") }),
            },
            Some(out) if out.lines().any(|l| l == BIG_OUTPUT_LINES.to_string()) => text(DONE_TEXT),
            Some(_) => text(&format!(
                "{UNEXPECTED_TEXT}: the output lacks its last line"
            )),
        },
        Script::LongSession if !offers("read") => text(SIDE_TEXT),
        Script::LongSession => match result {
            None => Reply::ToolCall {
                name: "read",
                args: json!({ "path": SESSION_FILE }),
            },
            Some(out) if out.contains(SESSION_FILE_END) => {
                Reply::Text(markdown_answer(SESSION_ANSWER_BYTES, SESSION_TURN_END))
            }
            Some(_) => text(&format!("{UNEXPECTED_TEXT}: the read lacks the file's end")),
        },
    }
}

/// The SSE `data:` payloads for one response, `[DONE]` excluded.
fn events(script: Script, body: &Value, content: &str) -> Vec<Value> {
    let model = script.model_id();
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "pibake-1", "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let mut out = Vec::new();
    match reply_for(script, body, content) {
        Reply::Text(text) => {
            for (i, piece) in chunk_str(&text, ANSWER_DELTA_BYTES).into_iter().enumerate() {
                let delta = if i == 0 {
                    json!({ "role": "assistant", "content": piece })
                } else {
                    json!({ "content": piece })
                };
                out.push(chunk(delta, Value::Null));
            }
            out.push(chunk(json!({}), json!("stop")));
        }
        Reply::ToolCall { name, args } => {
            // Unique per request: a session's turns each call a tool.
            let id = format!(
                "call_pibake_{}",
                body["messages"].as_array().map_or(0, Vec::len)
            );
            out.push(chunk(
                json!({ "role": "assistant", "tool_calls": [{
                    "index": 0, "id": id, "type": "function",
                    "function": { "name": name, "arguments": "" },
                }]}),
                Value::Null,
            ));
            for piece in chunk_str(&args.to_string(), DELTA_BYTES) {
                out.push(chunk(
                    json!({ "tool_calls": [{ "index": 0, "function": { "arguments": piece } }] }),
                    Value::Null,
                ));
            }
            out.push(chunk(json!({}), json!("tool_calls")));
        }
    }
    out.push(json!({
        "id": "pibake-1", "object": "chat.completion.chunk", "created": 0, "model": model,
        "choices": [], "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    }));
    out
}

pub struct FakeProvider {
    port: u16,
    /// `None` when unrecorded.
    requests: Option<Arc<Mutex<Vec<Value>>>>,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    accept_loop: Option<JoinHandle<()>>,
}

impl FakeProvider {
    pub fn start() -> Result<Self> {
        Self::new(BIG_WRITE_BYTES, true, false)
    }

    /// As `start`, keeping each connection open for the client's next request, as real
    /// providers do; answers are sent chunked.
    pub fn keep_alive() -> Result<Self> {
        Self::new(BIG_WRITE_BYTES, true, true)
    }

    /// As `start`, with a `big_write` file of `bytes` and no requests kept: a bench would grow by
    /// every context sent, and a forked Pi starts with that RSS in its `ru_maxrss`.
    pub fn unrecorded(bytes: usize) -> Result<Self> {
        Self::new(bytes, false, false)
    }

    fn new(bytes: usize, record: bool, keep_alive: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("binding the fake provider")?;
        let port = listener.local_addr()?.port();
        let requests = record.then(|| Arc::new(Mutex::new(Vec::new())));
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let content: Arc<str> = big_write_content(bytes).into();
        let accept_loop = {
            let (requests, stop, connections) =
                (requests.clone(), stop.clone(), connections.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    connections.fetch_add(1, Ordering::SeqCst);
                    let (requests, content) = (requests.clone(), content.clone());
                    std::thread::spawn(move || {
                        if let Err(e) = serve(stream, requests.as_deref(), &content, keep_alive) {
                            eprintln!("pibake_fake: {e:#}");
                        }
                    });
                }
            })
        };
        Ok(Self {
            port,
            requests,
            stop,
            connections,
            accept_loop: Some(accept_loop),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Every request body received so far, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.recorded().clone()
    }

    pub fn request_count(&self) -> usize {
        self.recorded().len()
    }

    fn recorded(&self) -> std::sync::MutexGuard<'_, Vec<Value>> {
        let requests = self.requests.as_ref();
        requests.expect("an unrecorded provider").lock().unwrap()
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocking accept so the loop sees `stop`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.accept_loop.take() {
            let _ = handle.join();
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// `None` when the client closed without sending anything (Pi does, when it exits mid-call).
fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        reader.read_until(b'\n', &mut head)?;
        if head.is_empty() {
            return Ok(None);
        }
        if head.len() == before {
            bail!("connection closed inside the request head");
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD_BYTES {
            bail!("request head over {MAX_HEAD_BYTES} bytes");
        }
    }
    let head = String::from_utf8(head).context("request head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let mut length = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().context("bad content-length")?,
            "transfer-encoding" => bail!("chunked request bodies are not supported"),
            _ => {}
        }
    }
    if length > MAX_BODY_BYTES {
        bail!("request body of {length} bytes is over the limit");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(Request { method, path, body }))
}

/// Answers one request per connection, or, with `keep_alive`, every request until the client
/// closes.
fn serve(
    stream: TcpStream,
    requests: Option<&Mutex<Vec<Value>>>,
    content: &str,
    keep_alive: bool,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    while let Some(request) = read_request(&mut reader)? {
        if !respond(&stream, request, requests, content, keep_alive)? {
            break;
        }
    }
    Ok(())
}

/// Whether the connection stays open for another request.
fn respond(
    stream: &TcpStream,
    request: Request,
    requests: Option<&Mutex<Vec<Value>>>,
    content: &str,
    keep_alive: bool,
) -> Result<bool> {
    let mut out = BufWriter::new(stream);
    let status = |out: &mut BufWriter<&TcpStream>, code: &str, msg: &str| -> Result<bool> {
        let body = json!({ "error": { "message": msg } }).to_string();
        write!(
            out,
            "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        out.flush()?;
        Ok(false)
    };
    if request.method != "POST" || !request.path.ends_with("/chat/completions") {
        return status(
            &mut out,
            "404 Not Found",
            "pibake_fake serves POST /chat/completions only",
        );
    }
    let body: Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(e) => {
            return status(
                &mut out,
                "400 Bad Request",
                &format!("body is not JSON: {e}"),
            );
        }
    };
    if let Some(requests) = requests {
        requests.lock().unwrap().push(body.clone());
    }
    let Some(script) = body["model"].as_str().and_then(Script::from_model_id) else {
        return status(&mut out, "404 Not Found", "unknown model");
    };
    let framing = if keep_alive {
        "Transfer-Encoding: chunked\r\nConnection: keep-alive"
    } else {
        "Connection: close"
    };
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n{framing}\r\n\r\n"
    )?;
    out.flush()?;
    let mut send = |data: &str| -> Result<()> {
        if keep_alive {
            write!(out, "{:x}\r\n{data}\r\n", data.len())?;
        } else {
            out.write_all(data.as_bytes())?;
        }
        Ok(out.flush()?)
    };
    for (i, event) in events(script, &body, content).into_iter().enumerate() {
        if i > 0 {
            std::thread::sleep(script.pace());
        }
        send(&format!("data: {event}\n\n"))?;
    }
    send("data: [DONE]\n\n")?;
    if keep_alive {
        out.write_all(b"0\r\n\r\n")?;
        out.flush()?;
        return Ok(true);
    }
    drop(out);
    let _ = stream.shutdown(Shutdown::Write);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn agent_request(model: &str, last_role: &str) -> Value {
        json!({
            "model": model,
            "messages": [{ "role": "system", "content": "s" }, { "role": last_role, "content": "x" }],
            "tools": (["read", "bash", "write"]
                .map(|name| json!({ "type": "function", "function": { "name": name } }))),
        })
    }

    /// `agent_request` with a tool result as the last message.
    fn after_tool(model: &str, result: &str) -> Value {
        let mut body = agent_request(model, "tool");
        body["messages"][1]["content"] = json!([{ "type": "text", "text": result }]);
        body
    }

    fn tool_call(events: &[Value]) -> (String, Value) {
        let name = events[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        (name, serde_json::from_str(&streamed_args(events)).unwrap())
    }

    fn streamed_text(events: &[Value]) -> String {
        events
            .iter()
            .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    fn streamed_args(events: &[Value]) -> String {
        events
            .iter()
            .filter_map(|e| {
                e["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .collect()
    }

    #[test]
    fn content_is_exact_size_and_needs_escaping() {
        let content = big_write_content(BIG_WRITE_BYTES);
        assert_eq!(content.len(), BIG_WRITE_BYTES);
        assert!(content.is_ascii());
        for c in ['"', '\\', '\n', '\t'] {
            assert!(content.contains(c), "{c:?}");
        }
        assert_eq!(content, big_write_content(BIG_WRITE_BYTES));
    }

    #[test]
    fn big_write_streams_the_whole_file_in_small_deltas() {
        let content = big_write_content(BIG_WRITE_BYTES);
        let events = events(
            Script::BigWrite,
            &agent_request("big_write", "user"),
            &content,
        );
        let args: Value = serde_json::from_str(&streamed_args(&events)).unwrap();
        assert_eq!(args["path"], BIG_WRITE_PATH);
        assert_eq!(args["content"].as_str().unwrap(), content);
        let deltas = events.len() - 3;
        assert!(deltas > BIG_WRITE_BYTES / DELTA_BYTES, "{deltas}");
        assert_eq!(
            events[events.len() - 2]["choices"][0]["finish_reason"],
            "tool_calls"
        );
        assert!(events.last().unwrap()["usage"].is_object());
    }

    #[hegel::test(test_cases = 300)]
    fn any_content_survives_the_stream(tc: hegel::TestCase) {
        let content = tc.draw(hegel::generators::text().max_size(300));
        let events = events(
            Script::BigWrite,
            &agent_request("big_write", "user"),
            &content,
        );
        let args: Value = serde_json::from_str(&streamed_args(&events)).unwrap();
        assert_eq!(args["content"], content);
        // One `data:` line per event: a raw newline would break SSE framing.
        assert!(events.iter().all(|e| !e.to_string().contains('\n')));
    }

    #[test]
    fn big_write_answers_text_after_the_tool_and_to_side_calls() {
        let after = events(Script::BigWrite, &agent_request("big_write", "tool"), "c");
        assert_eq!(streamed_text(&after), DONE_TEXT);
        let side =
            json!({ "model": "big_write", "messages": [{ "role": "user", "content": "x" }] });
        for script in Script::ALL.into_iter().skip(1) {
            let mut side = side.clone();
            side["model"] = json!(script.model_id());
            assert_eq!(
                streamed_text(&events(script, &side, "c")),
                SIDE_TEXT,
                "{script:?}"
            );
        }
    }

    #[test]
    fn markdown_answers_are_exact_size_and_end_with_their_marker() {
        for bytes in [256, 300, 2 * 1024, 5000, LONG_ANSWER_BYTES] {
            let md = markdown_answer(bytes, LONG_ANSWER_END);
            assert_eq!(md.len(), bytes);
            assert!(md.ends_with(&format!("\n\n{LONG_ANSWER_END}\n")), "{bytes}");
            assert_eq!(md.matches(LONG_ANSWER_END).count(), 1);
        }
        let md = markdown_answer(LONG_ANSWER_BYTES, LONG_ANSWER_END);
        for needle in [
            "## ",
            "**bold**",
            "| key |",
            "```rust",
            "```python",
            "```javascript",
            "```bash",
            "\u{2192}",
        ] {
            assert!(md.contains(needle), "{needle}");
        }
        assert_eq!(
            md.matches("```").count() % 2,
            0,
            "every code block is closed"
        );
    }

    #[test]
    fn the_long_answer_streams_small_paced_deltas() {
        let events = events(
            Script::LongAnswer,
            &agent_request("long_answer", "user"),
            "c",
        );
        assert_eq!(
            streamed_text(&events),
            markdown_answer(LONG_ANSWER_BYTES, LONG_ANSWER_END)
        );
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            deltas
                .iter()
                .all(|d| !d.is_empty() && d.len() <= ANSWER_DELTA_BYTES)
        );
        assert_eq!(
            deltas.len(),
            LONG_ANSWER_BYTES.div_ceil(ANSWER_DELTA_BYTES) + 1
        );
        assert_eq!(Script::LongAnswer.pace(), Duration::from_millis(20));
        assert_eq!(Script::LongSession.pace(), Duration::ZERO);
    }

    #[test]
    fn big_output_is_8_mib_of_seq() {
        // `seq` prints the numbers of each digit count d, 9 * 10^(d-1) of them, d + 1 bytes each.
        let mut bytes = 0;
        let mut first = 1;
        while first <= BIG_OUTPUT_LINES {
            let last = (first * 10 - 1).min(BIG_OUTPUT_LINES);
            bytes += (last - first + 1) * (first.to_string().len() as u64 + 1);
            first *= 10;
        }
        assert_eq!(bytes, BIG_OUTPUT_BYTES);
        let call = events(Script::BigOutput, &agent_request("big_output", "user"), "c");
        assert_eq!(
            tool_call(&call),
            ("bash".to_string(), json!({ "command": "seq 1 1187464" }))
        );
        let tail =
            "1187463\n1187464\n\n[Showing lines 1185465-1187464 of 1187464. Full output: /tmp/x]";
        let done = events(Script::BigOutput, &after_tool("big_output", tail), "c");
        assert_eq!(streamed_text(&done), DONE_TEXT);
        let cut = events(Script::BigOutput, &after_tool("big_output", "1\n2\n"), "c");
        assert!(streamed_text(&cut).starts_with(UNEXPECTED_TEXT));
    }

    #[test]
    fn a_session_turn_reads_the_file_then_answers() {
        let file = session_file_content(SESSION_FILE_BYTES);
        assert_eq!(file.len(), SESSION_FILE_BYTES);
        assert!(file.is_ascii() && file.ends_with(&format!("\n{SESSION_FILE_END}\n")));
        let call = events(
            Script::LongSession,
            &agent_request("long_session", "user"),
            "c",
        );
        assert_eq!(
            tool_call(&call),
            ("read".to_string(), json!({ "path": SESSION_FILE }))
        );
        let answer = events(Script::LongSession, &after_tool("long_session", &file), "c");
        assert_eq!(
            streamed_text(&answer),
            markdown_answer(SESSION_ANSWER_BYTES, SESSION_TURN_END)
        );
        let mut string_content = agent_request("long_session", "tool");
        string_content["messages"][1]["content"] = json!(file);
        let answer = events(Script::LongSession, &string_content, "c");
        assert!(streamed_text(&answer).ends_with(&format!("{SESSION_TURN_END}\n")));
        let cut = events(
            Script::LongSession,
            &after_tool("long_session", &file[..100]),
            "c",
        );
        assert!(streamed_text(&cut).starts_with(UNEXPECTED_TEXT));
    }

    #[hegel::test(test_cases = 100)]
    fn markdown_answers_fit_any_size(tc: hegel::TestCase) {
        let bytes = tc.draw(
            hegel::generators::integers::<usize>()
                .min_value(256)
                .max_value(40_000),
        );
        let md = markdown_answer(bytes, SESSION_TURN_END);
        assert_eq!(md.len(), bytes);
        assert!(md.ends_with(&format!("\n\n{SESSION_TURN_END}\n")));
    }

    #[test]
    fn hello_is_text_only() {
        let events = events(Script::Hello, &agent_request("hello", "user"), "c");
        assert_eq!(streamed_text(&events), HELLO_TEXT);
        assert_eq!(streamed_args(&events), "");
        assert_eq!(events[1]["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn chunking_respects_char_boundaries() {
        assert_eq!(chunk_str("", 4), Vec::<&str>::new());
        assert_eq!(chunk_str("abcdefghi", 4), ["abcd", "efgh", "i"]);
        assert_eq!(chunk_str("aé€😀", 4), ["aé", "€", "😀"]);
    }

    fn post(port: u16, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn serves_and_records_over_http() {
        let server = FakeProvider::start().unwrap();
        let port = server.port();
        let body = agent_request("hello", "user").to_string();
        let response = post(port, &body);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("text/event-stream"));
        assert!(response.contains(HELLO_TEXT));
        assert!(response.ends_with("data: [DONE]\n\n"));
        assert_eq!(server.requests(), vec![agent_request("hello", "user")]);

        let unknown = post(port, &json!({ "model": "nope" }).to_string());
        assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");
        assert!(post(port, "{not json").starts_with("HTTP/1.1 400"));
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn keep_alive_answers_chunked_on_one_connection() {
        let server = FakeProvider::keep_alive().unwrap();
        let stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let body = agent_request("hello", "user").to_string();
        for _ in 0..2 {
            write!(
                &stream,
                "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut response = String::new();
            while !response.ends_with("\r\n0\r\n\r\n") {
                assert!(reader.read_line(&mut response).unwrap() > 0, "{response}");
            }
            assert!(response.contains("Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n"));
            assert!(response.contains(HELLO_TEXT));
            assert!(
                response.contains("\r\ne\r\ndata: [DONE]\n\n\r\n"),
                "{response}"
            );
        }
        assert_eq!(server.request_count(), 2);
        assert_eq!(server.connections(), 1);
    }

    #[test]
    fn rejects_chunked_requests() {
        let mut reader = "POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".as_bytes();
        assert!(read_request(&mut reader).is_err());
        let mut reader = "POST /x HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc".as_bytes();
        assert_eq!(read_request(&mut reader).unwrap().unwrap().body, b"abc");
        assert!(read_request(&mut "".as_bytes()).unwrap().is_none());
        assert!(read_request(&mut "POST /x HTTP/1.1\r\n".as_bytes()).is_err());
    }
}
