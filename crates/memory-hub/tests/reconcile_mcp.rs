#![allow(clippy::expect_used)]
#![allow(clippy::needless_pass_by_value)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use git2::{Oid, Repository, Signature};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(project: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_memory-hub"))
            .args(["mcp", "--project"])
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("MCP process should start");
        let input = child.stdin.take().expect("stdin should be piped");
        let output = BufReader::new(child.stdout.take().expect("stdout should be piped"));
        Self {
            child,
            input,
            output,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(
            &mut self.input,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .expect("request should serialize");
        self.input.write_all(b"\n").expect("request should write");
        self.input.flush().expect("request should flush");
        loop {
            let mut line = String::new();
            self.output
                .read_line(&mut line)
                .expect("response should read");
            let response: Value = serde_json::from_str(&line).expect("response should be JSON");
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return response
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| panic!("request failed: {response}"));
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            ["structuredContent"]
            .clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn commit_file(
    repository: &Repository,
    root: &Path,
    content: &str,
) -> Result<Oid, Box<dyn std::error::Error>> {
    fs::write(root.join("code.rs"), content)?;
    let mut index = repository.index()?;
    index.add_path(Path::new("code.rs"))?;
    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repository.find_tree(tree_oid)?;
    let signature = Signature::now("Test", "test@example.invalid")?;
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    Ok(repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        content,
        &tree,
        &parents,
    )?)
}

#[test]
fn running_mcp_reconciles_before_the_first_memory_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let repository = Repository::init(project.path())?;
    // The project says where its memory lives before it has any.
    let init = std::process::Command::new(env!("CARGO_BIN_EXE_memory-hub"))
        .args(["init", "--records", "refs", "--project"])
        .arg(project.path())
        .output()?;
    assert!(init.status.success(), "init: {init:?}");
    commit_file(&repository, project.path(), "one")?;
    let mut session = Session::start(project.path());
    session.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "reconcile-test", "version": "1"}
        }),
    );
    let revision = session.request(
        "resources/read",
        json!({"uri": "memory://revision/current"}),
    );
    let revision_text = revision
        .pointer("/contents/0/text")
        .and_then(Value::as_str)
        .ok_or("revision resource missing")?;
    let base = serde_json::from_str::<Value>(revision_text)?["revision"]
        .as_str()
        .ok_or("revision missing")?
        .to_owned();

    let code_revision = commit_file(&repository, project.path(), "two")?.to_string();
    let content = "after code commit";
    let applied = session.tool(
        "memory_apply_transaction",
        json!({
            "transaction_id": "after-code",
            "expected_revision": base,
            "operations": [{
                "op": "put",
                "record": {
                    "representation": "plaintext",
                    "envelope": {
                        "envelope_version": {"major": 1, "minor": 0},
                        "key": "new-memory",
                        "kind": "note",
                        "content": content,
                        "source_paths": {},
                        "archive": {"archived": false},
                        "freshness": {"state": "unverified"},
                        "content_hash": format!("sha256:{:x}", Sha256::digest(content.as_bytes()))
                    }
                }
            }]
        }),
    );
    let applied_revision = applied["revision"]
        .as_str()
        .ok_or("apply revision missing")?;
    let history = session.tool("memory_history", json!({"limit": 10}));
    let newest = &history["checkpoints"][0];
    assert_eq!(newest["code_revision"], code_revision);
    assert_eq!(newest["revision"], base);
    assert_ne!(newest["revision"], applied_revision);
    let index_status: Value = serde_json::from_slice(&fs::read(
        project.path().join(".git/memory-hub/index/status.json"),
    )?)?;
    assert_eq!(index_status["state"], "fresh");
    assert_eq!(index_status["indexed_revision"], applied_revision);
    Ok(())
}
