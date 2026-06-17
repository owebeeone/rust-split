const PUBLISH_WORKFLOW: &str = include_str!("../.github/workflows/publish.yml");

#[test]
fn publish_workflow_tests_linux_and_windows() {
    assert!(PUBLISH_WORKFLOW.contains("ubuntu-latest"));
    assert!(PUBLISH_WORKFLOW.contains("windows-latest"));
}

#[test]
fn publish_workflow_runs_full_rust_verification() {
    assert!(PUBLISH_WORKFLOW.contains("cargo fmt --check"));
    assert!(PUBLISH_WORKFLOW.contains("cargo test --locked"));
    assert!(PUBLISH_WORKFLOW.contains("cargo clippy --all-targets -- -D warnings"));
}

#[test]
fn publish_workflow_installs_release_taut_proto_for_protocol_tests() {
    assert!(PUBLISH_WORKFLOW.contains("actions/setup-python"));
    assert!(PUBLISH_WORKFLOW.contains("TAUT_PYTHON: python"));
    assert!(PUBLISH_WORKFLOW.contains("python -m pip install --upgrade pip taut-proto"));
}

#[test]
fn publish_workflow_allows_manual_and_tagged_runs() {
    assert!(PUBLISH_WORKFLOW.contains("workflow_dispatch"));
    assert!(PUBLISH_WORKFLOW.contains("tags:"));
    assert!(PUBLISH_WORKFLOW.contains("v*"));
}
