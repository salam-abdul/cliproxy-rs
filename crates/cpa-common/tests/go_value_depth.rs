use cpa_common::json::{self, GoValue};
use std::collections::BTreeMap;

// A stack overflow aborts the process, so isolate each regression from the test runner.
fn small_stack(name: &str, run: fn()) {
    if std::env::var("CPA_GO_VALUE_DEPTH_CASE").as_deref() == Ok(name) {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("CPA_GO_VALUE_DEPTH_CASE", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{name}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn nested(depth: usize, leaf: &str) -> Vec<u8> {
    assert_eq!(depth % 2, 0);
    format!("{}{leaf}{}", r#"[{"d":"#.repeat(depth / 2), "}]".repeat(depth / 2)).into_bytes()
}

fn tree(depth: usize) -> GoValue {
    let mut value = GoValue::String("<leaf>".into());
    for _ in 0..depth / 2 {
        value = GoValue::Array(vec![GoValue::Object(BTreeMap::from([("d".into(), value)]))]);
    }
    value
}

#[test]
fn decode_at_go_depth_limit() {
    small_stack("decode_at_go_depth_limit", || {
        for decode in [GoValue::parse, GoValue::parse_f64] {
            let value = decode(&nested(10_000, "1.5")).expect("Go accepts depth 10000");
            let mut current = &value;
            for _ in 0..5_000 {
                let GoValue::Array(items) = current else {
                    panic!("expected array")
                };
                let GoValue::Object(map) = &items[0] else {
                    panic!("expected object")
                };
                current = &map["d"];
            }
            assert!(matches!(current, GoValue::Number(n) if n == "1.5"));
        }
    });
}

#[test]
fn reject_documents_beyond_go_depth_limit() {
    small_stack("reject_documents_beyond_go_depth_limit", || {
        let too_deep = [b"[".as_slice(), nested(10_000, "0").as_slice(), b"]"].concat();
        assert!(json::valid(&too_deep), "gjson has no depth limit");
        assert!(GoValue::parse(&too_deep).is_none());
        assert!(GoValue::parse_f64(&too_deep).is_none());
    });
}

#[test]
fn marshal_at_go_depth_limit() {
    small_stack("marshal_at_go_depth_limit", || {
        let value = tree(10_000);
        assert_eq!(value.marshal(), nested(10_000, r#""\u003cleaf\u003e""#));
        assert_eq!(value.marshal_no_html(), nested(10_000, r#""<leaf>""#));
    });
}

#[test]
fn drop_deep_constructed_values() {
    small_stack("drop_deep_constructed_values", || drop(tree(30_000)));
}

#[test]
fn clone_deep_values_without_changing_number_literals() {
    small_stack("clone_deep_values_without_changing_number_literals", || {
        let mut value = GoValue::Number("1.500e+3".into());
        for _ in 0..5_000 {
            value = GoValue::Array(vec![GoValue::Object(BTreeMap::from([("d".into(), value)]))]);
        }
        let copy = value.clone();
        drop(value);
        assert_eq!(copy.marshal(), nested(10_000, "1.500e+3"));
    });
}

#[test]
fn compare_deep_values_with_literal_number_semantics() {
    small_stack("compare_deep_values_with_literal_number_semantics", || {
        let a = GoValue::parse(&nested(10_000, "1")).unwrap();
        let mut b = a.clone();
        assert!(a == b);
        b.visit_mut(|v| {
            if matches!(v, GoValue::Number(_)) {
                *v = GoValue::Number("1.0".into());
            }
        });
        assert!(a != b, "number literals stay distinct");
    });
    for (a, b) in [
        (r#"{"a":1}"#, r#"{"b":1}"#),
        ("[1]", "[]"),
        ("{}", r#"{"a":null}"#),
        (r#""1""#, "1"),
        ("true", "false"),
    ] {
        assert!(GoValue::parse(a.as_bytes()) != GoValue::parse(b.as_bytes()));
    }
}

#[test]
fn decode_cleans_up_deep_values_on_overflow_and_duplicate_keys() {
    small_stack("decode_cleans_up_deep_values_on_overflow_and_duplicate_keys", || {
        let deep = String::from_utf8(nested(9_998, "0")).unwrap();
        let overflow = format!(r#"{{"a":{deep},"b":1e400}}"#);
        assert!(GoValue::parse_f64(overflow.as_bytes()).is_none());
        let duplicate = format!(r#"{{"a":{deep},"a":false}}"#);
        assert_eq!(
            GoValue::parse(duplicate.as_bytes()).unwrap().marshal(),
            br#"{"a":false}"#
        );
    });
}

#[test]
fn indented_marshal_keeps_spacing_and_empty_containers() {
    let value = GoValue::parse(br#"{"z":{"d":[false,null,{}]},"a":[],"s":"<&>"}"#).unwrap();
    assert_eq!(
        value.encode_indented(),
        b"{\n  \"a\": [],\n  \"s\": \"\\u003c\\u0026\\u003e\",\n  \"z\": {\n    \"d\": [\n      false,\n      null,\n      {}\n    ]\n  }\n}\n"
    );
    small_stack("indented_marshal_keeps_spacing_and_empty_containers", || {
        let value = tree(1_000);
        let pretty = value.encode_indented();
        assert_eq!(
            json::canonical(&pretty).unwrap(),
            nested(1_000, r#""\u003cleaf\u003e""#)
        );
        assert_eq!(pretty.last(), Some(&b'\n'));
    });
}

#[test]
fn decode_keeps_number_and_string_wire_semantics() {
    let raw = b" {\"a\\\"b\" : [ {},[], -0.0,1e3,9007199254740993,1e-7,1e21 ],\"x\":0,\"x\":\"\xff\\ud800\" } ";
    assert_eq!(
        GoValue::parse(raw).unwrap().marshal(),
        "{\"a\\\"b\":[{},[],-0.0,1e3,9007199254740993,1e-7,1e21],\"x\":\"��\"}".as_bytes()
    );
    assert_eq!(
        GoValue::parse_f64(raw).unwrap().marshal(),
        "{\"a\\\"b\":[{},[],-0,1000,9007199254740992,1e-7,1e+21],\"x\":\"��\"}".as_bytes()
    );
    for invalid in [b"[1,]".as_slice(), b"{}true", b"{\"a\":}", b"[\"\\u00\"]"] {
        assert!(GoValue::parse(invalid).is_none());
        assert!(GoValue::parse_f64(invalid).is_none());
    }
}
