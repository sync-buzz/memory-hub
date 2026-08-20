// Owned JSON values keep the call sites lifetime-free and are discarded after
// one serialization/parsing boundary; borrowing them would not improve reuse.
#![allow(clippy::needless_pass_by_value)]

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{MCP_PROTOCOL_VERSION, ServerTarget};

/// Generous on purpose: the harness drives a spawned binary that may be an
/// unoptimized build, where projecting a snapshot into `LanceDB` costs seconds.
/// A timeout should mean "the server is stuck", not "the build is slow".
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);
const INTERRUPT_PROGRESS_TOKEN: &str = "memory-hub-contract/pre-commit";

#[derive(Debug)]
pub(crate) enum CallError {
    Transport(String),
    Rpc { code: i64, data: Value },
    Tool { kind: String, data: Value },
    Protocol(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "transport error: {detail}"),
            Self::Rpc { code, data } => write!(formatter, "JSON-RPC error {code}: {data}"),
            Self::Tool { kind, data } => write!(formatter, "tool error {kind}: {data}"),
            Self::Protocol(detail) => write!(formatter, "protocol error: {detail}"),
        }
    }
}

pub(crate) fn call_tool(
    target: &dyn ServerTarget,
    project: &Path,
    name: &str,
    arguments: Value,
) -> Result<Value, CallError> {
    let mut session = Session::start(target.command(project))?;
    session.initialize()?;
    let result = session.request("tools/call", json!({"name": name, "arguments": arguments}))?;
    parse_tool_result(result)
}

pub(crate) fn read_resource(
    target: &dyn ServerTarget,
    project: &Path,
    uri: &str,
) -> Result<Value, CallError> {
    let mut session = Session::start(target.command(project))?;
    session.initialize()?;
    let result = session.request("resources/read", json!({"uri": uri}))?;
    let text = result
        .get("contents")
        .and_then(Value::as_array)
        .and_then(|contents| contents.first())
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Protocol("resource result has no textual content".to_owned()))?;
    serde_json::from_str(text)
        .map_err(|error| CallError::Protocol(format!("resource content is not JSON: {error}")))
}

/// Send a valid transaction and terminate the server without observing a result.
/// Recovery is checked after starting a completely new public-interface session.
pub(crate) fn interrupt_transaction(
    target: &dyn ServerTarget,
    project: &Path,
    arguments: Value,
) -> Result<(), CallError> {
    let mut session = Session::start(target.interruption_command(project))?;
    session.initialize()?;
    let id = session.take_id();
    session.send(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "_meta": {"progressToken": INTERRUPT_PROGRESS_TOKEN},
            "name": "memory_apply_transaction",
            "arguments": arguments
        }
    }))?;
    if let Some(marker) = target.interruption_marker(project) {
        wait_for_marker(&marker)?;
    } else if target.has_synchronized_interruption() {
        session.wait_for_progress(INTERRUPT_PROGRESS_TOKEN)?;
    }
    session
        .child
        .kill()
        .map_err(|error| CallError::Transport(error.to_string()))?;
    let _ = session.child.wait();
    Ok(())
}

fn wait_for_marker(marker: &Path) -> Result<(), CallError> {
    let started = Instant::now();
    while !marker.is_file() {
        if started.elapsed() >= RESPONSE_TIMEOUT {
            return Err(CallError::Transport(format!(
                "pre-ref-update marker was not created at {}",
                marker.display()
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn parse_tool_result(result: Value) -> Result<Value, CallError> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let error = result
            .get("structuredContent")
            .and_then(|content| content.get("error"))
            .ok_or_else(|| {
                CallError::Protocol("tool error has no structuredContent.error".to_owned())
            })?;
        let kind = error.get("kind").and_then(Value::as_str).ok_or_else(|| {
            CallError::Protocol("tool error has no machine-readable kind".to_owned())
        })?;
        return Err(CallError::Tool {
            kind: kind.to_owned(),
            data: error.get("data").cloned().unwrap_or(Value::Null),
        });
    }

    result
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| CallError::Protocol("tool result has no structuredContent".to_owned()))
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Value, String>>,
    next_id: u64,
}

impl Session {
    fn start(mut command: Command) -> Result<Self, CallError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CallError::Transport(format!("unable to start server: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CallError::Transport("server stdin was not piped".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CallError::Transport("server stdout was not piped".to_owned()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| CallError::Transport("server stderr was not piped".to_owned()))?;
        thread::spawn(move || {
            let _ = io::copy(&mut stderr, &mut io::sink());
        });
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line).map_err(|error| error.to_string())
                });
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            responses,
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<(), CallError> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "memory-hub-contract", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        let negotiated = result.get("protocolVersion").and_then(Value::as_str);
        if negotiated != Some(MCP_PROTOCOL_VERSION) {
            return Err(CallError::Protocol(format!(
                "server negotiated unsupported MCP version {negotiated:?}"
            )));
        }
        for capability in ["resources", "tools"] {
            if !result
                .pointer(&format!("/capabilities/{capability}"))
                .is_some_and(Value::is_object)
            {
                return Err(CallError::Protocol(format!(
                    "server did not advertise required {capability} capability"
                )));
            }
        }
        for field in ["name", "version"] {
            if result
                .pointer(&format!("/serverInfo/{field}"))
                .and_then(Value::as_str)
                .is_none()
            {
                return Err(CallError::Protocol(format!(
                    "initialize result has no serverInfo.{field}"
                )));
            }
        }
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
    }

    fn wait_for_progress(&self, expected_token: &str) -> Result<(), CallError> {
        loop {
            let message = self
                .responses
                .recv_timeout(RESPONSE_TIMEOUT)
                .map_err(|error| {
                    CallError::Transport(format!("pre-commit acknowledgement unavailable: {error}"))
                })?
                .map_err(CallError::Transport)?;
            if message.get("method").and_then(Value::as_str) == Some("notifications/progress")
                && message
                    .pointer("/params/progressToken")
                    .and_then(Value::as_str)
                    == Some(expected_token)
            {
                return Ok(());
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.take_id();
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let response = self
                .responses
                .recv_timeout(RESPONSE_TIMEOUT)
                .map_err(|error| {
                    CallError::Transport(format!("server response unavailable: {error}"))
                })?
                .map_err(CallError::Transport)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(CallError::Rpc {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-32_603),
                    data: error.get("data").cloned().unwrap_or(Value::Null),
                });
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| CallError::Protocol("JSON-RPC response has no result".to_owned()));
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, value: Value) -> Result<(), CallError> {
        serde_json::to_writer(&mut self.stdin, &value)
            .map_err(|error| CallError::Transport(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| CallError::Transport(error.to_string()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
