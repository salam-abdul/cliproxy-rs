use super::*;

fn modes() -> Vec<Options> {
    vec![
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            ..Options::default()
        },
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            preserve_all_additional_properties: true,
            preserve_standard_constraints: true,
            ..Options::default()
        },
        Options {
            add_placeholder: true,
            add_missing_array_items: true,
            antigravity_semantics: true,
            flatten_unions: true,
            drop_all_enums: true,
            ..Options::default()
        },
        Options {
            antigravity_semantics: true,
            flatten_unions: true,
            drop_boolean_enums: true,
            preserve_additional_properties_false: true,
            ..Options::default()
        },
        Options {
            add_missing_array_items: true,
            antigravity_semantics: true,
            remove_tool_title: true,
            flatten_unions: true,
            drop_all_enums: true,
            ..Options::default()
        },
        Options {
            add_placeholder: true,
            add_missing_array_items: true,
            antigravity_semantics: true,
            flatten_unions: true,
            flatten_multi_branch_unions: true,
            drop_all_enums: true,
            ..Options::default()
        },
    ]
}

#[test]
fn schema_batching_matches_ordered_rules_and_raw_formatting() {
    let inputs: &[&[u8]] = &[
        br#" { "type":"object", "properties": { "alpha": {"type":"string","enum":[1,2],"minLength":2}, "beta":{"items":{"const":"x"}} }, "required":["alpha","missing"],"additionalProperties":false,"title":"T" } "#,
        br#"{"properties":{"alpha":{"properties":{"beta":{"type":"boolean","enum":[true,false]}}},"gamma":{"type":"array"}},"description":"kept"}"#,
        br#"{"type":"string","items":{"properties":{"alpha":{"type":"integer"}}},"const":4,"default":2}"#,
        br#"{"properties":{},"x-example":true,"maximum":5}"#,
        br#"{"properties":{"alpha":{"properties":{}}},"required":[]}"#,
    ];
    for input in inputs {
        for options in modes() {
            let fast = clean_indexed(input, options).expect("ordinary schemas use the index");
            assert_eq!(
                fast,
                clean_inner(input, options),
                "input: {}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

#[test]
fn schema_batching_routes_complex_constructs_to_the_arena() {
    for input in [
        br#"{"properties":{"a|b":{"title":"T"}}}"#.as_slice(),
        br#"{"properties":{"properties":{"properties":{}}}}"#,
        br#"{"properties":{"reason":{"description":"Brief explanation of why you are calling this tool"}}}"#,
        br#"{"type":"string","type":"object"}"#,
        br#"{"anyOf":[{"type":"string"},{"type":"null"}]}"#,
        br#"{"properties":{"alpha":{"required":true}}}"#,
        br##"{"$ref":"#/$defs/X","$defs":{"X":{"type":"string"}}}"##,
    ] {
        let options = modes()[0];
        assert!(clean_indexed(input, options).is_none());
        assert!(gj::valid(&clean(input, options)));
    }
}

#[test]
fn schema_batching_keeps_keyword_named_properties() {
    for name in [
        "type",
        "items",
        "enum",
        "title",
        "description",
        "required",
        "default",
        "examples",
        "allOf",
        "then",
        "additionalProperties",
        "not",
        "x-custom",
    ] {
        let input = format!(
            r#"{{"properties":{{"{name}":{{"type":"string","enum":["x","y"],"title":"T","minLength":1}}}},"required":["{name}"]}}"#
        );
        for options in modes() {
            let cleaned = clean(input.as_bytes(), options);
            assert!(
                value(&cleaned)["properties"].get(name).is_some(),
                "literal property {name}"
            );
            if name != "enum" {
                let fast = clean_indexed(input.as_bytes(), options).expect("keyword names with independent rules");
                assert_eq!(fast, clean_inner(input.as_bytes(), options), "property {name}");
            }
        }
    }
}

#[test]
fn schema_batching_after_repair_and_reference_inlining() {
    for input in [
        br##"{"properties":{"alpha":{"$ref":"#/$defs/Entry"}},"$defs":{"Entry":{"properties":{"beta":{"type":"string"}}}}}"##.as_slice(),
        br#"{"alpha":{"type":"string","required":true},"title":"T"}"#,
        br#"{"properties":{"alpha":{"enum":["$ref","other"]}},"title":"T"}"#,
    ] {
        for options in modes() {
            assert_eq!(clean(input, options), clean_inner(input, options));
        }
    }
}

#[test]
fn schema_batching_combines_independent_local_rules() {
    let variants = [
        r#""type":"object","properties":{"alpha":{"enum":[1,2]}},"required":["alpha","missing"]"#,
        r#""items":{"type":"string","minLength":2},"description":"D","title":"T""#,
        r#""properties":{"alpha":{"type":"boolean"}},"enum":["x","y"],"additionalProperties":false"#,
        r#""type":"array","items":{"const":1},"maxItems":4"#,
        r#""properties":{},"const":"x","required":[],"maximum":5"#,
        r#""additionalProperties":{"properties":{"alpha":{"type":"string"}}},"default":4"#,
    ];
    for outer in variants {
        for inner in variants {
            let leaf = format!("{{{inner}}}");
            let input = format!(r#"{{"properties":{{"field":{{{outer}}},"sibling":{leaf}}}}}"#);
            for options in modes() {
                let fast = clean_indexed(input.as_bytes(), options).unwrap();
                assert_eq!(fast, clean_inner(input.as_bytes(), options), "{input}");
            }
        }
    }
}

fn nested(depth: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for _ in 0..depth {
        input.extend_from_slice(br#"{"properties":{"child":"#);
    }
    input.extend_from_slice(br#"{"type":"string","minLength":1}"#);
    for _ in 0..depth {
        input.extend_from_slice(b"}}");
    }
    input
}

#[test]
fn schema_batching_handles_depth_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            let input = nested(2000);
            let output = for_gemini(&input);
            assert!(gj::valid(&output));
            assert_eq!(
                output
                    .windows(b"\"type\":\"object\"".len())
                    .filter(|v| *v == b"\"type\":\"object\"")
                    .count(),
                2000
            );
            assert!(crate::gostr::contains(&output, b"minLength: 1"));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn schema_batching_allocations_scale_with_document_size() {
    let options = modes()[0];
    let allocated = |depth| {
        let input = nested(depth);
        let before = crate::alloc_count::bytes();
        let output = clean_indexed(&input, options).unwrap();
        let bytes = crate::alloc_count::bytes() - before;
        assert!(gj::valid(&output));
        bytes
    };
    let small = allocated(128);
    let large = allocated(512);
    assert!(large < small * 5, "allocation growth: {small} -> {large}");
}

#[test]
fn schema_batching_wide_required_membership_scales() {
    let allocated = |count| {
        let properties: Vec<_> = (0..count)
            .map(|i| format!(r#""field{i}":{{"type":"string","maxLength":8}}"#))
            .collect();
        let required: Vec<_> = (0..count).map(|i| format!(r#""field{i}""#)).collect();
        let input = format!(
            r#"{{"properties":{{{}}},"required":[{},"missing"]}}"#,
            properties.join(","),
            required.join(",")
        );
        let before = crate::alloc_count::bytes();
        let output = clean_indexed(input.as_bytes(), modes()[0]).expect("wide ordinary schemas use the index");
        let bytes = crate::alloc_count::bytes() - before;
        let doc = crate::schema_path::Document::parse(&output).unwrap();
        let required = doc.field(0, b"required").unwrap();
        assert_eq!(doc.nodes[required].children.len(), count);
        bytes
    };
    let small = allocated(128);
    let large = allocated(512);
    assert!(large < small * 5, "allocation growth: {small} -> {large}");
}

fn value(input: &[u8]) -> serde_json::Value {
    serde_json::from_slice(input).unwrap()
}

#[test]
fn schema_batching_complex_provider_semantics() {
    let input = br#"{"properties":{"alpha":{"type":["string","null"]},"beta":{"oneOf":[{"type":"integer"},{"type":"null"}]}},"required":["alpha","beta"],"allOf":[{"properties":{"gamma":{"type":"boolean"}},"required":["gamma"]}]}"#;
    let gemini = value(&for_gemini(input));
    assert_eq!(gemini["properties"]["alpha"]["type"], "string");
    assert_eq!(gemini["properties"]["beta"]["type"], "integer");
    assert_eq!(gemini["properties"]["gamma"]["type"], "boolean");
    assert!(
        !gemini["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("alpha"))
    );
    assert!(
        gemini["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("gamma"))
    );
    assert!(gemini.get("allOf").is_none());
    let native = value(&for_gemini_json_schema(input));
    assert!(native["allOf"].is_array());
    assert!(native["properties"]["alpha"]["type"].is_array());
    assert!(native["properties"]["beta"]["oneOf"].is_array());

    let union = br#"{"anyOf":[{"type":"string"},{"type":"integer"},{"type":"null"}]}"#;
    let preserved = value(&for_antigravity_tool(union, false));
    assert_eq!(preserved["anyOf"].as_array().unwrap().len(), 2);
    assert_eq!(preserved["nullable"], true);
    let projected = value(&for_antigravity_claude_tool(union, false));
    assert_eq!(projected["type"], "string");
    assert!(projected.get("anyOf").is_none());
    assert_eq!(projected["nullable"], true);
}

fn sort_required(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if key == "required"
                    && let Some(values) = value.as_array_mut()
                {
                    values.sort_by_key(|value| value.to_string());
                } else {
                    sort_required(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sort_required(value);
            }
        }
        _ => {}
    }
}

#[test]
fn schema_batching_complex_rules_keep_provider_meaning() {
    for input in [
        br#"{"allOf":[{"type":"object","properties":{"alpha":{"type":"string"}},"required":["alpha"]},{"properties":{"beta":{"type":"number"}},"required":["beta"]}],"title":"T"}"#.as_slice(),
        br#"{"properties":{"alpha":{"type":"array"}},"then":{"properties":{"beta":{"type":"string"}}},"else":{"properties":{"beta":{"type":"integer"},"gamma":{"type":"boolean"}}}}"#,
        br#"{"oneOf":[{"type":"integer"},{"type":"string"}],"anyOf":[{"type":"boolean"},{"type":"null"}],"description":"D"}"#,
        br#"{"anyOf":[true,false,{},1]}"#,
        br#"{"properties":{"alpha":{"type":"string"}},"anyOf":[{"required":["alpha"]},{"type":"null"}]}"#,
        br#"{"not":{"enum":[1,2],"type":"boolean"},"contains":{"type":"array","minItems":1}}"#,
        br#"{"items":[{"type":"string"},{"type":"integer"}],"type":"array"}"#,
        br#"{"allOf":[{"properties":{"alpha":{"type":["string","null"]}}},{"properties":{"alpha":{"description":"D"}}}],"required":["alpha"]}"#,
    ] {
        for options in modes() {
            let mut actual = value(&clean(input, options));
            let mut expected = value(&clean_inner(input, options));
            sort_required(&mut actual);
            sort_required(&mut expected);
            assert_eq!(actual, expected, "{}", String::from_utf8_lossy(input));
        }
    }
}

#[test]
fn schema_batching_required_only_union_branches_and_literals() {
    let input = br#"{"properties":{"alpha":{"type":"string"},"beta":{"type":"integer"}},"anyOf":[{"required":["alpha"]},{"required":["beta"]}],"example":{"type":["string","null"],"title":"literal"},"additionalProperties":false}"#;
    let output = value(&for_gemini(input));
    assert_eq!(output["anyOf"].as_array().unwrap().len(), 2);
    assert_eq!(output["anyOf"][0]["properties"]["alpha"]["type"], "string");
    assert_eq!(output["anyOf"][1]["properties"]["beta"]["type"], "integer");
    assert_eq!(output["example"]["title"], "literal");
    assert!(output["example"]["type"].is_array());
    assert!(output.get("additionalProperties").is_none());
}

#[test]
fn schema_batching_preserves_repeated_union_type_hints() {
    let input = br#"{"properties":{"duo":{"anyOf":[{"type":"string"},{"type":"string"}]}}}"#;
    let output = value(&for_antigravity_claude_tool(input, false));
    assert_eq!(output["properties"]["duo"]["type"], "string");
    assert_eq!(output["properties"]["duo"]["description"], "Accepts: string | string");
    assert!(output["properties"]["duo"].get("anyOf").is_none());
}

#[test]
fn schema_batching_uses_literal_keys_and_last_duplicate_value() {
    let input = br#"{"properties":{"a|b":{"title":"remove","type":"string"},"a.b":{"type":"integer"},"*":{"type":"boolean"},"const":{"const":5}},"required":["a|b","a.b","*","const"],"title":"first","title":"last"}"#;
    let output = value(&for_gemini(input));
    assert_eq!(output["properties"]["a|b"]["type"], "string");
    assert!(output["properties"]["a|b"].get("title").is_none());
    assert_eq!(output["properties"]["a.b"]["type"], "integer");
    assert_eq!(output["properties"]["*"]["type"], "boolean");
    assert_eq!(output["properties"]["const"]["enum"], serde_json::json!(["5"]));
    assert_eq!(output["required"].as_array().unwrap().len(), 4);
    let duplicate = value(&for_antigravity_response(
        br#"{"description":"first","description":"last","anyOf":[{"type":"string"}]}"#,
    ));
    assert_eq!(duplicate["description"], "last");
}

fn accumulating_all_of(depth: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for i in 0..depth {
        input.extend_from_slice(
            format!(r#"{{"properties":{{"field{i}":{{"type":"string"}}}},"required":["field{i}"],"allOf":["#)
                .as_bytes(),
        );
    }
    input.extend_from_slice(b"{}");
    for _ in 0..depth {
        input.extend_from_slice(b"]}");
    }
    input
}

#[test]
fn schema_batching_deep_accumulating_all_of_scales() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            let allocated = |depth| {
                let input = accumulating_all_of(depth);
                let before = crate::alloc_count::bytes();
                let output = for_gemini(&input);
                let bytes = crate::alloc_count::bytes() - before;
                let output = value(&output);
                assert_eq!(output["properties"].as_object().unwrap().len(), depth);
                assert_eq!(output["required"].as_array().unwrap().len(), depth);
                assert!(output.get("allOf").is_none());
                bytes
            };
            let small = allocated(128);
            let large = allocated(512);
            assert!(large < small * 5, "allocation growth: {small} -> {large}");
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn schema_batching_wide_references_and_unions() {
    let properties: Vec<_> = (0..256)
        .map(|i| format!(r##""field{i}":{{"$ref":"#/$defs/Entry"}}"##))
        .collect();
    let input = format!(
        r##"{{"properties":{{{}}},"$defs":{{"Entry":{{"anyOf":[{{"type":"string"}},{{"type":"null"}}]}}}}}}"##,
        properties.join(",")
    );
    let output = value(&for_gemini(input.as_bytes()));
    assert_eq!(output["properties"].as_object().unwrap().len(), 256);
    assert!(output.get("$defs").is_none());
    for child in output["properties"].as_object().unwrap().values() {
        assert_eq!(child["type"], "string");
        assert!(child.get("anyOf").is_none());
    }
}

#[test]
fn schema_batching_wide_nullable_required_edits_are_batched() {
    let allocated = |count| {
        let properties: Vec<_> = (0..count)
            .map(|i| format!(r#""field{i}":{{"type":["string","null"]}}"#))
            .collect();
        let required: Vec<_> = (0..count).map(|i| format!(r#""field{i}""#)).collect();
        let input = format!(
            r#"{{"properties":{{{}}},"required":[{}]}}"#,
            properties.join(","),
            required.join(",")
        );
        let before = crate::alloc_count::bytes();
        let output = for_gemini(input.as_bytes());
        let allocated = crate::alloc_count::bytes() - before;
        assert!(value(&output).get("required").is_none());
        allocated
    };
    let small = allocated(128);
    let large = allocated(512);
    assert!(large < small * 5, "allocation growth: {small} -> {large}");
}

#[test]
fn schema_batching_deep_union_descriptions_scale() {
    std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| {
            let allocated = |depth| {
                let mut input = Vec::new();
                for _ in 0..depth {
                    input.extend_from_slice(br#"{"description":"D","anyOf":["#);
                }
                input.extend_from_slice(br#"{"type":"string"}"#);
                for _ in 0..depth {
                    input.extend_from_slice(br#",{"type":"null"}]}"#);
                }
                let before = crate::alloc_count::bytes();
                let output = for_gemini(&input);
                let bytes = crate::alloc_count::bytes() - before;
                let output = value(&output);
                assert_eq!(output["type"], "string");
                assert!(output["description"].as_str().unwrap().len() >= depth);
                bytes
            };
            let small = allocated(128);
            let large = allocated(512);
            assert!(large < small * 5, "allocation growth: {small} -> {large}");
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn schema_batching_deep_conditional_properties_scale() {
    let allocated = |depth| {
        let mut input = Vec::new();
        for i in 0..depth {
            input.extend_from_slice(format!(r#"{{"properties":{{"field{i}":{{"type":"string"}}}},"then":"#).as_bytes());
        }
        input.extend_from_slice(b"{}");
        input.extend(std::iter::repeat_n(b'}', depth));
        let before = crate::alloc_count::bytes();
        let output = for_gemini(&input);
        let bytes = crate::alloc_count::bytes() - before;
        assert_eq!(value(&output)["properties"].as_object().unwrap().len(), depth);
        bytes
    };
    let small = allocated(128);
    let large = allocated(512);
    assert!(large < small * 5, "allocation growth: {small} -> {large}");
}
