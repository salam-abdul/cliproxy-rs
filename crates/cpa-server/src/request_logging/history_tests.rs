use super::*;

fn settings(yaml: &str) -> Option<Settings> {
    Settings::from_config(&crate::testing::config(yaml))
}

#[test]
fn history_is_off_unless_enabled_with_a_dir() {
    assert!(settings("{}").is_none());
    assert!(settings("observability: {history: {enable: false, dir: /tmp/h}}").is_none());
    assert!(settings("observability: {history: {enable: true, dir: ''}}").is_none());
    assert!(settings("observability: {history: {enable: true}}").is_none());
    assert!(
        settings("server: {commercial-mode: true}\nobservability: {history: {enable: true, dir: /tmp/h}}").is_none()
    );

    let on = settings("observability: {history: {enable: true, dir: /tmp/h}}").unwrap();
    assert_eq!(on.dir, std::path::PathBuf::from("/tmp/h"));
    assert_eq!(on.max_total_bytes, Some(10240 << 20));
    let uncapped = settings("observability: {history: {enable: true, dir: /tmp/h, max-total-size-mb: 0}}").unwrap();
    assert_eq!(uncapped.max_total_bytes, None);
    let small = settings("observability: {history: {enable: true, dir: /tmp/h, max-total-size-mb: 5}}").unwrap();
    assert_eq!(small.max_total_bytes, Some(5 << 20));
}

#[test]
fn history_dir_expands_the_home_directory() {
    let Some(home) = std::env::var_os("HOME") else { return };
    let on = settings("observability: {history: {enable: true, dir: ~/h}}").unwrap();
    assert_eq!(on.dir, std::path::PathBuf::from(home).join("h"));
}
