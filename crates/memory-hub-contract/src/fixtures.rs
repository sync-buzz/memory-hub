// Fixture builders intentionally consume their one-shot JSON inputs.
#![allow(clippy::needless_pass_by_value)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) fn record(key: &str, content: &str) -> Value {
    let content_hash = format!("sha256:{:x}", Sha256::digest(content.as_bytes()));
    json!({
        "representation": "plaintext",
        "envelope": {
            "envelope_version": {"major": 1, "minor": 0},
            "key": key,
            "kind": "note",
            "content": content,
            "title": "Behavioral contract fixture",
            "tags": ["contract"],
            "links": [],
            "source_paths": {},
            "archive": {"archived": false},
            "freshness": {"state": "unverified"},
            "content_hash": content_hash,
            "profile": {
                "name": "contract.example",
                "version": {"major": 1, "minor": 0},
                "metadata": {"priority": "normal"}
            }
        }
    })
}

pub(crate) fn put(record: Value) -> Value {
    json!({"op": "put", "record": record})
}

pub(crate) fn delete(key: &str) -> Value {
    json!({"op": "delete", "key": key})
}
