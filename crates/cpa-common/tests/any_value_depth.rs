use cpa_common::json::{self, AnyValue};
use std::collections::BTreeMap;

fn small_stack(name: &str, run: fn()) {
    if std::env::var("CPA_ANY_VALUE_DEPTH_CASE").as_deref() == Ok(name) {
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
        .env("CPA_ANY_VALUE_DEPTH_CASE", name)
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
        "{name}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn nested(depth: usize, leaf: &str) -> Vec<u8> {
    format!("{}{leaf}{}", r#"[{"d":"#.repeat(depth / 2), "}]".repeat(depth / 2)).into_bytes()
}

fn tree(depth: usize, mut leaf: AnyValue) -> AnyValue {
    for _ in 0..depth / 2 {
        leaf = AnyValue::Array(vec![AnyValue::Object(BTreeMap::from([(b"d".to_vec(), leaf)]))]);
    }
    leaf
}

#[test]
fn convert_deep_permissive_values() {
    small_stack("convert_deep_permissive_values", || {
        // gjson does not apply encoding/json's 10000-container limit.
        let raw = nested(20_000, "1.50");
        let value = AnyValue::from_res(&json::parse(&raw));
        let mut leaf = &value;
        for _ in 0..10_000 {
            let AnyValue::Array(items) = leaf else { panic!("array") };
            let AnyValue::Object(map) = &items[0] else {
                panic!("object")
            };
            leaf = &map[b"d".as_slice()];
        }
        assert!(matches!(leaf, AnyValue::Number(n) if *n == 1.5));
        assert_eq!(value.marshal().unwrap(), nested(20_000, "1.5"));
        let truncated = &raw[..raw.len() - 20_000];
        assert_eq!(json::parse(truncated).value_json().unwrap(), nested(20_000, "1.5"));
    });
}

#[test]
fn marshal_deep_values_and_reject_nonfinite_numbers() {
    small_stack("marshal_deep_values_and_reject_nonfinite_numbers", || {
        let value = tree(20_000, AnyValue::String(b"<\xff>".to_vec()));
        assert_eq!(value.marshal().unwrap(), nested(20_000, r#""\u003c\ufffd\u003e""#));
        let bad = tree(20_000, AnyValue::Number(f64::INFINITY));
        assert!(bad.marshal().is_none());
        let mut out = b"{}".to_vec();
        assert!(!bad.set(&mut out, "x"));
        assert!(out.is_empty());
    });
}

#[test]
fn drop_deep_values() {
    small_stack("drop_deep_values", || drop(tree(30_000, AnyValue::Null)));
}

#[test]
fn clone_deep_values() {
    small_stack("clone_deep_values", || {
        let value = tree(20_000, AnyValue::String(b"leaf".to_vec()));
        let copy = value.clone();
        drop(value);
        assert_eq!(copy.marshal().unwrap(), nested(20_000, r#""leaf""#));
    });
}

#[test]
fn compare_deep_values_with_float_semantics() {
    small_stack("compare_deep_values_with_float_semantics", || {
        let a = tree(20_000, AnyValue::Number(0.0));
        let b = tree(20_000, AnyValue::Number(-0.0));
        assert!(a == b);
        assert!(a != tree(20_000, AnyValue::Number(1.0)));
        assert!(AnyValue::Number(f64::NAN) != AnyValue::Number(f64::NAN));
        assert!(AnyValue::String(vec![0xff]) != AnyValue::String(vec![0xfe]));
        for (a, b) in [
            (r#"{"a":1}"#, r#"{"b":1}"#),
            ("[]", "[null]"),
            ("true", "false"),
            ("null", "false"),
        ] {
            assert!(AnyValue::from_res(&json::parse(a.as_bytes())) != AnyValue::from_res(&json::parse(b.as_bytes())));
        }
    });
}

#[test]
fn duplicate_keys_keep_first_even_when_later_value_is_deep_or_nonfinite() {
    small_stack(
        "duplicate_keys_keep_first_even_when_later_value_is_deep_or_nonfinite",
        || {
            let raw = format!(
                r#"{{"x":{},"x":1e400}}"#,
                String::from_utf8(nested(10_000, "1")).unwrap()
            );
            let expected = format!(r#"{{"x":{}}}"#, String::from_utf8(nested(10_000, "1")).unwrap());
            assert_eq!(json::parse(raw.as_bytes()).value_json().unwrap(), expected.as_bytes());
            let raw = format!(
                r#"{{"x":false,"x":{}}}"#,
                String::from_utf8(nested(10_000, "1e400")).unwrap()
            );
            assert_eq!(json::parse(raw.as_bytes()).value_json().unwrap(), br#"{"x":false}"#);
        },
    );
}

#[test]
fn preserve_gjson_permissive_and_byte_semantics() {
    for (input, expected) in [
        (br#"{"z":1,"a":2,"a":3}"#.as_slice(), br#"{"a":2,"z":1}"#.as_slice()),
        (br#"[1,,true false null] trailing"#, b"[1,true,false,null]"),
        (br#"{"x" 1 "y":2"#, br#"{"x":1,"y":2}"#),
        (b"[[)1],2]", b"[[],1]"),
        (b"[[(1]2)3]", b"[[1],3]"),
        (b"[1\" x {\"a\":2}]", br#"[0,{"a":2}]"#),
        (
            b"[\"\xff\",\"bad\\qtail\",1e3,-0.0,9007199254740993]",
            br#"["\ufffd","bad",1000,-0,9007199254740992]"#,
        ),
        (br#"{1:2,"":3}"#, br#"{"":""}"#),
        (b"[naN,Infinity,+Inf]", b"[null,null,null]"),
        // Preserve this fork's replacement spelling for malformed strings.
        (b"[wordword\0\"\0\xff[", br#"["\u0000\ufffd["]"#),
    ] {
        assert_eq!(json::parse(input).value_json().as_deref(), Some(expected), "{input:?}");
    }
    for input in [b"NaN".as_slice(), b"[1e400]", b"{\"n\":-1e400}"] {
        assert!(json::parse(input).value_json().is_none());
    }
    let mut out = b"{}".to_vec();
    assert!(AnyValue::Number(f64::INFINITY).set(&mut out, "x"));
    assert_eq!(out, b"{\"x\":+Inf}");
}
