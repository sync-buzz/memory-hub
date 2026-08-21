#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

use memory_hub_contract::{FakeServerTarget, ReleaseBinaryTarget, run_contract};
use serde_json::Value;

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_memory-hub-contract-fake"))
}

#[test]
fn deterministic_fake_passes_every_shared_scenario() {
    let report = run_contract(&FakeServerTarget::new(fake_binary()));
    assert!(report.passed, "{report:#?}");
    assert_eq!(
        report
            .scenarios
            .iter()
            .map(|scenario| scenario.name)
            .collect::<Vec<_>>(),
        [
            "atomic_batch",
            "snapshot_consistency",
            "different_key_race",
            "same_key_conflict",
            "interrupted_write_recovery",
            "diff_import_export",
            "search_fts_and_filters",
            "search_pagination",
            "backlinks_explicit_and_mentions"
        ]
    );
}

#[test]
fn release_process_adapter_runs_the_identical_scenarios() {
    // The fake accepts the release binary's `mcp --project` invocation shape,
    // proving that target selection does not fork or copy scenario definitions.
    let report = run_contract(&ReleaseBinaryTarget::new(fake_binary()));
    assert!(report.passed, "{report:#?}");
    assert_eq!(report.target, "release_binary");
}

#[test]
fn command_line_runner_emits_a_machine_readable_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_memory-hub-contract"))
        .arg("--fake-binary")
        .arg(fake_binary())
        .args(["--output", "json"])
        .output()
        .expect("contract runner should start");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("report should be JSON");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["target"], "deterministic_fake");
    assert_eq!(report["passed"], true);
    assert_eq!(report["scenarios"].as_array().map(Vec::len), Some(9));
}

#[test]
fn fixtures_are_product_neutral() {
    let source = include_str!("../src/fixtures.rs").to_ascii_lowercase();
    for sync_specific_term in ["goal", "milestone", "roadmap", "session", "sync metadata"] {
        assert!(
            !source.contains(sync_specific_term),
            "fixture source contains product-specific term {sync_specific_term:?}"
        );
    }
}
