#![allow(clippy::expect_used, clippy::unwrap_used)]
// Owned `Value` arguments keep the JSON-RPC call sites readable; they are
// serialized once and dropped, so borrowing them would not save work.
#![allow(clippy::needless_pass_by_value)]

//! Integration coverage for the listing API (GITMEMO-18):
//! pagination, filters, sort, metadata-only mode and summary counts.
//!
//! Drives the real `memory-hub mcp` stdio server over JSON-RPC so the
//! assertions exercise the public contract end to end.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::Digest;
use tempfile::TempDir;

/// Generous on purpose: these tests drive the real binary, `cargo test` builds
/// it unoptimized, and the whole file runs in parallel — building a `LanceDB`
/// projection under that load takes far longer than it does in a release
/// build. A timeout here should mean "the server is stuck", not "the machine
/// was busy".
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_memory-hub"))
}

fn init_repository() -> TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(dir.path())
        .output()
        .expect("git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Nothing else to prepare: `mcp` keeps a repository's records in Git, and
    // opening them is what creates them.
    dir
}

/// A single persistent MCP session over the binary's stdio boundary.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<Value>,
    next_id: u64,
}

impl McpSession {
    fn start(project: &Path) -> Self {
        let mut child = Command::new(binary())
            .args(["mcp", "--project"])
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("memory-hub mcp starts");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&line)
                            && tx.send(value).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut session = Self {
            child,
            stdin,
            rx,
            next_id: 1,
        };
        session.initialize();
        session
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        serde_json::to_writer(&mut self.stdin, &request).expect("write request");
        self.stdin.write_all(b"\n").expect("newline");
        self.stdin.flush().expect("flush");
        let response = self.recv();
        if let Some(error) = response.get("error") {
            panic!("JSON-RPC error for {method}: {error}");
        }
        response.get("result").cloned().unwrap_or(response)
    }

    fn notify(&mut self, method: &str, params: Value) {
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        serde_json::to_writer(&mut self.stdin, &request).expect("write notify");
        self.stdin.write_all(b"\n").expect("newline");
        self.stdin.flush().expect("flush");
    }

    fn recv(&self) -> Value {
        self.rx
            .recv_timeout(RESPONSE_TIMEOUT)
            .expect("server responded within timeout")
    }

    fn initialize(&mut self) {
        let result = self.send(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "listing-api-test", "version": "0.0.0"},
            }),
        );
        assert!(result.get("protocolVersion").is_some(), "initialize ok");
        self.notify("notifications/initialized", json!({}));
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.send("tools/call", json!({"name": name, "arguments": arguments}));

        result.get("structuredContent").cloned().unwrap_or(result)
    }

    fn read_resource(&mut self, uri: &str) -> Value {
        let result = self.send("resources/read", json!({"uri": uri}));
        let text = result
            .get("contents")
            .and_then(Value::as_array)
            .and_then(|contents| contents.first())
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
            .expect("resource has text content");
        serde_json::from_str(text).expect("resource content is JSON")
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a put operation with a fully-specified envelope.
fn put_record(
    key: &str,
    kind: &str,
    title: &str,
    content: &str,
    tags: &[&str],
    freshness: &str,
    archived: bool,
) -> Value {
    let content_hash = format!("sha256:{:x}", sha2::Sha256::digest(content.as_bytes()));
    let envelope = json!({
        "envelope_version": {"major": 1, "minor": 0},
        "key": key,
        "kind": kind,
        "content": content,
        "title": title,
        "tags": tags,
        "links": [],
        "source_paths": {},
        "archive": {"archived": archived},
        "freshness": {"state": freshness},
        "content_hash": content_hash,
        "profile": {
            "name": "listing.test",
            "version": {"major": 1, "minor": 0},
            "metadata": {}
        }
    });
    json!({"op": "put", "record": {"representation": "plaintext", "envelope": envelope}})
}

fn apply(session: &mut McpSession, tx_id: &str, expected: &str, ops: Vec<Value>) -> String {
    let result = session.call_tool(
        "memory_apply_transaction",
        json!({
            "transaction_id": tx_id,
            "expected_revision": expected,
            "operations": ops,
        }),
    );
    result
        .get("revision")
        .and_then(Value::as_str)
        .expect("apply returns revision")
        .to_owned()
}

fn current_revision(session: &mut McpSession) -> String {
    let resource = session.read_resource("memory://revision/current");
    resource
        .get("revision")
        .and_then(Value::as_str)
        .expect("revision resource")
        .to_owned()
}

fn list_records(session: &mut McpSession, args: Value) -> Value {
    session.call_tool("memory_list_records", args)
}

/// Seed a corpus with varied kinds, tags, freshness and archived state so
/// every filter and sort axis has discriminating fixtures.
fn seed_corpus(session: &mut McpSession) -> String {
    let base = current_revision(session);
    let ops = vec![
        put_record(
            "alpha",
            "decision",
            "Alpha decision",
            "decide A",
            &["auth", "core"],
            "fresh",
            false,
        ),
        put_record(
            "beta",
            "constraint",
            "Beta constraint",
            "must B",
            &["auth"],
            "stale",
            false,
        ),
        put_record(
            "gamma",
            "observation",
            "Gamma observation",
            "observed G",
            &["core", "infra"],
            "unverified",
            false,
        ),
        put_record(
            "delta",
            "decision",
            "Delta decision",
            "decide D",
            &["infra"],
            "fresh",
            true,
        ),
        put_record(
            "epsilon",
            "note",
            "Epsilon note",
            "note E",
            &["core"],
            "invalid",
            false,
        ),
        put_record(
            "zeta",
            "note",
            "Zeta note",
            "note Z",
            &["auth", "infra"],
            "fresh",
            true,
        ),
    ];
    apply(session, "seed", &base, ops)
}

#[test]
fn list_returns_all_records_with_counts() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(&mut session, json!({"limit": 50}));
    let records = result["records"].as_array().expect("records array");
    assert_eq!(records.len(), 6, "all six seeded records return");
    assert_eq!(result["total"], 6);
    assert_eq!(result["limit"], 50);
    assert_eq!(result["offset"], 0);
    assert_eq!(result["has_more"], false);

    let counts = &result["counts"];
    assert_eq!(counts["total"], 6);
    assert_eq!(counts["by_kind"]["decision"], 2);
    assert_eq!(counts["by_kind"]["note"], 2);
    assert_eq!(counts["by_kind"]["constraint"], 1);
    assert_eq!(counts["by_kind"]["observation"], 1);
    assert_eq!(counts["archived"], 2);
    assert_eq!(counts["live"], 4);
}

#[test]
fn pagination_returns_pages_and_has_more() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let page1 = list_records(
        &mut session,
        json!({"limit": 2, "offset": 0, "sort": "key"}),
    );
    let p1: Vec<&str> = page1["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(p1, vec!["alpha", "beta"]);
    assert_eq!(page1["total"], 6, "total reflects full corpus");
    assert_eq!(page1["has_more"], true);

    let page2 = list_records(
        &mut session,
        json!({"limit": 2, "offset": 2, "sort": "key"}),
    );
    let p2: Vec<&str> = page2["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(p2, vec!["delta", "epsilon"]);
    assert_eq!(page2["has_more"], true);

    let page3 = list_records(
        &mut session,
        json!({"limit": 2, "offset": 4, "sort": "key"}),
    );
    let p3: Vec<&str> = page3["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(p3, vec!["gamma", "zeta"]);
    assert_eq!(page3["has_more"], false, "last page has no more");
}

#[test]
fn limit_is_capped_at_max_two_hundred() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(&mut session, json!({"limit": 9999}));
    assert_eq!(result["limit"], 200, "limit clamped to 200");
}

#[test]
fn filter_by_kind_returns_matching_subset() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(&mut session, json!({"kind": "decision"}));
    let keys: Vec<&str> = result["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["alpha", "delta"]);
    assert_eq!(result["total"], 2);
    assert_eq!(result["counts"]["by_kind"]["decision"], 2);
    assert_eq!(
        result["counts"]["by_kind"].as_object().unwrap().len(),
        1,
        "counts only contain the filtered kind"
    );
}

#[test]
fn filter_by_tags_requires_all_tags_and_semantics() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let both = list_records(&mut session, json!({"tags": ["auth", "core"]}));
    let keys: Vec<&str> = both["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["alpha"], "only alpha has both auth and core");

    let one = list_records(&mut session, json!({"tags": ["infra"]}));
    let keys: Vec<&str> = one["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["delta", "gamma", "zeta"], "infra matches three");
}

#[test]
fn filter_by_archived_splits_live_and_archived() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let archived = list_records(&mut session, json!({"archived": true, "sort": "key"}));
    let keys: Vec<&str> = archived["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["delta", "zeta"]);
    assert_eq!(archived["counts"]["archived"], 2);
    assert_eq!(archived["counts"]["live"], 0);

    let live = list_records(&mut session, json!({"archived": false, "sort": "key"}));
    let keys: Vec<&str> = live["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["alpha", "beta", "epsilon", "gamma"]);
}

#[test]
fn filter_by_freshness_matches_states() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let fresh = list_records(&mut session, json!({"freshness": ["fresh"], "sort": "key"}));
    let keys: Vec<&str> = fresh["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["alpha", "delta", "zeta"]);

    let mixed = list_records(
        &mut session,
        json!({"freshness": ["stale", "invalid"], "sort": "key"}),
    );
    let keys: Vec<&str> = mixed["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["beta", "epsilon"]);
}

#[test]
fn sort_by_kind_title_and_freshness_in_both_orders() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let by_kind_asc = list_records(&mut session, json!({"sort": "kind", "sort_order": "asc"}));
    let kinds: Vec<&str> = by_kind_asc["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "constraint",
            "decision",
            "decision",
            "note",
            "note",
            "observation"
        ]
    );

    let by_kind_desc = list_records(&mut session, json!({"sort": "kind", "sort_order": "desc"}));
    let kinds: Vec<&str> = by_kind_desc["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "observation",
            "note",
            "note",
            "decision",
            "decision",
            "constraint"
        ]
    );

    let by_title = list_records(&mut session, json!({"sort": "title", "sort_order": "asc"}));
    let titles: Vec<&str> = by_title["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec![
            "Alpha decision",
            "Beta constraint",
            "Delta decision",
            "Epsilon note",
            "Gamma observation",
            "Zeta note"
        ]
    );

    let by_freshness = list_records(
        &mut session,
        json!({"sort": "freshness", "sort_order": "asc", "metadata_only": true}),
    );
    let states: Vec<&str> = by_freshness["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["freshness"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        vec!["fresh", "fresh", "fresh", "invalid", "stale", "unverified"],
        "freshness sorts alphabetically: fresh < invalid < stale < unverified"
    );
}

#[test]
fn metadata_only_omits_content_and_links() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(&mut session, json!({"metadata_only": true, "limit": 1}));
    let record = &result["records"][0];
    assert!(record.get("content").is_none(), "content omitted");
    assert!(record.get("links").is_none(), "links omitted");
    assert!(record.get("source_paths").is_none(), "source_paths omitted");
    assert!(record.get("archive").is_none(), "archive object omitted");
    assert!(record["key"].is_string(), "key present");
    assert!(record["kind"].is_string(), "kind present");
    assert!(record["title"].is_string(), "title present");
    assert!(record["tags"].is_array(), "tags present");
    assert!(record["archived"].is_boolean(), "archived flag present");
    assert!(record["freshness"].is_string(), "freshness present");
    assert!(record["content_hash"].is_string(), "content_hash present");
}

#[test]
fn full_record_includes_content_and_links_when_not_metadata_only() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(&mut session, json!({"limit": 1, "sort": "key"}));
    let record = &result["records"][0];
    assert_eq!(record["key"], "alpha");
    assert!(
        record.get("content").is_some(),
        "content present in full mode"
    );
    assert!(record.get("links").is_some(), "links present in full mode");
    assert!(record.get("archive").is_some(), "archive object present");
    assert!(
        record
            .get("freshness")
            .is_some_and(serde_json::Value::is_object),
        "freshness object present"
    );
}

#[test]
fn summary_resource_returns_counts_without_pagination() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let summary = session.read_resource("memory://records/summary");
    assert_eq!(summary["total"], 6);
    assert_eq!(summary["by_kind"]["decision"], 2);
    assert_eq!(summary["by_kind"]["note"], 2);
    assert_eq!(summary["by_kind"]["constraint"], 1);
    assert_eq!(summary["by_kind"]["observation"], 1);
    assert_eq!(summary["by_freshness"]["fresh"], 3);
    assert_eq!(summary["by_freshness"]["stale"], 1);
    assert_eq!(summary["by_freshness"]["unverified"], 1);
    assert_eq!(summary["by_freshness"]["invalid"], 1);
    assert_eq!(summary["archived"], 2);
    assert_eq!(summary["live"], 4);
}

#[test]
fn combined_filters_narrow_to_a_single_record() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());
    seed_corpus(&mut session);

    let result = list_records(
        &mut session,
        json!({"kind": "note", "tags": ["infra"], "archived": true}),
    );
    let keys: Vec<&str> = result["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["zeta"], "only zeta is an archived infra note");
    assert_eq!(result["total"], 1);
}

#[test]
fn empty_corpus_returns_empty_page_and_zero_counts() {
    let repo = init_repository();
    let mut session = McpSession::start(repo.path());

    let result = list_records(&mut session, json!({}));
    assert_eq!(result["records"].as_array().unwrap().len(), 0);
    assert_eq!(result["total"], 0);
    assert_eq!(result["has_more"], false);
    assert_eq!(result["counts"]["total"], 0);
    assert_eq!(result["counts"]["archived"], 0);
    assert_eq!(result["counts"]["live"], 0);

    let summary = session.read_resource("memory://records/summary");
    assert_eq!(summary["total"], 0);
}
