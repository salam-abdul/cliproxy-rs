use cpa_common::{gemini_schema::inline_local_refs, json::GoValue};

#[test]
fn reference_sibling_assertions_remain_conjunctive() {
    let got = inline_local_refs(
        br##"{"$defs":{"s":{"type":"string"}},"properties":{"x":{"$ref":"#/$defs/s","type":"number"}}}"##,
    );
    assert_eq!(
        GoValue::parse(&got),
        GoValue::parse(
            br##"{"$defs":{"s":{"type":"string"}},"properties":{"x":{"allOf":[{"type":"string"}],"type":"number"}}}"##
        )
    );
}

#[test]
fn references_inside_literal_values_are_not_schemas() {
    let input = br##"{"$defs":{"s":{"type":"string"}},"properties":{"default":{"$ref":"#/$defs/s"}},"default":{"$ref":"#/$defs/s"}}"##;
    let got = inline_local_refs(input);
    assert_eq!(GoValue::parse(&got), GoValue::parse(br##"{"$defs":{"s":{"type":"string"}},"properties":{"default":{"type":"string"}},"default":{"$ref":"#/$defs/s"}}"##));
}

#[test]
fn oversized_reference_degrades_to_bounded_hint() {
    let input = format!(
        r##"{{"$defs":{{"s":{{"type":"string","description":"{}"}}}},"properties":{{"x":{{"$ref":"#/$defs/s"}}}}}}"##,
        "a".repeat(300_000)
    );
    let got = inline_local_refs(input.as_bytes());
    let x = cpa_common::json::get(&got, "properties.x");
    assert_eq!(x.get("description").str().as_ref(), "See: s");
    assert_eq!(x.get("type").str().as_ref(), "string");
}

#[test]
fn schema_consumers_handle_depth_on_small_stacks() {
    const FLAG: &str = "CPA_SCHEMA_SMALL_STACK";
    if std::env::var_os(FLAG).is_some() {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let arrays = format!("{}{{\"$ref\":\"#/missing\"}}{}", "[".repeat(1000), "]".repeat(1000));
                assert!(!inline_local_refs(arrays.as_bytes()).is_empty());
                assert!(!cpa_common::gemini_schema::for_gemini(arrays.as_bytes()).is_empty());
                let depth = std::env::var("CPA_SCHEMA_DEPTH")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(128);
                let objects = format!(
                    "{}{{\"required\":true}}{}",
                    "{\"properties\":{\"p\":".repeat(depth),
                    "}}".repeat(depth)
                );
                assert!(!cpa_common::gemini_schema::for_gemini(objects.as_bytes()).is_empty());
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "schema_consumers_handle_depth_on_small_stacks",
            "--nocapture",
        ])
        .env(FLAG, "1")
        .status()
        .unwrap();
    assert!(status.success(), "schema subprocess failed: {status}");
}
