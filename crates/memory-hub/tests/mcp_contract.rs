use memory_hub_contract::{ReleaseBinaryTarget, run_contract};

#[test]
fn release_binary_passes_the_independent_mcp_contract() {
    let target = ReleaseBinaryTarget::new(env!("CARGO_BIN_EXE_memory-hub"));
    let report = run_contract(&target);
    assert!(report.passed, "{report:#?}");
}
