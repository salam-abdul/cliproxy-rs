use cpa_common::json::{self as gj, Kind};
use std::collections::BTreeMap;

// Claude tool schema (util/claude_schema.go)

const EMPTY_SCHEMA: &[u8] = br#"{"type":"object","properties":{}}"#;

/// `json.Unmarshal(raw, &map[string]json.RawMessage)`; `None` for errors and `null`.
fn raw_object(raw: &[u8]) -> Option<BTreeMap<String, Vec<u8>>> {
    if !gj::valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    if !root.is_object() {
        return None;
    }
    let mut map = BTreeMap::new();
    root.each(|k, v| {
        map.insert(gj::go_unquote(&k.raw).unwrap_or_default(), v.raw.to_vec());
        true
    });
    Some(map)
}

/// `json.Unmarshal(raw, &[]string)`.
fn raw_strings(raw: &[u8]) -> Option<Vec<String>> {
    if !gj::valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    match root.kind {
        Kind::Null => Some(vec![]),
        Kind::Json if root.is_array() => root
            .array()
            .iter()
            .map(|v| match v.kind {
                Kind::String => gj::go_unquote(&v.raw),
                Kind::Null => Some(String::new()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn marshal_raw_map(map: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = vec![b'{'];
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gj::marshal_str(&mut out, k.as_bytes(), true);
        out.push(b':');
        out.extend(gj::compact(v, true));
    }
    out.push(b'}');
    out
}

/// util.NormalizeClaudeToolInputSchema: always an object schema, with union branch
/// properties (and allOf requirements) folded into the root. Go re-marshals through maps,
/// so keys come out sorted and values compacted and HTML-escaped.
pub fn normalize_claude_tool_input_schema(schema: Option<&[u8]>) -> Vec<u8> {
    let Some(mut root) = schema.filter(|s| !s.is_empty()).and_then(raw_object) else {
        return EMPTY_SCHEMA.to_vec();
    };
    let object = |raw: Option<&Vec<u8>>| raw.and_then(|r| raw_object(r)).unwrap_or_default();
    let mut properties = object(root.get("properties"));
    for union in ["anyOf", "oneOf", "allOf"] {
        let Some(raw) = root.remove(union) else {
            continue;
        };
        let branches = if gj::valid(&raw) && gj::parse(&raw).is_array() {
            gj::parse(&raw).array().iter().map(|b| b.raw.to_vec()).collect()
        } else if gj::valid(&raw) && gj::parse(&raw).kind == Kind::Null {
            vec![]
        } else {
            continue;
        };
        for branch in branches {
            let Some(branch) = raw_object(&branch) else {
                continue;
            };
            if !schema_can_be_object(&branch) {
                continue;
            }
            for (name, property) in object(branch.get("properties")) {
                properties.entry(name).or_insert(property);
            }
            if union == "allOf" {
                merge_required(&mut root, branch.get("required"));
            }
        }
    }
    root.insert("type".into(), br#""object""#.to_vec());
    root.insert("properties".into(), marshal_raw_map(&properties));
    marshal_raw_map(&root)
}

fn schema_can_be_object(schema: &BTreeMap<String, Vec<u8>>) -> bool {
    let Some(raw) = schema.get("type") else {
        return true;
    };
    if gj::valid(raw) {
        let value = gj::parse(raw);
        match value.kind {
            Kind::String => return &*value.s == b"object",
            Kind::Null => return false,
            _ => {}
        }
    }
    raw_strings(raw).is_some_and(|types| types.iter().any(|t| t == "object"))
}

fn merge_required(root: &mut BTreeMap<String, Vec<u8>>, branch: Option<&Vec<u8>>) {
    let mut required = root.get("required").and_then(|r| raw_strings(r)).unwrap_or_default();
    let Some(names) = branch.and_then(|b| raw_strings(b)) else {
        return;
    };
    for name in names {
        if !required.contains(&name) {
            required.push(name);
        }
    }
    if !required.is_empty() {
        root.insert("required".into(), gj::quote_all(&required));
    }
}
