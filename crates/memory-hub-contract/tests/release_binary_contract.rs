//! Contract test that runs against a release artifact binary, not the cargo
//! workspace binary.
//!
//! Set `MEMORY_HUB_RELEASE_BINARY` to the path of a release binary. When
//! unset, the test is skipped (marked ignored) so it does not block normal
//! `cargo test` runs. The release CI workflow sets this environment variable.

#![allow(clippy::unwrap_used)]

use memory_hub_contract::{ReleaseBinaryTarget, run_contract};
use std::env;

#[test]
fn release_artifact_passes_contract() {
    let binary = match env::var("MEMORY_HUB_RELEASE_BINARY") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            eprintln!("skipping: MEMORY_HUB_RELEASE_BINARY not set");
            return;
        }
    };
    let target = ReleaseBinaryTarget::new(&binary);
    let report = run_contract(&target);
    assert!(report.passed, "{report:#?}");
}
