//! What the model and the user see of a Pi setup: tools, system prompt, request options and
//! commands. Captured from one RPC session against the fake provider.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::diff;
use crate::fake_provider::{FakeProvider, PROVIDER, Script};
use crate::harness::{Harness, PiCommand};
use crate::lock::PI_PACKAGE;
use crate::process::Supervised;
use crate::sea;

const TIMEOUT: Duration = Duration::from_secs(120);
const PROMPT: &str = "Say hello.";
const LOG: &str = "verify.log";

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `(name, definition)` in request order.
    pub tools: Vec<(String, Value)>,
    pub system_prompt: String,
    /// The request body without messages and tools.
    pub request_options: Value,
    pub commands: Vec<Value>,
}

impl Snapshot {
    /// Builds a snapshot from the agent's first model request and the `get_commands` data.
    pub fn from_parts(request: &Value, commands: Vec<Value>) -> Result<Self> {
        let messages = request["messages"]
            .as_array()
            .context("request has no messages")?;
        let system = messages
            .first()
            .filter(|m| m["role"] == "system")
            .context("the first message of the agent's request is not the system prompt")?;
        let system_prompt = match &system["content"] {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other)?,
        };
        let tools = request["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .map(|t| {
                        let name = t["function"]["name"].as_str().unwrap_or_default();
                        (name.to_string(), t.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut request_options = request.clone();
        if let Some(obj) = request_options.as_object_mut() {
            obj.remove("messages");
            obj.remove("tools");
        }
        Ok(Self {
            tools,
            system_prompt,
            request_options,
            commands,
        })
    }

    fn normalized(self, n: &Normalizer) -> Self {
        Self {
            tools: self
                .tools
                .into_iter()
                .map(|(k, v)| (n.text(&k), n.value(v)))
                .collect(),
            system_prompt: n.text(&self.system_prompt),
            request_options: n.value(self.request_options),
            commands: self.commands.into_iter().map(|c| n.value(c)).collect(),
        }
    }
}

/// Replaces run-specific paths with placeholders. Longer paths win, and a path only matches
/// whole: `/tmp/h` does not match inside `/tmp/home`.
#[derive(Debug, Clone)]
pub struct Normalizer {
    rules: Vec<(String, String)>,
}

impl Normalizer {
    pub fn new(pairs: impl IntoIterator<Item = (PathBuf, &'static str)>) -> Self {
        let mut rules = Vec::new();
        for (path, name) in pairs {
            let canonical = std::fs::canonicalize(&path).ok();
            for p in std::iter::once(path).chain(canonical) {
                let p = p.to_string_lossy().trim_end_matches('/').to_string();
                if !p.is_empty() && !rules.iter().any(|(q, _)| *q == p) {
                    rules.push((p, format!("<{name}>")));
                }
            }
        }
        rules.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
        Self { rules }
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        'outer: while let Some(c) = rest.chars().next() {
            for (path, placeholder) in &self.rules {
                if let Some(after) = rest.strip_prefix(path.as_str())
                    && !after.starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    out.push_str(placeholder);
                    rest = after;
                    continue 'outer;
                }
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    pub fn value(&self, v: Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(&s)),
            Value::Array(items) => Value::Array(items.into_iter().map(|i| self.value(i)).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, self.value(v))).collect())
            }
            other => other,
        }
    }
}

/// Runs `pi` once in a fresh harness: `get_commands`, then one `hello` prompt.
pub fn capture(
    pi: &PiCommand,
    source: &Path,
    real_home: &Path,
    provider: &FakeProvider,
) -> Result<Snapshot> {
    let harness = Harness::new(source, real_home, &provider.base_url())?;
    let mark = provider.request_count();
    let commands = rpc_session(pi, &harness)
        .with_context(|| format!("`{}`, stderr:\n{}", pi.label, harness.log_tail(LOG)))?;
    let requests = provider.requests();
    let request = requests
        .get(mark)
        .with_context(|| format!("`{}` sent no model request", pi.label))?;
    let normalizer = Normalizer::new(
        [
            (harness.agent_dir.clone(), "agent_dir"),
            (harness.cwd.clone(), "cwd"),
            (harness.home.clone(), "home"),
            (harness.root().to_path_buf(), "harness"),
        ]
        .into_iter()
        .chain(pi.package_dir().map(|d| (d, "pi_package_dir")))
        .chain(extracted_pi(&harness.home.join(".cache/pibake")).map(|d| (d, "pi_package_dir")))
        .chain(extracted_pi(&sea::cache_dir(real_home)).map(|d| (d, "pi_package_dir"))),
    );
    Ok(Snapshot::from_parts(request, commands)?.normalized(&normalizer))
}

/// Where pibake executables extracted their Pi package: at launch under the harness home's
/// cache, or for a snapshot executable, at build time under the real one.
fn extracted_pi(cache: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(cache)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.path().join("node_modules").join(PI_PACKAGE)))
        .filter(|d| d.is_dir())
}

fn rpc_session(pi: &PiCommand, harness: &Harness) -> Result<Vec<Value>> {
    let mut cmd = harness.command(pi, LOG)?;
    cmd.args(["--mode", "rpc", "--no-session", "--provider", PROVIDER])
        .args(["--model", Script::Hello.model_id()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut run = Supervised::spawn(cmd, TIMEOUT)?;
    let mut stdin = run.child.stdin.take().context("no stdin")?;
    let mut events = BufReader::new(run.child.stdout.take().context("no stdout")?).lines();
    let mut next_event = || -> Result<Value> {
        let line = events.next().context("Pi exited before answering")??;
        serde_json::from_str(&line).with_context(|| format!("not JSON: {line}"))
    };
    let mut send = |command: Value| writeln!(stdin, "{command}").context("writing to Pi");

    send(json!({ "id": "pibake_commands", "type": "get_commands" }))?;
    let commands = loop {
        let event = next_event()?;
        if event["id"] == "pibake_commands" {
            if event["success"] != true {
                bail!("get_commands failed: {event}");
            }
            break event["data"]["commands"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
    };
    send(json!({ "id": "pibake_prompt", "type": "prompt", "message": PROMPT }))?;
    loop {
        let event = next_event()?;
        match event["type"].as_str() {
            Some("response") if event["id"] == "pibake_prompt" && event["success"] != true => {
                bail!("prompt rejected: {event}")
            }
            Some("message_end") if event["message"]["stopReason"] == "error" => {
                bail!("model request failed: {}", event["message"]["errorMessage"])
            }
            Some("agent_end") => break,
            _ => {}
        }
    }
    drop(stdin);
    // Drain, so Pi never blocks on a full pipe while shutting down.
    for line in events.by_ref() {
        line?;
    }
    let (status, _) = run.wait()?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(commands)
}

/// A readable report of how `ours` differs from `reference`; empty when they match.
pub fn compare(
    ours: &Snapshot,
    ours_label: &str,
    reference: &Snapshot,
    reference_label: &str,
) -> String {
    let mut out = String::new();
    let mut section = |title: &str, body: String| {
        if !body.is_empty() {
            out.push_str(&format!("## {title}\n{body}\n"));
        }
    };
    let only = |a: &[String], b: &[String], label: &str| {
        let names: Vec<&str> = a
            .iter()
            .filter(|n| !b.contains(n))
            .map(String::as_str)
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            format!("only in `{label}`: {}\n", names.join(", "))
        }
    };

    let tool_names = |s: &Snapshot| s.tools.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    let (ours_tools, reference_tools) = (tool_names(ours), tool_names(reference));
    let mut tools = only(&reference_tools, &ours_tools, reference_label);
    tools += &only(&ours_tools, &reference_tools, ours_label);
    if tools.is_empty() && ours_tools != reference_tools {
        tools += &format!(
            "order differs:\n{}",
            diff::render(
                &diff::lines(&reference_tools.join("\n"), &ours_tools.join("\n")),
                1
            )
        );
    }
    for (name, reference_def) in &reference.tools {
        if let Some((_, ours_def)) = ours.tools.iter().find(|(n, _)| n == name)
            && ours_def != reference_def
        {
            tools += &format!(
                "`{name}` definition:\n{}",
                json_diff(reference_def, ours_def)
            );
        }
    }
    section("Tools", tools);
    section(
        "Request options",
        json_diff(&reference.request_options, &ours.request_options),
    );
    let prompt = diff::render(
        &diff::lines(&reference.system_prompt, &ours.system_prompt),
        3,
    );
    // `diff::lines` compares lines, blind to `\r\n` and a final line break.
    section(
        "System prompt",
        if prompt.is_empty() && reference.system_prompt != ours.system_prompt {
            "differs only in line endings\n".into()
        } else {
            prompt
        },
    );

    let command_key = |c: &Value| {
        format!(
            "{} ({})",
            c["name"].as_str().unwrap_or("?"),
            c["source"].as_str().unwrap_or("?")
        )
    };
    let keys = |s: &Snapshot| s.commands.iter().map(command_key).collect::<Vec<_>>();
    let (ours_keys, reference_keys) = (keys(ours), keys(reference));
    let mut commands = only(&reference_keys, &ours_keys, reference_label);
    commands += &only(&ours_keys, &reference_keys, ours_label);
    let one_per_line = |s: &Snapshot| {
        s.commands
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    commands += &diff::render(
        &diff::lines(&one_per_line(reference), &one_per_line(ours)),
        0,
    );
    section("Commands", commands);
    out
}

fn json_diff(reference: &Value, ours: &Value) -> String {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    diff::render(&diff::lines(&pretty(reference), &pretty(ours)), 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalizer() -> Normalizer {
        Normalizer::new([
            (PathBuf::from("/tmp/pibake-x/home/.pi/agent"), "agent_dir"),
            (PathBuf::from("/tmp/pibake-x/home"), "home"),
            (PathBuf::from("/tmp/pibake-x/work"), "cwd"),
            (PathBuf::from("/opt/pi/"), "pi_package_dir"),
        ])
    }

    #[test]
    fn normalizes_a_recorded_prompt() {
        let prompt = "\
- Main documentation: /opt/pi/README.md
<project_instructions path=\"/tmp/pibake-x/home/.pi/agent/AGENTS.md\">
    <location>/tmp/pibake-x/home/.agents/skills/tdd/SKILL.md</location>
    <location>/home/u/.agents/skills/tdd/SKILL.md</location>
<cwd>
/tmp/pibake-x/work
</cwd>
/opt/pi-other and /tmp/pibake-x/homework stay.";
        assert_eq!(
            normalizer().text(prompt),
            "\
- Main documentation: <pi_package_dir>/README.md
<project_instructions path=\"<agent_dir>/AGENTS.md\">
    <location><home>/.agents/skills/tdd/SKILL.md</location>
    <location>/home/u/.agents/skills/tdd/SKILL.md</location>
<cwd>
<cwd>
</cwd>
/opt/pi-other and /tmp/pibake-x/homework stay."
        );
    }

    #[test]
    fn normalizes_strings_inside_json() {
        let command = json!({
            "name": "init",
            "sourceInfo": { "path": "/tmp/pibake-x/home/.pi/agent/extensions/init.ts", "n": 1 },
        });
        assert_eq!(
            normalizer().value(command),
            json!({ "name": "init", "sourceInfo": { "path": "<agent_dir>/extensions/init.ts", "n": 1 } })
        );
    }

    fn request(tools: &[&str], system: &str) -> Value {
        json!({
            "model": "hello",
            "stream": true,
            "messages": [{ "role": "system", "content": system }, { "role": "user", "content": "x" }],
            "tools": tools.iter().map(|t| json!({ "type": "function", "function": { "name": t, "parameters": {} } })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn extracts_parts_of_a_request() {
        let snap = Snapshot::from_parts(
            &request(&["read", "ask"], "sys"),
            vec![json!({ "name": "c" })],
        )
        .unwrap();
        assert_eq!(
            snap.tools
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["read", "ask"]
        );
        assert_eq!(snap.system_prompt, "sys");
        assert_eq!(
            snap.request_options,
            json!({ "model": "hello", "stream": true })
        );
        let side_call = json!({ "messages": [{ "role": "user", "content": "x" }] });
        assert!(Snapshot::from_parts(&side_call, vec![]).is_err());
    }

    #[test]
    fn equal_snapshots_have_an_empty_report() {
        let snap = Snapshot::from_parts(&request(&["read"], "a\nb"), vec![json!({ "name": "c" })])
            .unwrap();
        assert_eq!(compare(&snap, "x", &snap.clone(), "y"), "");
    }

    #[test]
    fn report_names_missing_tools_and_commands() {
        let full = Snapshot::from_parts(
            &request(&["read", "ask", "recall"], "a\nb"),
            vec![
                json!({ "name": "plan", "source": "extension" }),
                json!({ "name": "skill:tdd", "source": "skill" }),
            ],
        )
        .unwrap();
        let bare = Snapshot::from_parts(
            &request(&["read"], "a\nc"),
            vec![json!({ "name": "skill:tdd", "source": "skill" })],
        )
        .unwrap();
        let report = compare(&bare, "pi -ne", &full, "pi");
        assert!(
            report.contains("## Tools\nonly in `pi`: ask, recall\n"),
            "{report}"
        );
        assert!(
            report.contains("## System prompt\n   a\n - b\n + c\n"),
            "{report}"
        );
        assert!(
            report.contains("only in `pi`: plan (extension)\n"),
            "{report}"
        );
        assert!(!report.contains("Request options"), "{report}");
    }

    #[test]
    fn line_ending_only_prompt_changes_are_reported() {
        let a = Snapshot::from_parts(&request(&[], "a\nb"), vec![]).unwrap();
        for prompt in ["a\nb\n", "a\r\nb"] {
            let b = Snapshot::from_parts(&request(&[], prompt), vec![]).unwrap();
            let report = compare(&b, "b", &a, "a");
            assert!(
                report.contains("## System prompt\ndiffers only in line endings"),
                "{prompt:?}: {report}"
            );
        }
    }

    #[test]
    fn report_shows_schema_and_order_changes() {
        let a = Snapshot::from_parts(&request(&["read", "write"], "s"), vec![]).unwrap();
        let mut b = Snapshot::from_parts(&request(&["write", "read"], "s"), vec![]).unwrap();
        let report = compare(&b, "b", &a, "a");
        assert!(report.contains("order differs"), "{report}");
        b.tools[0].1["function"]["parameters"] = json!({ "type": "object" });
        let report = compare(&b, "b", &a, "a");
        assert!(report.contains("`write` definition:"), "{report}");
        assert!(report.contains("+       \"type\": \"object\""), "{report}");
    }
}
