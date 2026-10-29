use serde_json::Value;

#[test]
fn workspace_releases_use_one_pull_request() {
    let config: Value = serde_json::from_str(include_str!("../release-please-config.json"))
        .expect("valid Release Please configuration");

    assert_eq!(config["separate-pull-requests"], false);
    assert!(
        config["plugins"]
            .as_array()
            .expect("plugins array")
            .iter()
            .any(|plugin| plugin["type"] == "cargo-workspace")
    );
}
