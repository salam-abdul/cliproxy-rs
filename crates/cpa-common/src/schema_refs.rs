use crate::json::{self as gj, GoValue};
use std::collections::{BTreeMap, HashMap};
fn ref_name(reference: &[u8]) -> Vec<u8> {
    match reference.iter().rposition(|&c| c == b'/') {
        Some(i) if i + 1 < reference.len() => pointer_unescape(&reference[i + 1..]),
        _ => reference.to_vec(),
    }
}

/// `~1` -> `/`, then `~0` -> `~` (two strings.ReplaceAll passes).
fn pointer_unescape(part: &[u8]) -> Vec<u8> {
    let replace = |s: &[u8], from: &[u8], to: u8| {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(from) {
                out.push(to);
                i += from.len();
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        out
    };
    replace(&replace(part, b"~1", b'/'), b"~0", b'~')
}

// ---------------------------------------------------------------------------------------
// Phase 0: malformed object repair (Go's `any` model)

type Map = BTreeMap<String, GoValue>;

/// json.Decoder.Decode: only the first value is decoded; the decoder reports bytes after
/// it on the next call, so they never fail this one.
fn merge_hint(existing: &[u8], hint: &[u8]) -> Vec<u8> {
    if existing.is_empty() {
        return hint.to_vec();
    }
    if existing == hint
        || existing.starts_with(&[hint, b" ("].concat())
        || contains_seq(existing, &[&b"("[..], hint, b")"].concat())
    {
        return existing.to_vec();
    }
    [existing, b" (", hint, b")"].concat()
}

fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// mergeHint: an existing copy of the hint is kept as is.
fn decode_first(text: &[u8]) -> Option<GoValue> {
    let start = text.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))?;
    let rest = &text[start..];
    let end = match rest[0] {
        b't' | b'n' => 4,
        b'f' => 5,
        _ => gj::parse(rest).raw.len(),
    };
    GoValue::parse(rest.get(..end)?)
}

/// maxInlinedSchemaBytes: shared definitions fan out once per reference, so expansions past
/// this budget degrade to a typed hint like cycles do.
const MAX_INLINED_SCHEMA_BYTES: usize = 256 * 1024;
/// maxFallbackDescriptionBytes: the longest target description a degraded reference copies.
const MAX_FALLBACK_DESCRIPTION_BYTES: usize = 512;

type Active = std::collections::HashSet<String>;

/// The inlining budget of one call, and each reference's target size. Targets never
/// change within a call, so a reference used many times is sized once (Go sizes it on
/// every use, which is 14 s for 8,000 references to one 300 KB definition).
struct Budget {
    left: usize,
    sizes: HashMap<String, usize>,
}

impl Budget {
    fn new() -> Self {
        Self {
            left: MAX_INLINED_SCHEMA_BYTES,
            sizes: HashMap::new(),
        }
    }

    /// targetExpansionSize of `reference`'s `target`, memoized.
    fn target_size(&mut self, reference: &str, target: &GoValue, for_clean: bool) -> usize {
        if let Some(size) = self.sizes.get(reference) {
            return *size;
        }
        let size = target_expansion_size(target, for_clean);
        self.sizes.insert(reference.to_owned(), size);
        size
    }
}

fn is_name_map_keyword(key: &str) -> bool {
    matches!(
        key,
        "properties" | "patternProperties" | "dependentSchemas" | "dependencies" | "$defs" | "definitions"
    )
}

fn is_definitions(key: &str) -> bool {
    key == "$defs" || key == "definitions"
}

/// inlineLocalRefs: every reference gets its own copy while the budget lasts, sibling
/// keywords override the target, and cycles end as a typed hint.
pub(super) fn inline_refs(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut budget = Budget::new();
    resolve_schema_root(&root, &mut budget, false).marshal()
}

/// inlineLocalRefsForClean: inside the single-key `{"schema": ...}` carrier, references
/// resolve against the inner schema.
pub(super) fn inline_refs_for_clean(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut budget = Budget::new();
    if let GoValue::Object(map) = &root
        && map.len() == 1
        && let Some(inner @ GoValue::Object(_)) = map.get("schema")
    {
        let resolved = resolve_schema_root(inner, &mut budget, true);
        let mut out = Map::new();
        out.insert("schema".into(), resolved);
        return GoValue::Object(out).marshal();
    }
    resolve_schema_root(&root, &mut budget, true).marshal()
}

/// resolveSchemaRoot: the schema body resolves before the definition containers, which
/// cleaning drops altogether, so the budget is spent on what is kept.
fn resolve_schema_root(root: &GoValue, budget: &mut Budget, for_clean: bool) -> GoValue {
    let GoValue::Object(root_map) = root else {
        return resolve_local_refs(root, root, &mut Active::new(), budget, for_clean);
    };
    let mut out = Map::new();
    for (key, item) in root_map {
        match key.as_str() {
            "$defs" | "definitions" => {
                if !for_clean {
                    out.insert(key.clone(), GoValue::Null); // resolved after the body
                }
            }
            "$ref" => {
                out.insert(key.clone(), item.clone());
            }
            _ => {
                let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget, for_clean);
                out.insert(key.clone(), resolved);
            }
        }
    }
    if let Some(GoValue::String(reference)) = root_map.get("$ref")
        && reference.starts_with("#/")
        && let Some(target) = resolve_pointer(root, reference)
    {
        let size = budget.target_size(reference, target, for_clean);
        if size <= budget.left {
            budget.left -= size;
            let mut active = Active::from([reference.clone()]);
            match resolve_local_refs(root, target, &mut active, budget, for_clean).into_object() {
                Ok(merged) => {
                    out.remove("$ref");
                    out.remove("$defs");
                    out.remove("definitions");
                    out.remove("$schema");
                    out = merge_inlined_ref_schemas(merged, out);
                    if let Some(dialect) = root_map.get("$schema") {
                        out.insert("$schema".into(), dialect.clone());
                    }
                }
                Err(_) => {
                    out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget, for_clean)
                }
            }
        } else {
            out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget, for_clean);
        }
        out.remove("$ref");
    }
    if !for_clean {
        for key in ["$defs", "definitions"] {
            if let Some(item) = root_map.get(key) {
                let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget, false);
                out.insert(key.into(), resolved);
            }
        }
    }
    GoValue::Object(out)
}

/// targetExpansionSize: when cleaning, definitions inside the target are deleted later, so
/// they cost nothing.
fn target_expansion_size(value: &GoValue, for_clean: bool) -> usize {
    if for_clean {
        clean_expansion_size(value, false)
    } else {
        json_value_size(value)
    }
}

fn clean_expansion_size(value: &GoValue, in_name_map: bool) -> usize {
    value_size(value, true, in_name_map)
}
fn json_value_size(value: &GoValue) -> usize {
    value_size(value, false, false)
}
fn value_size(value: &GoValue, for_clean: bool, names: bool) -> usize {
    let mut size = 0usize;
    let mut work = vec![(value, for_clean, names)];
    while let Some((value, clean, names)) = work.pop() {
        match value {
            GoValue::Null => size += 4,
            GoValue::Bool(_) => size += 5,
            GoValue::Number(n) => size += n.len(),
            GoValue::String(s) => size += json_string_encoded_size(s),
            GoValue::Array(items) => {
                size += 2 + items.len();
                work.extend(items.iter().map(|v| (v, clean, false)));
            }
            GoValue::Object(map) => {
                size += 2;
                for (key, item) in map {
                    if clean && !names && is_definitions(key) {
                        continue;
                    }
                    size += json_string_encoded_size(key) + 2;
                    work.push((
                        item,
                        clean && (names || !is_literal_value_keyword(key.as_bytes())),
                        !names && is_name_map_keyword(key),
                    ));
                }
            }
        }
        if size > MAX_INLINED_SCHEMA_BYTES {
            return MAX_INLINED_SCHEMA_BYTES + 1;
        }
    }
    size
}

/// jsonStringEncodedSize: with encoding/json's HTML escaping.
fn json_string_encoded_size(s: &str) -> usize {
    let b = s.as_bytes();
    let mut size = 2;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        size += match c {
            b'\n' | b'\r' | b'\t' => 2,
            0..0x20 => 6,
            b'"' | b'\\' => 2,
            b'<' | b'>' | b'&' => 6,
            0xe2 if i + 2 < b.len() && b[i + 1] == 0x80 && matches!(b[i + 2], 0xa8 | 0xa9) => {
                i += 2;
                6
            }
            _ => 1,
        };
        i += 1;
    }
    size
}

fn resolve_local_refs(
    root: &GoValue,
    value: &GoValue,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> GoValue {
    enum Work<'a> {
        Visit(&'a GoValue, bool),
        Entry(&'a str, &'a GoValue),
        Object(Vec<String>, Map),
        Array(usize),
        Target(&'a Map, &'a str),
        Merge(Map),
    }
    let mut work = vec![Work::Visit(value, false)];
    let mut results: Vec<GoValue> = Vec::new();
    while let Some(task) = work.pop() {
        match task {
            Work::Entry(key, item) => {
                if is_literal_value_keyword(key.as_bytes()) {
                    results.push(item.clone());
                } else {
                    work.push(Work::Visit(item, is_name_map_keyword(key)));
                }
            }
            Work::Visit(value, names) => match value {
                GoValue::Array(items) => {
                    work.push(Work::Array(items.len()));
                    for item in items.iter().rev() {
                        work.push(Work::Visit(item, false));
                    }
                }
                GoValue::Object(node) => {
                    if !names
                        && let Some(GoValue::String(reference)) = node.get("$ref")
                        && reference.starts_with("#/")
                        && let Some(target) = resolve_pointer(root, reference)
                    {
                        let size = budget.target_size(reference, target, for_clean);
                        if !active.contains(reference) && size <= budget.left {
                            budget.left -= size;
                            active.insert(reference.clone());
                            work.push(Work::Target(node, reference));
                            work.push(Work::Visit(target, false));
                            continue;
                        }
                        let mut fallback = cyclic_ref_fallback(node, target, reference);
                        if let GoValue::Object(target_map) = target
                            && let Some(GoValue::String(d)) = target_map.get("description")
                            && !d.is_empty()
                            && d.len() <= MAX_FALLBACK_DESCRIPTION_BYTES
                            && !node.contains_key("description")
                        {
                            let cost = json_string_encoded_size(d);
                            if cost > budget.left {
                                fallback.insert(
                                    "description".into(),
                                    GoValue::String(format!(
                                        "See: {}",
                                        String::from_utf8_lossy(&ref_name(reference.as_bytes()))
                                    )),
                                );
                            } else {
                                budget.left -= cost;
                            }
                        }
                        let entries: Vec<_> = node
                            .iter()
                            .filter(|(k, _)| {
                                k.as_str() != "$ref" && k.as_str() != "description" && !(for_clean && is_definitions(k))
                            })
                            .collect();
                        if for_clean {
                            fallback.remove("$defs");
                            fallback.remove("definitions");
                        }
                        work.push(Work::Object(
                            entries.iter().map(|(k, _)| (*k).clone()).collect(),
                            fallback,
                        ));
                        for (key, item) in entries.into_iter().rev() {
                            work.push(Work::Entry(key, item));
                        }
                        continue;
                    }
                    let entries: Vec<_> = node
                        .iter()
                        .filter(|(k, _)| names || !(for_clean && is_definitions(k)))
                        .collect();
                    work.push(Work::Object(
                        entries.iter().map(|(k, _)| (*k).clone()).collect(),
                        Map::new(),
                    ));
                    for (key, item) in entries.into_iter().rev() {
                        work.push(if names {
                            Work::Visit(item, false)
                        } else {
                            Work::Entry(key, item)
                        });
                    }
                }
                other => results.push(other.clone()),
            },
            Work::Object(keys, mut map) => {
                let values = results.split_off(results.len() - keys.len());
                map.extend(keys.into_iter().zip(values));
                results.push(GoValue::Object(map));
            }
            Work::Array(n) => {
                let values = results.split_off(results.len() - n);
                results.push(GoValue::Array(values));
            }
            Work::Target(node, reference) => {
                active.remove(reference);
                let target = results.pop().expect("resolved reference target");
                if let Ok(map) = target.into_object() {
                    let entries: Vec<_> = node
                        .iter()
                        .filter(|(k, _)| k.as_str() != "$ref" && !(for_clean && is_definitions(k)))
                        .collect();
                    work.push(Work::Merge(map));
                    work.push(Work::Object(
                        entries.iter().map(|(k, _)| (*k).clone()).collect(),
                        Map::new(),
                    ));
                    for (key, item) in entries.into_iter().rev() {
                        work.push(Work::Entry(key, item));
                    }
                } else {
                    let entries: Vec<_> = node.iter().filter(|(k, _)| !(for_clean && is_definitions(k))).collect();
                    work.push(Work::Object(
                        entries.iter().map(|(k, _)| (*k).clone()).collect(),
                        Map::new(),
                    ));
                    for (key, item) in entries.into_iter().rev() {
                        work.push(Work::Entry(key, item));
                    }
                }
            }
            Work::Merge(target) => {
                let siblings = results
                    .pop()
                    .expect("resolved reference siblings")
                    .into_object()
                    .expect("schema object");
                results.push(GoValue::Object(merge_inlined_ref_schemas(target, siblings)));
            }
        }
    }
    results.pop().expect("resolved schema")
}

fn merge_inlined_ref_schemas(mut target: Map, mut siblings: Map) -> Map {
    let present = |m: &Map, k: &str| m.get(k).is_some_and(|v| !matches!(v, GoValue::Null));
    let mut conjoin =
        !siblings.is_empty() && (present(&target, "unevaluatedProperties") || present(&target, "unevaluatedItems"));
    for (a, b) in [
        ("additionalProperties", "properties"),
        ("additionalProperties", "patternProperties"),
        ("additionalProperties", "additionalProperties"),
        ("items", "prefixItems"),
        ("contains", "contains"),
        ("contains", "minContains"),
        ("contains", "maxContains"),
    ] {
        conjoin |= (present(&target, a) && present(&siblings, b)) || (present(&siblings, a) && present(&target, b));
    }
    for (key, value) in &siblings {
        if !matches!(
            key.as_str(),
            "title"
                | "description"
                | "default"
                | "example"
                | "examples"
                | "$comment"
                | "deprecated"
                | "readOnly"
                | "writeOnly"
        ) && target.get(key).is_some_and(|old| old != value)
        {
            conjoin = true;
        }
    }
    if conjoin {
        let mut branches = vec![GoValue::Object(target)];
        if let Some(old) = siblings.remove("allOf")
            && let Ok(items) = old.into_array()
        {
            branches.extend(items);
        }
        siblings.insert("allOf".into(), GoValue::Array(branches));
        siblings
    } else {
        target.extend(siblings);
        target
    }
}

/// resolveRefFallback: a reference that cannot expand (cycle or spent budget) becomes a
/// typed hint, and its sibling subschemas still resolve.
fn resolve_ref_fallback(
    root: &GoValue,
    node: &Map,
    target: &GoValue,
    reference: &str,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> Map {
    let mut out = cyclic_ref_fallback(node, target, reference);
    if let GoValue::Object(target_map) = target
        && let Some(GoValue::String(description)) = target_map.get("description")
        && !description.is_empty()
        && description.len() <= MAX_FALLBACK_DESCRIPTION_BYTES
        && !node.contains_key("description")
    {
        let cost = json_string_encoded_size(description);
        if cost > budget.left {
            let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
            out.insert(
                "description".into(),
                GoValue::String(String::from_utf8_lossy(&hint).into_owned()),
            );
        } else {
            budget.left -= cost;
        }
    }
    for (key, item) in node {
        if key == "$ref" || key == "description" {
            continue;
        }
        if for_clean && is_definitions(key) {
            out.remove(key);
            continue;
        }
        let resolved = resolve_schema_entry(root, key, item, active, budget, for_clean);
        out.insert(key.clone(), resolved);
    }
    out
}

/// resolveSchemaEntry: literal values are copied verbatim; name-map entries are subschemas.
fn resolve_schema_entry(
    root: &GoValue,
    key: &str,
    item: &GoValue,
    active: &mut Active,
    budget: &mut Budget,
    for_clean: bool,
) -> GoValue {
    if is_literal_value_keyword(key.as_bytes()) {
        return item.clone();
    }
    if is_name_map_keyword(key)
        && let GoValue::Object(names) = item
    {
        return GoValue::Object(
            names
                .iter()
                .map(|(name, v)| (name.clone(), resolve_local_refs(root, v, active, budget, for_clean)))
                .collect(),
        );
    }
    resolve_local_refs(root, item, active, budget, for_clean)
}

fn resolve_pointer<'a>(root: &'a GoValue, reference: &str) -> Option<&'a GoValue> {
    let mut current = root;
    for raw in reference["#/".len()..].split('/') {
        let part = String::from_utf8(pointer_unescape(raw.as_bytes())).unwrap_or_default();
        current = match current {
            GoValue::Object(map) => map.get(&part)?,
            GoValue::Array(items) => {
                let index: i64 = part.parse().ok()?;
                items.get(usize::try_from(index).ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn cyclic_ref_fallback(node: &Map, target: &GoValue, reference: &str) -> Map {
    let mut out = Map::new();
    if let GoValue::Object(target) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = target.get(key) {
                if key == "description" && matches!(v, GoValue::String(d) if d.len() > MAX_FALLBACK_DESCRIPTION_BYTES) {
                    continue;
                }
                out.insert(key.into(), v.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
    let description = match out.get("description") {
        Some(GoValue::String(d)) if !d.is_empty() => merge_hint(d.as_bytes(), &hint),
        _ => hint,
    };
    out.insert(
        "description".into(),
        GoValue::String(String::from_utf8_lossy(&description).into_owned()),
    );
    out
}

fn is_literal_value_keyword(k: &[u8]) -> bool {
    matches!(k, b"default" | b"example" | b"examples" | b"const" | b"enum")
}
