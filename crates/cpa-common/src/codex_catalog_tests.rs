//! Client version feature gates.
use super::*;

#[test]
fn client_versions_gate_extended_levels() {
    assert!(extended_levels(""));
    assert!(extended_levels("garbage"));
    assert!(extended_levels("0.144.0"));
    assert!(extended_levels("V0.144.0-rc1"));
    assert!(!extended_levels("0.143.99"));
    assert!(!extended_levels("0.143"));
    assert!(extended_levels("0.144"));
    // Go cuts at the first `-` or `+` before parsing, so only a bad part fails.
    assert_eq!(compare_versions("1.-2", "1"), Some(0));
    assert_eq!(compare_versions("1.x", "1"), None);
}
