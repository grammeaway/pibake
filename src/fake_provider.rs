//! A local OpenAI chat-completions endpoint that records requests and plays scripts.
//! The script is chosen by the request's model ID, so one server serves every run.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const PROVIDER: &str = "pibake_fake";
pub const BIG_WRITE_PATH: &str = "big_write.txt";
pub const BIG_WRITE_BYTES: usize = 200 * 1024;
/// Real providers stream tool arguments in small pieces; the per-chunk cost is what l11 targets.
pub const DELTA_BYTES: usize = 20;
const HELLO_TEXT: &str = "Hello from pibake_fake.";
const DONE_TEXT: &str = "Done.";
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Hello,
    BigWrite,
}

impl Script {
    pub const ALL: [Script; 2] = [Script::Hello, Script::BigWrite];

    pub fn model_id(self) -> &'static str {
        match self {
            Script::Hello => "hello",
            Script::BigWrite => "big_write",
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

/// Deterministic ASCII text, exactly `BIG_WRITE_BYTES` long, with the characters JSON must escape.
pub fn big_write_content() -> String {
    let mut out = String::with_capacity(BIG_WRITE_BYTES + 128);
    let mut line = 0u32;
    while out.len() < BIG_WRITE_BYTES {
        out.push_str(&format!(
            "{line:06}: let path = \"C:\\\\tmp\\\\{line}\";\tlog(`{{\"n\": {line}}}`);\n"
        ));
        line += 1;
    }
    out.truncate(BIG_WRITE_BYTES);
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
    Text(&'static str),
    WriteCall,
}

fn reply_for(script: Script, body: &Value) -> Reply {
    match script {
        Script::Hello => Reply::Text(HELLO_TEXT),
        Script::BigWrite => {
            let after_tool = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .is_some_and(|m| m["role"] == "tool");
            let offers_write = body["tools"]
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["function"]["name"] == "write"));
            // Extensions make their own model calls (e.g. an observer); only the agent's turn gets the write.
            if offers_write && !after_tool {
                Reply::WriteCall
            } else {
                Reply::Text(DONE_TEXT)
            }
        }
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
    match reply_for(script, body) {
        Reply::Text(text) => {
            out.push(chunk(
                json!({ "role": "assistant", "content": text }),
                Value::Null,
            ));
            out.push(chunk(json!({}), json!("stop")));
        }
        Reply::WriteCall => {
            let args = json!({ "path": BIG_WRITE_PATH, "content": content }).to_string();
            out.push(chunk(
                json!({ "role": "assistant", "tool_calls": [{
                    "index": 0, "id": "call_pibake_1", "type": "function",
                    "function": { "name": "write", "arguments": "" },
                }]}),
                Value::Null,
            ));
            for piece in chunk_str(&args, DELTA_BYTES) {
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
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    accept_loop: Option<JoinHandle<()>>,
}

impl FakeProvider {
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("binding the fake provider")?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let content: Arc<str> = big_write_content().into();
        let accept_loop = {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (requests, content) = (requests.clone(), content.clone());
                    std::thread::spawn(move || {
                        if let Err(e) = serve(stream, &requests, &content) {
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
            accept_loop: Some(accept_loop),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Every request body received so far, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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

fn serve(stream: TcpStream, requests: &Mutex<Vec<Value>>, content: &str) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(request) = read_request(&mut reader)? else {
        return Ok(());
    };
    let mut out = BufWriter::new(&stream);
    let status = |out: &mut BufWriter<&TcpStream>, code: &str, msg: &str| -> Result<()> {
        let body = json!({ "error": { "message": msg } }).to_string();
        write!(
            out,
            "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(out.flush()?)
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
    requests.lock().unwrap().push(body.clone());
    let Some(script) = body["model"].as_str().and_then(Script::from_model_id) else {
        return status(&mut out, "404 Not Found", "unknown model");
    };
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )?;
    out.flush()?;
    for event in events(script, &body, content) {
        write!(out, "data: {event}\n\n")?;
        out.flush()?;
    }
    write!(out, "data: [DONE]\n\n")?;
    out.flush()?;
    drop(out);
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn agent_request(model: &str, last_role: &str) -> Value {
        json!({
            "model": model,
            "messages": [{ "role": "system", "content": "s" }, { "role": last_role, "content": "x" }],
            "tools": [{ "type": "function", "function": { "name": "write" } }],
        })
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
        let content = big_write_content();
        assert_eq!(content.len(), BIG_WRITE_BYTES);
        assert!(content.is_ascii());
        for c in ['"', '\\', '\n', '\t'] {
            assert!(content.contains(c), "{c:?}");
        }
        assert_eq!(content, big_write_content());
    }

    #[test]
    fn big_write_streams_the_whole_file_in_small_deltas() {
        let content = big_write_content();
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
        assert_eq!(
            streamed_text(&events(Script::BigWrite, &side, "c")),
            DONE_TEXT
        );
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
    fn rejects_chunked_requests() {
        let mut reader = "POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".as_bytes();
        assert!(read_request(&mut reader).is_err());
        let mut reader = "POST /x HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc".as_bytes();
        assert_eq!(read_request(&mut reader).unwrap().unwrap().body, b"abc");
        assert!(read_request(&mut "".as_bytes()).unwrap().is_none());
        assert!(read_request(&mut "POST /x HTTP/1.1\r\n".as_bytes()).is_err());
    }
}
