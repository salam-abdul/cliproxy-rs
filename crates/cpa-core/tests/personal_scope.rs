use cpa_core::config::Config;

#[test]
fn plugin_execution_cannot_be_enabled() {
    for source in [
        "plugins: {enabled: true}",
        "config-version: 8\nplugins: {enabled: true, dir: /tmp/synthetic-plugins}",
    ] {
        let error = Config::parse(source).expect_err("plugin execution must be rejected");
        assert!(error.to_string().contains("plugins are not supported"));
    }
    assert!(Config::parse("plugins: {enabled: false}").is_ok());
}
