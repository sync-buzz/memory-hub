//! Independent sample consumer for Memory Hub.
//!
//! This binary proves that the public MCP stdio interface is usable without
//! any private crate dependencies — only `serde_json` for JSON-RPC framing.
//!
//! It performs the full consumer lifecycle:
//! 1. Launch `memory-hub mcp` as a subprocess
//! 2. MCP initialize with memory interface handshake
//! 3. Create a record via `memory_apply_transaction`
//! 4. Read it back via `memory_get_record`
//! 5. Search via `memory_search`
//! 6. Export via `memory_export`
//! 7. Print the results
//!
//! Usage:
//!   memory-hub-sample-consumer <path-to-memory-hub-binary> <project-path>

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MEMORY_INTERFACE_MAJOR: u16 = 1;

/// A minimal MCP stdio client that speaks JSON-RPC 2.0 over the process's
/// stdin/stdout.
struct McpClient {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    /// Launch `memory-hub mcp --project <project>` as a subprocess.
    fn launch(binary: &str, project: &str) -> Result<Self, String> {
        let mut child = Command::new(binary)
            .arg("mcp")
            .arg("--project")
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to launch memory-hub: {e}"))?;

        let stdin = child.stdin.take().ok_or("failed to capture stdin")?;
        let stdout = child.stdout.take().ok_or("failed to capture stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        self.stdin.write_all(b"\n").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;

        // Read response lines until we get our id
        loop {
            let mut response_line = String::new();
            self.stdout
                .read_line(&mut response_line)
                .map_err(|e| e.to_string())?;

            if response_line.trim().is_empty() {
                continue;
            }

            let response: Value = serde_json::from_str(&response_line)
                .map_err(|e| format!("failed to parse response: {e}"))?;

            // Skip notifications (no id)
            if response.get("id").is_none() {
                continue;
            }

            let response_id = response.get("id").and_then(Value::as_u64);
            if response_id == Some(id) {
                if let Some(error) = response.get("error") {
                    return Err(format!("RPC error: {error}"));
                }
                return Ok(response.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// Initialize the MCP session with the memory interface handshake.
    fn initialize(&mut self) -> Result<Value, String> {
        self.call(
            "initialize",
            &json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "sample-consumer",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "_meta": {
                    "memoryHub": {
                        "memoryInterfaceVersion": {
                            "major": MEMORY_INTERFACE_MAJOR,
                            "minor": 0,
                        }
                    }
                }
            }),
        )
    }

    /// Send an initialized notification (no response expected).
    fn notify_initialized(&mut self) -> Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let line = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        self.stdin.write_all(b"\n").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_sample(binary: &str, project: &str) -> Result<(), String> {
    println!("Launching memory-hub MCP server...");
    let mut client = McpClient::launch(binary, project)?;

    // 1. Initialize
    println!("Initializing MCP session...");
    let init_result = client.initialize()?;
    let server_info = &init_result["serverInfo"];
    println!(
        "Connected to {} v{}",
        server_info["name"].as_str().unwrap_or("unknown"),
        server_info["version"].as_str().unwrap_or("unknown")
    );
    client.notify_initialized()?;

    // 2. Get current revision
    println!("\nReading current revision...");
    let revision_result = client.call(
        "resources/read",
        &json!({"uri": "memory://revision/current"}),
    )?;
    let revision_text = &revision_result["contents"][0]["text"];
    let revision: Value =
        serde_json::from_str(revision_text.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
    let current_revision = revision["revision"].as_str().unwrap_or("");
    println!("Current revision: {current_revision}");

    // 3. Create a record
    println!("\nCreating a record...");
    let content = "This is a sample decision record created by the sample consumer.";
    let content_hash = format!("sha256:{}", sha256_hex(content));
    let create_result = client.call(
        "tools/call",
        &json!({
            "name": "memory_apply_transaction",
            "arguments": {
                "transaction_id": "sample-tx-1",
                "expected_revision": current_revision,
                "operations": [{
                    "op": "put",
                    "record": {
                        "representation": "plaintext",
                        "envelope": {
                            "envelope_version": {"major": 1, "minor": 0},
                            "key": "decisions/sample",
                            "kind": "decision",
                            "content": content,
                            "content_hash": content_hash,
                            "title": "Sample Decision",
                            "source_paths": {},
                            "archive": {"archived": false},
                            "freshness": {"state": "unverified"}
                        }
                    }
                }]
            }
        }),
    )?;
    let create_text = &create_result["content"][0]["text"];
    let create_data: Value =
        serde_json::from_str(create_text.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
    let new_revision = create_data["revision"].as_str().unwrap_or("");
    println!("Created record, new revision: {new_revision}");

    // 4. Read it back
    println!("\nReading the record back...");
    let read_result = client.call(
        "resources/read",
        &json!({"uri": "memory://records/decisions/sample"}),
    )?;
    let record_text = &read_result["contents"][0]["text"];
    let record: Value =
        serde_json::from_str(record_text.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
    println!(
        "Record title: {}",
        record["record"]["envelope"]["title"]
            .as_str()
            .unwrap_or("unknown")
    );

    // 5. Search
    println!("\nSearching for 'sample'...");
    let search_result = client.call(
        "tools/call",
        &json!({
            "name": "memory_search",
            "arguments": {"query": "sample"}
        }),
    )?;
    let search_text = &search_result["content"][0]["text"];
    let search_data: Value =
        serde_json::from_str(search_text.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
    let hit_count = search_data["hits"].as_array().map_or(0, Vec::len);
    println!("Found {hit_count} search result(s)");

    // 6. Export
    println!("\nExporting records...");
    let export_result = client.call(
        "tools/call",
        &json!({
            "name": "memory_export",
            "arguments": {"revision": new_revision}
        }),
    )?;
    let export_text = &export_result["content"][0]["text"];
    let export_data: Value =
        serde_json::from_str(export_text.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
    let record_count = export_data["records"].as_array().map_or(0, Vec::len);
    println!("Exported {record_count} record(s)");

    println!("\nSample consumer completed successfully!");
    Ok(())
}

/// SHA-256 of the record content, hex-encoded.
///
/// The envelope contract requires `sha256:` followed by exactly 64 lowercase
/// hex digits and re-derives the digest on every write, so a stand-in hash is
/// rejected before the record reaches the store. `sha2` is an ordinary
/// third-party crate — depending on it does not weaken what this consumer
/// demonstrates, which is that no private Memory Hub crate is needed.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_bytes()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <memory-hub-binary> <project-path>", args[0]);
        std::process::exit(1);
    }
    if let Err(error) = run_sample(&args[1], &args[2]) {
        eprintln!("Sample consumer failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_produces_consistent_output() {
        let hash1 = sha256_hex("test");
        let hash2 = sha256_hex("test");
        assert_eq!(hash1, hash2);
        assert_ne!(sha256_hex("test"), sha256_hex("other"));
    }

    #[test]
    fn sha256_hex_matches_the_envelope_content_hash_contract() {
        // 64 lowercase hex digits, and the well-known digest of the empty
        // string — anything else is rejected by the store before the write.
        let digest = sha256_hex("");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }
}
