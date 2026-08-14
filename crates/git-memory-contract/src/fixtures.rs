#![allow(clippy::needless_pass_by_value)]

use serde_json::{Value, json};

pub(crate) fn record(key: &str, content: &str) -> Value {
    json!({
        "key": key,
        "kind": "note",
        "content": content,
        "metadata": {
            "client_namespace": "contract.example",
            "priority": "normal"
        }
    })
}

pub(crate) fn put(record: Value) -> Value {
    json!({"op": "put", "record": record})
}
