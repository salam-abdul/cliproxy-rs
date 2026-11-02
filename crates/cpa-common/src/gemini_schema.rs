//! JSON Schema cleaning for Gemini and Antigravity tool and response schemas
//! (internal/util/gemini_schema.go).
//!
//! Ordinary schemas preserve raw formatting through indexed shallow edits. Complex
//! schemas use literal keys and indexed semantic passes. Pass one schema, never a request.

use crate::json::{self as gj, AnyValue, GoValue, Kind, Res};
use crate::schema_path::{
    delete as schema_delete, get as schema_get, set_bool as schema_set_bool, set_raw as schema_set_raw,
    set_str as schema_set_str, set_strs as schema_set_strs,
};
use crate::schema_support::{decode_first, is_literal_value_keyword, is_name_map_keyword, merge_hint, ref_name};
use std::collections::{BTreeMap, HashMap};

#[path = "schema_indexed.rs"]
mod indexed;

#[cfg(test)]
#[path = "schema_batching_tests.rs"]
mod schema_batching_tests;

const PLACEHOLDER_REASON_DESCRIPTION: &[u8] = b"Brief explanation of why you are calling this tool";

#[derive(Clone, Copy, Default)]
struct Options {
    add_placeholder: bool,
    add_missing_array_items: bool,
    antigravity_semantics: bool,
    remove_tool_title: bool,
    remove_gemini_metadata: bool,
    flatten_unions: bool,
    /// Flatten multi-branch anyOf/oneOf unions to one branch instead of preserving them.
    flatten_multi_branch_unions: bool,
    force_enum_string_type: bool,
    drop_all_enums: bool,
    drop_boolean_enums: bool,
    preserve_additional_properties_false: bool,
    preserve_all_additional_properties: bool,
    preserve_standard_constraints: bool,
}

/// CleanJSONSchemaForAntigravity.
pub fn for_antigravity(schema: &[u8]) -> Vec<u8> {
    for_antigravity_tool(schema, true)
}

/// CleanJSONSchemaForAntigravityTool.
pub fn for_antigravity_tool(schema: &[u8], require_placeholder: bool) -> Vec<u8> {
    antigravity_tool(schema, require_placeholder, false)
}

/// CleanJSONSchemaForAntigravityClaudeTool: the Antigravity tool cleaner for Claude models,
/// which flattens multi-branch unions because that backend rejects them.
pub fn for_antigravity_claude_tool(schema: &[u8], require_placeholder: bool) -> Vec<u8> {
    antigravity_tool(schema, require_placeholder, true)
}

fn antigravity_tool(schema: &[u8], require_placeholder: bool, flatten_multi_branch_unions: bool) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_placeholder: require_placeholder,
            add_missing_array_items: true,
            antigravity_semantics: true,
            remove_tool_title: !require_placeholder,
            flatten_unions: true,
            flatten_multi_branch_unions,
            drop_all_enums: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForAntigravityResponse.
pub fn for_antigravity_response(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            antigravity_semantics: true,
            flatten_unions: true,
            drop_boolean_enums: true,
            preserve_additional_properties_false: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForGemini.
pub fn for_gemini(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            ..Options::default()
        },
    )
}

/// CleanJSONSchemaForGeminiJSONSchema: for the `parametersJsonSchema` carrier, keeping
/// standard constraints and every `additionalProperties`.
pub fn for_gemini_json_schema(schema: &[u8]) -> Vec<u8> {
    clean(
        schema,
        Options {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            preserve_all_additional_properties: true,
            preserve_standard_constraints: true,
            ..Options::default()
        },
    )
}

/// InlineLocalRefs.
pub fn inline_local_refs(schema: &[u8]) -> Vec<u8> {
    inline_refs(schema.to_vec())
}

/// Clean a schema after normalization and budgeted local-reference inlining.
fn clean(schema: &[u8], options: Options) -> Vec<u8> {
    if let Some(cleaned) = clean_indexed(schema, options) {
        return cleaned;
    }
    let normalized = normalize_malformed_schema_objects(schema.to_vec(), options.add_missing_array_items);
    let inlined = inline_refs_for_clean(normalized);
    clean_indexed(&inlined, options)
        .or_else(|| indexed::clean(&inlined, options))
        .unwrap_or(inlined)
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn ordinary_property(name: &[u8]) -> bool {
    plain_name(name)
        && !matches!(
            name,
            b"_" | b"reason"
                | b"const"
                | b"enum"
                | b"properties"
                | b"patternProperties"
                | b"dependentSchemas"
                | b"dependencies"
                | b"definitions"
        )
}

/// Index descendants once, run the ordered rules on shallow schemas, and expand the
/// resulting holes once. Nested schemas use a wrapper to retain root-path differences.
fn clean_indexed(raw: &[u8], options: Options) -> Option<Vec<u8>> {
    use crate::schema_path::Document;
    use std::collections::HashSet;

    struct Hole {
        map: bool,
        key: Vec<u8>,
        node: usize,
    }
    enum Piece {
        Bytes(Vec<u8>),
        Child(usize),
    }
    // The inliner can re-marshal the whole document even for a literal "$ref" value.
    if crate::gostr::contains(raw, br#""$ref""#) {
        return None;
    }
    let document = Document::parse(raw)?;
    let nodes = &document.nodes;
    let mut schemas = vec![0usize];
    let mut holes: HashMap<usize, Vec<Hole>> = HashMap::new();
    let mut cursor = 0;
    while cursor < schemas.len() {
        let node = schemas[cursor];
        cursor += 1;
        if raw[nodes[node].span.start] != b'{' {
            return None;
        }
        // ponytail: raw-formatting preservation is limited to 128 schema fields and
        // independent local rules. Other valid schemas use the literal-key arena passes.
        if nodes[node].children.len() > 128 {
            return None;
        }
        let mut seen = HashSet::new();
        let mut children = Vec::new();
        for (key, child) in &nodes[node].children {
            if !seen.insert(key.as_slice())
                || (!plain_name(key)
                    && !matches!(
                        key.as_slice(),
                        b"$schema"
                            | b"$id"
                            | b"$anchor"
                            | b"$vocabulary"
                            | b"$dynamicRef"
                            | b"$dynamicAnchor"
                            | b"$comment"
                            | b"$ref"
                            | b"$defs"
                    ))
            {
                return None;
            }
            let value = &nodes[*child];
            let first = raw[value.span.start];
            match key.as_slice() {
                b"properties" => {
                    if first != b'{' {
                        return None;
                    }
                    let mut names = HashSet::new();
                    for (name, schema) in &value.children {
                        if !ordinary_property(name) || !names.insert(name.as_slice()) {
                            return None;
                        }
                        children.push(Hole {
                            map: true,
                            key: name.clone(),
                            node: *schema,
                        });
                    }
                }
                b"items" | b"additionalProperties" if first == b'{' => {
                    children.push(Hole {
                        map: false,
                        key: key.clone(),
                        node: *child,
                    });
                }
                b"$ref" | b"$defs" | b"definitions" | b"allOf" | b"anyOf" | b"oneOf" | b"if" | b"then" | b"else"
                | b"not" | b"schema" => return None,
                b"type" if first == b'[' => return None,
                b"required" if matches!(first, b't' | b'f') => return None,
                b"required" if first == b'[' => {
                    if value.children.iter().any(|(_, child)| {
                        let text = &raw[nodes[*child].span.clone()];
                        text.first() != Some(&b'"') || !plain_name(&gj::parse(text).bytes())
                    }) {
                        return None;
                    }
                }
                _ if first == b'{' => return None,
                _ if first == b'['
                    && (!matches!(
                        key.as_slice(),
                        b"enum" | b"required" | b"examples" | b"default" | b"example" | b"const"
                    ) || value
                        .children
                        .iter()
                        .any(|(_, child)| matches!(raw[nodes[*child].span.start], b'{' | b'['))) =>
                {
                    return None;
                }
                _ => {}
            }
        }
        for hole in &children {
            schemas.push(hole.node);
        }
        holes.insert(node, children);
    }

    let mut pieces: HashMap<usize, Vec<Piece>> = HashMap::new();
    for node in schemas {
        let children = holes.remove(&node)?;
        let span = if node == 0 {
            0..raw.len()
        } else {
            nodes[node].span.clone()
        };
        let mut shallow = Vec::new();
        let mut at = span.start;
        for hole in &children {
            shallow.extend_from_slice(&raw[at..nodes[hole.node].span.start]);
            shallow.extend_from_slice(b"null");
            at = nodes[hole.node].span.end;
        }
        shallow.extend_from_slice(&raw[at..span.end]);
        let (cleaned, base) = if node == 0 {
            (clean_normalized(shallow, options), None)
        } else {
            let wrapped = [br#"{"properties":{"v":"#.as_slice(), &shallow, b"}}"].concat();
            (clean_normalized(wrapped, options), Some(b"v".as_slice()))
        };
        let local = Document::parse(&cleaned)?;
        let root = if let Some(key) = base {
            local.field(local.field(0, b"properties")?, key)?
        } else {
            0
        };
        let mut replacements = Vec::new();
        let mut child_map: HashMap<&[u8], usize> = children
            .iter()
            .filter(|hole| hole.map)
            .map(|hole| (hole.key.as_slice(), hole.node))
            .collect();
        for (key, value) in &local.nodes[root].children {
            if key == b"properties" {
                for (name, value) in &local.nodes[*value].children {
                    if let Some(child) = child_map.remove(name.as_slice()) {
                        replacements.push((local.nodes[*value].span.clone(), child));
                    }
                }
            } else if let Some(hole) = children.iter().find(|hole| !hole.map && hole.key == *key) {
                replacements.push((local.nodes[*value].span.clone(), hole.node));
            }
        }
        let span = if node == 0 {
            0..cleaned.len()
        } else {
            local.nodes[root].span.clone()
        };
        let mut at = span.start;
        let mut output = Vec::new();
        for (hole, child) in replacements {
            output.push(Piece::Bytes(cleaned[at..hole.start].to_vec()));
            output.push(Piece::Child(child));
            at = hole.end;
        }
        output.push(Piece::Bytes(cleaned[at..span.end].to_vec()));
        pieces.insert(node, output);
    }
    let mut output = Vec::with_capacity(raw.len());
    let mut work = vec![Piece::Child(0)];
    while let Some(piece) = work.pop() {
        match piece {
            Piece::Bytes(bytes) => output.extend_from_slice(&bytes),
            Piece::Child(node) => work.extend(pieces.remove(&node)?.into_iter().rev()),
        }
    }
    Some(output)
}

#[cfg(test)]
fn clean_inner(schema: &[u8], options: Options) -> Vec<u8> {
    clean_normalized(
        normalize_malformed_schema_objects(schema.to_vec(), options.add_missing_array_items),
        options,
    )
}

fn clean_normalized(s: Vec<u8>, options: Options) -> Vec<u8> {
    // Every cleaner removes $defs/definitions later, so local references expand on every
    // route. Per request this costs one decode and re-encode of each schema with a
    // `"$ref"` (none without one), and up to the 256 KiB budget of copies.
    clean_inlined(inline_refs_for_clean(s), options)
}

fn clean_inlined(mut s: Vec<u8>, options: Options) -> Vec<u8> {
    s = convert_refs_to_hints(s, options.antigravity_semantics);
    s = convert_const_to_enum(s);
    s = convert_enum_values_to_strings(s, options.force_enum_string_type);
    s = add_enum_hints(s);
    s = drop_ignored_enums_to_hints(s, options);
    if !options.preserve_additional_properties_false && !options.preserve_all_additional_properties {
        s = add_additional_properties_hints(s);
    }
    s = move_constraints_to_description(s, options);
    if options.antigravity_semantics {
        s = move_not_to_description(s);
    }
    s = merge_conditionals(s);
    let preserve_conjunctions =
        options.preserve_standard_constraints && find_paths(&s, b"allOf").iter().any(|p| schema_get(&s, p).is_array());
    if !options.preserve_standard_constraints {
        s = merge_all_of(s);
    }
    if options.flatten_unions && !preserve_conjunctions {
        s = flatten_any_of_one_of(
            s,
            options.preserve_standard_constraints,
            options.flatten_multi_branch_unions,
        );
    }
    if !preserve_conjunctions {
        s = flatten_type_arrays(s, options.antigravity_semantics);
    }
    s = remove_unsupported_keywords(s, options);
    if options.remove_gemini_metadata {
        s = remove_keywords(s, &[b"nullable", b"title"]);
        s = remove_placeholder_fields(s);
    } else if options.remove_tool_title {
        s = remove_keywords(s, &[b"title"]);
    }
    if !preserve_conjunctions {
        s = cleanup_required_fields(s);
        s = sanitize_object_properties(s);
        s = sanitize_array_items(s, options.add_missing_array_items);
    }
    if options.add_placeholder {
        s = add_empty_schema_placeholder(s);
    }
    s
}

// ---------------------------------------------------------------------------------------
// Path helpers

fn escape_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    for &c in key {
        if matches!(c, b'.' | b'*' | b'?') {
            out.push(b'\\');
        }
        out.push(c);
    }
    out
}

fn unescape_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    let mut i = 0;
    while i < key.len() {
        if key[i] == b'\\' && i + 1 < key.len() {
            i += 1;
        }
        out.push(key[i]);
        i += 1;
    }
    out
}

/// splitGJSONPath: segments keep their escapes.
fn split_path(path: &[u8]) -> Vec<&[u8]> {
    if path.is_empty() {
        return vec![];
    }
    let mut parts = vec![];
    let mut start = 0;
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'\\' && i + 1 < path.len() {
            i += 2;
            continue;
        }
        if path[i] == b'.' {
            parts.push(&path[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&path[start..]);
    parts
}

fn join_path(base: &[u8], suffix: &[u8]) -> Vec<u8> {
    if base.is_empty() {
        return suffix.to_vec();
    }
    [base, b".", suffix].concat()
}

fn trim_suffix<'a>(path: &'a [u8], suffix: &[u8]) -> &'a [u8] {
    if path == suffix.strip_prefix(b".").unwrap_or(suffix) {
        return b"";
    }
    path.strip_suffix(suffix).unwrap_or(path)
}

fn depth_sorted(mut paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    paths.sort_by_key(|p| std::cmp::Reverse(split_path(p).len()));
    paths
}

/// util.WalkSchema: every path (keys escaped) whose last key equals `field`, in document
/// order, without descending into literal values.
fn find_paths(json: &[u8], field: &[u8]) -> Vec<Vec<u8>> {
    let mut paths = vec![];
    walk_fields(&gj::parse(json), b"", true, &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

fn find_paths_by_fields(json: &[u8], fields: &[&[u8]]) -> HashMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut paths: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    walk_fields(&gj::parse(json), b"", true, &mut |key, path| {
        if fields.contains(&key) {
            paths.entry(key.to_vec()).or_default().push(path.to_vec());
        }
    });
    paths
}

/// util.Walk: the escaped gjson paths of every `field` key inside `value`, depth first
/// in document order.
pub fn walk(value: &Res<'_>, field: &[u8]) -> Vec<Vec<u8>> {
    let mut paths = vec![];
    walk_fields(value, b"", false, &mut |key, path| {
        if key == field {
            paths.push(path.to_vec());
        }
    });
    paths
}

/// `schema_aware` (WalkSchema) records a literal keyword but does not descend into its value,
/// except under a name map, where the key is a property name.
fn walk_fields(value: &Res<'_>, path: &[u8], schema_aware: bool, visit: &mut dyn FnMut(&[u8], &[u8])) {
    enum Work<'a> {
        Node(Res<'a>, Vec<u8>),
        Entry(Vec<u8>, Res<'a>, Vec<u8>, bool),
    }
    let mut work = vec![Work::Node(value.clone(), path.to_vec())];
    while let Some(task) = work.pop() {
        match task {
            Work::Node(value, path) => {
                if value.kind != Kind::Json {
                    continue;
                }
                let mut entries = Vec::new();
                value.each(|key, child| {
                    entries.push((key.bytes().into_owned(), child));
                    true
                });
                for (key, child) in entries.into_iter().rev() {
                    let child_path = join_path(&path, &escape_key(&key));
                    let descend = !(schema_aware && is_literal_value_keyword(&key) && !is_property_definition(&path));
                    work.push(Work::Entry(key, child, child_path, descend));
                }
            }
            Work::Entry(key, child, path, descend) => {
                visit(&key, &path);
                if descend {
                    work.push(Work::Node(child, path));
                }
            }
        }
    }
}

/// isPropertyDefinition: an odd run of trailing name-map keywords means `path` is a map
/// of author-chosen names, not a schema.
fn is_property_definition(path: &[u8]) -> bool {
    let trailing = split_path(path)
        .iter()
        .rev()
        .take_while(|seg| is_name_map_keyword(&unescape_key(seg)))
        .count();
    trailing % 2 == 1
}

fn description_path(parent: &[u8]) -> Vec<u8> {
    if parent.is_empty() || parent == b"@this" {
        return b"description".to_vec();
    }
    [parent, b".description"].concat()
}

fn append_hint(mut json: Vec<u8>, parent: &[u8], hint: &[u8]) -> Vec<u8> {
    let path = description_path(parent);
    let merged = merge_hint(&schema_get(&json, &path).bytes(), hint);
    schema_set_str(&mut json, &path, String::from_utf8_lossy(&merged).as_bytes());
    json
}

fn get_strings(json: &[u8], path: &[u8]) -> Vec<Vec<u8>> {
    let arr = schema_get(json, path);
    if !arr.is_array() {
        return vec![];
    }
    arr.array().iter().map(|r| r.bytes().into_owned()).collect()
}

/// `sjson.SetBytes(json, path, []string)`: a nil (empty) slice marshals as `null`.
fn set_strings(json: &mut Vec<u8>, path: &[u8], items: &[Vec<u8>]) {
    if items.is_empty() {
        schema_set_raw(json, path, b"null");
    } else {
        schema_set_raw(json, path, gj::quote_all(items));
    }
}

fn set_raw_at(json: Vec<u8>, path: &[u8], value: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return value.to_vec();
    }
    let mut json = json;
    schema_set_raw(&mut json, path, value);
    json
}

fn delete(mut json: Vec<u8>, path: &[u8]) -> Vec<u8> {
    schema_delete(&mut json, path);
    json
}

// ---------------------------------------------------------------------------------------
// Phase 0: malformed object repair (Go's `any` model)

type Map = BTreeMap<String, GoValue>;

fn is_known_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

// strings.EqualFold against an ASCII word with no special Unicode folds ("object" and
// "array" contain neither k nor s) is an ASCII case-insensitive comparison.
fn is_non_object_type(t: Option<&GoValue>) -> bool {
    match t {
        Some(GoValue::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(GoValue::Array(items)) => {
            !items
                .iter()
                .any(|i| matches!(i, GoValue::String(s) if s.eq_ignore_ascii_case("object")))
                && !items.is_empty()
        }
        _ => false,
    }
}

fn is_api_request_document(mut m: &Map) -> bool {
    loop {
        for key in [
            "tools",
            "contents",
            "messages",
            "functionDeclarations",
            "function_declarations",
        ] {
            if matches!(m.get(key), Some(GoValue::Array(_))) {
                return true;
            }
        }
        match m.get("request") {
            Some(GoValue::Object(request)) => m = request,
            _ => return false,
        }
    }
}

fn normalize_malformed_schema_objects(json: Vec<u8>, add_missing_array_items: bool) -> Vec<u8> {
    if json.is_empty() {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    if root == GoValue::Bool(true) {
        return b"{}".to_vec();
    }
    let root = match root.into_object().ok() {
        Some(map) if !is_api_request_document(&map) => map,
        _ => return json,
    };
    if root.len() == 1 {
        match root.get("schema") {
            Some(GoValue::Object(inner)) => {
                let (repaired, modified) = repair_node(inner, add_missing_array_items);
                if !modified {
                    return json;
                }
                let mut wrapped = Map::new();
                wrapped.insert("schema".into(), GoValue::Object(repaired));
                return GoValue::Object(wrapped).marshal_no_html();
            }
            Some(GoValue::Bool(true)) => return br#"{"schema":{}}"#.to_vec(),
            _ => {}
        }
    }
    let (repaired, modified) = repair_node(&root, add_missing_array_items);
    if !modified {
        return json;
    }
    GoValue::Object(repaired).marshal_no_html()
}

fn string_array(value: Option<&GoValue>) -> Vec<String> {
    match value {
        Some(GoValue::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                GoValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn merge_string_slices(existing: Vec<String>, promoted: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    existing
        .into_iter()
        .chain(promoted.iter().cloned())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

fn promote_required(clone: &mut Map, promoted: &[String]) {
    let merged = merge_string_slices(string_array(clone.get("required")), promoted);
    // A nil []string marshals as null.
    let value = if merged.is_empty() {
        GoValue::Null
    } else {
        GoValue::Array(merged.into_iter().map(GoValue::String).collect())
    };
    clone.insert("required".into(), value);
}

fn empty_object() -> GoValue {
    GoValue::Object(Map::new())
}

fn repair_node(node: &Map, _add_items: bool) -> (Map, bool) {
    #[derive(Clone, Copy)]
    enum Mode {
        Schema,
        Names,
        List,
        Literal,
    }
    enum Work {
        Visit(GoValue, Mode),
        Object(Vec<String>, bool),
        Array(usize),
    }
    let mut work = vec![Work::Visit(GoValue::Object(node.clone()), Mode::Schema)];
    let mut results: Vec<(GoValue, bool)> = Vec::new();
    while let Some(task) = work.pop() {
        match task {
            Work::Visit(value, Mode::Literal) => results.push((value, false)),
            Work::Visit(value, mode) => {
                if matches!(mode, Mode::Schema) && matches!(value, GoValue::Bool(true)) {
                    results.push((empty_object(), true));
                    continue;
                }
                let value = match value.into_array() {
                    Ok(items) => {
                        if matches!(mode, Mode::List) {
                            work.push(Work::Array(items.len()));
                            for item in items.into_iter().rev() {
                                work.push(Work::Visit(item, Mode::Schema));
                            }
                        } else {
                            results.push((GoValue::Array(items), false));
                        }
                        continue;
                    }
                    Err(value) => value,
                };
                let mut map = match value.into_object() {
                    Ok(map) => map,
                    Err(value) => {
                        results.push((value, false));
                        continue;
                    }
                };
                let mut modified = false;
                if matches!(mode, Mode::Schema) {
                    if !is_non_object_type(map.get("type")) {
                        let keys: Vec<_> = map
                            .iter()
                            .filter(|(k, v)| matches!(v, GoValue::Object(_)) && !is_known_keyword_or_extension(k))
                            .map(|(k, _)| k.clone())
                            .collect();
                        if !keys.is_empty() {
                            let existing_properties = matches!(map.get("properties"), Some(GoValue::Object(_)));
                            let mut props = map
                                .remove("properties")
                                .and_then(|v| v.into_object().ok())
                                .unwrap_or_default();
                            for key in keys {
                                props.insert(key.clone(), map.remove(&key).unwrap());
                            }
                            map.insert("properties".into(), GoValue::Object(props));
                            if !existing_properties {
                                map.entry("type".into())
                                    .or_insert_with(|| GoValue::String("object".into()));
                            }
                            modified = true;
                        }
                    }
                    let mut promoted = Vec::new();
                    if let Some(GoValue::Object(props)) = map.get_mut("properties") {
                        for (name, value) in props {
                            if let GoValue::Object(child) = value
                                && matches!(child.get("required"), Some(GoValue::Bool(_)))
                            {
                                if matches!(child.remove("required"), Some(GoValue::Bool(true))) {
                                    promoted.push(name.clone());
                                }
                                modified = true;
                            }
                        }
                    }
                    if !promoted.is_empty() {
                        promote_required(&mut map, &promoted);
                    }
                }
                let entries: Vec<_> = map.into_iter().collect();
                work.push(Work::Object(entries.iter().map(|(k, _)| k.clone()).collect(), modified));
                for (key, value) in entries.into_iter().rev() {
                    let child_mode = match mode {
                        Mode::Names => Mode::Schema,
                        Mode::Schema => match key.as_str() {
                            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
                            | "dependencies"
                                if matches!(value, GoValue::Object(_)) =>
                            {
                                Mode::Names
                            }
                            "anyOf" | "oneOf" | "allOf" | "prefixItems" if matches!(value, GoValue::Array(_)) => {
                                Mode::List
                            }
                            "items" if matches!(value, GoValue::Array(_)) => Mode::List,
                            "items"
                            | "if"
                            | "then"
                            | "else"
                            | "not"
                            | "contains"
                            | "propertyNames"
                            | "unevaluatedProperties"
                            | "unevaluatedItems"
                            | "contentSchema"
                            | "additionalItems" => Mode::Schema,
                            "additionalProperties" if matches!(value, GoValue::Object(_)) => Mode::Schema,
                            _ => Mode::Literal,
                        },
                        _ => Mode::Literal,
                    };
                    work.push(Work::Visit(value, child_mode));
                }
            }
            Work::Object(keys, mut modified) => {
                let values = results.split_off(results.len() - keys.len());
                let mut map = Map::new();
                for (key, (value, changed)) in keys.into_iter().zip(values) {
                    modified |= changed;
                    map.insert(key, value);
                }
                results.push((GoValue::Object(map), modified));
            }
            Work::Array(n) => {
                let values = results.split_off(results.len() - n);
                let modified = values.iter().any(|(_, changed)| *changed);
                results.push((GoValue::Array(values.into_iter().map(|(v, _)| v).collect()), modified));
            }
        }
    }
    let (value, modified) = results.pop().unwrap();
    (value.into_object().unwrap(), modified)
}

// ---------------------------------------------------------------------------------------
// Phase 1: references, enums and hints

fn inline_refs(json: Vec<u8>) -> Vec<u8> {
    crate::schema_refs::inline_refs(json)
}
fn inline_refs_for_clean(json: Vec<u8>) -> Vec<u8> {
    crate::schema_refs::inline_refs_for_clean(json)
}

fn convert_refs_to_hints(mut json: Vec<u8>, preserve_siblings: bool) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"$ref")) {
        let parent = trim_suffix(&p, b".$ref").to_vec();
        if is_property_definition(&parent) {
            continue; // a property literally named "$ref"
        }
        let name = ref_name(&schema_get(&json, &p).bytes());
        let mut hint = [&b"See: "[..], &name].concat();
        if !preserve_siblings {
            let existing = schema_get(&json, &description_path(&parent)).bytes().into_owned();
            if !existing.is_empty() {
                hint = [&existing[..], b" (", &hint, b")"].concat();
            }
            let mut replacement = br#"{"type":"object","description":""}"#.to_vec();
            schema_set_str(&mut replacement, "description", &hint);
            json = set_raw_at(json, &parent, &replacement);
            continue;
        }
        json = delete(json, &p);
        json = append_hint(json, &parent, &hint);
    }
    json
}

fn convert_const_to_enum(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"const") {
        let value = schema_get(&json, &p).into_owned();
        if !value.exists() {
            continue;
        }
        let enum_path = join_path(trim_suffix(&p, b".const"), b"enum");
        if !schema_get(&json, &enum_path).exists() {
            AnyValue::Array(vec![AnyValue::from_res(&value)]).set(&mut json, &enum_path);
        }
    }
    json
}

fn convert_enum_values_to_strings(mut json: Vec<u8>, force_string_type: bool) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let arr = schema_get(&json, &p);
        if !arr.is_array() {
            continue;
        }
        let values: Vec<Vec<u8>> = arr.array().iter().map(|i| i.bytes().into_owned()).collect();
        set_strings(&mut json, &p, &values);
        if force_string_type {
            let parent = trim_suffix(&p, b".enum").to_vec();
            schema_set_str(&mut json, &join_path(&parent, b"type"), "string");
        }
    }
    json
}

fn add_enum_hints(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let arr = schema_get(&json, &p);
        if !arr.is_array() {
            continue;
        }
        let items = arr.array();
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let values: Vec<Vec<u8>> = items.iter().map(|i| i.bytes().into_owned()).collect();
        let hint = [&b"Allowed: "[..], &values.join(&b", "[..])].concat();
        json = append_hint(json, trim_suffix(&p, b".enum"), &hint);
    }
    json
}

fn drop_ignored_enums_to_hints(mut json: Vec<u8>, options: Options) -> Vec<u8> {
    for p in find_paths(&json, b"enum") {
        let parent = trim_suffix(&p, b".enum").to_vec();
        let drop = options.drop_all_enums
            || (options.drop_boolean_enums
                && schema_get(&json, &join_path(&parent, b"type")).bytes().as_ref() == b"boolean");
        if !drop {
            continue;
        }
        let values = schema_get(&json, &p);
        if values.is_array() {
            let items = values.array();
            if items.len() == 1 {
                let hint = [&b"Allowed: "[..], &items[0].bytes()].concat();
                json = append_hint(json, &parent, &hint);
            }
        }
        json = delete(json, &p);
    }
    json
}

fn add_additional_properties_hints(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"additionalProperties") {
        if schema_get(&json, &p).kind == Kind::False {
            json = append_hint(
                json,
                trim_suffix(&p, b".additionalProperties"),
                b"No extra properties allowed",
            );
        }
    }
    json
}

const UNSUPPORTED_CONSTRAINTS: [&[u8]; 12] = [
    b"minLength",
    b"maxLength",
    b"exclusiveMinimum",
    b"exclusiveMaximum",
    b"pattern",
    b"minItems",
    b"maxItems",
    b"uniqueItems",
    b"contains",
    b"format",
    b"default",
    b"examples",
];

fn constraint_keywords(options: Options) -> Vec<&'static [u8]> {
    if options.preserve_standard_constraints {
        return vec![];
    }
    let mut keywords = UNSUPPORTED_CONSTRAINTS.to_vec();
    if options.antigravity_semantics {
        keywords.extend([&b"minimum"[..], b"maximum", b"multipleOf"]);
    }
    keywords
}

fn move_constraints_to_description(mut json: Vec<u8>, options: Options) -> Vec<u8> {
    let constraints = constraint_keywords(options);
    if constraints.is_empty() {
        return json;
    }
    let by_field = find_paths_by_fields(&json, &constraints);
    for key in &constraints {
        for p in by_field.get(*key).into_iter().flatten() {
            let value = schema_get(&json, p).into_owned();
            if !value.exists() {
                continue;
            }
            let parent = trim_suffix(p, &[b".", *key].concat()).to_vec();
            if is_property_definition(&parent) {
                continue;
            }
            let shown = if value.is_object() || value.is_array() {
                value.raw.to_vec()
            } else {
                value.bytes().into_owned()
            };
            json = append_hint(json, &parent, &[*key, b": ", &shown].concat());
        }
    }
    json
}

fn move_not_to_description(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"not") {
        let value = schema_get(&json, &p).into_owned();
        let parent = trim_suffix(&p, b".not").to_vec();
        if !value.exists() || is_property_definition(&parent) {
            continue;
        }
        json = append_hint(json, &parent, &[&b"not: "[..], &value.raw].concat());
    }
    json
}

// ---------------------------------------------------------------------------------------
// Phase 2: flattening

fn merge_conditionals(mut json: Vec<u8>) -> Vec<u8> {
    let by_field = find_paths_by_fields(&json, &[b"then", b"else"]);
    let mut paths = vec![];
    for key in [&b"then"[..], b"else"] {
        for p in by_field.get(key).into_iter().flatten() {
            if !is_property_definition(trim_suffix(p, &[b".", key].concat())) {
                paths.push(p.clone());
            }
        }
    }
    for p in depth_sorted(paths) {
        let props = schema_get(&json, &join_path(&p, b"properties")).into_owned();
        if !props.is_object() {
            continue;
        }
        let parent: Vec<u8> = if let Some(parent) = p.strip_suffix(b".then").or_else(|| p.strip_suffix(b".else")) {
            parent.to_vec()
        } else if p == b"then" || p == b"else" {
            vec![]
        } else {
            continue;
        };
        props.each(|key, value| {
            let dest = join_path(&parent, &[&b"properties."[..], &escape_key(&key.bytes())].concat());
            if !schema_get(&json, &dest).exists() {
                schema_set_raw(&mut json, &dest, &value.raw);
            }
            true
        });
    }
    json
}

fn merge_all_of(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"allOf")) {
        let all_of = schema_get(&json, &p).into_owned();
        if !all_of.is_array() {
            continue;
        }
        let parent = trim_suffix(&p, b".allOf").to_vec();
        for item in all_of.array() {
            if !item.is_object() {
                continue;
            }
            item.each(|key, value| {
                let field = key.bytes();
                match field.as_ref() {
                    b"required" => {
                        if !value.is_array() {
                            return true;
                        }
                        let path = join_path(&parent, b"required");
                        let mut current = get_strings(&json, &path);
                        for required in value.array() {
                            let name = required.bytes().into_owned();
                            if !current.contains(&name) {
                                current.push(name);
                            }
                        }
                        set_strings(&mut json, &path, &current);
                    }
                    b"if" | b"then" | b"else" | b"allOf" => {}
                    _ => {
                        let dest = join_path(&parent, &escape_key(&field));
                        json = merge_missing_schema(std::mem::take(&mut json), &dest, &value);
                    }
                }
                true
            });
        }
        json = delete(json, &p);
    }
    json
}

/// mergeMissingSchemaAtPath: fills absent fields, never replacing an existing one.
fn merge_missing_schema(mut json: Vec<u8>, dest: &[u8], incoming: &Res<'_>) -> Vec<u8> {
    let mut work = vec![(dest.to_vec(), incoming.clone())];
    while let Some((dest, incoming)) = work.pop() {
        let existing = schema_get(&json, &dest);
        if !existing.exists() {
            schema_set_raw(&mut json, &dest, &incoming.raw);
        } else if existing.is_object() && incoming.is_object() {
            let mut children = Vec::new();
            incoming.each(|key, value| {
                children.push((join_path(&dest, &escape_key(&key.bytes())), value));
                true
            });
            work.extend(children.into_iter().rev());
        }
    }
    json
}

fn merge_description_raw(schema: &[u8], parent_desc: &[u8]) -> Vec<u8> {
    let child = schema_get(schema, "description").bytes().into_owned();
    let mut out = schema.to_vec();
    if child.is_empty() {
        schema_set_str(&mut out, "description", parent_desc);
    } else if child != parent_desc {
        schema_set_str(&mut out, "description", [parent_desc, b" (", &child, b")"].concat());
    }
    out
}

fn type_of(item: &Res<'_>) -> Vec<u8> {
    item.get("type").bytes().into_owned()
}

/// selectBest: objects beat arrays beat other types beat null; the first best wins.
fn select_best(items: &[Res<'_>]) -> (usize, Vec<Vec<u8>>) {
    let mut best = (0, -1);
    let mut types = vec![];
    for (i, item) in items.iter().enumerate() {
        let mut t = type_of(item);
        let score = if t == b"object" || item.get("properties").exists() {
            if t.is_empty() {
                t = b"object".to_vec();
            }
            3
        } else if t == b"array" || item.get("items").exists() {
            if t.is_empty() {
                t = b"array".to_vec();
            }
            2
        } else if !t.is_empty() && t != b"null" {
            1
        } else {
            0
        };
        if !t.is_empty() {
            types.push(t);
        }
        if score > best.1 {
            best = (i, score);
        }
    }
    (best.0, types)
}

fn is_required_only_union_branch(item: &Res<'_>) -> bool {
    item.is_object()
        && item.get("required").exists()
        && ["type", "properties", "items", "enum", "anyOf", "oneOf", "allOf"]
            .iter()
            .all(|k| !item.get(k).exists())
}

/// materializeRequiredOnlyBranch: a `{"required": [...]}` branch becomes an object branch
/// with the parent's definitions of those properties.
fn materialize_required_only_branch(item: &Res<'_>, parent_props: &Res<'_>) -> Option<Vec<u8>> {
    if !parent_props.is_object() {
        return None;
    }
    let required = get_strings(&item.raw, b"required");
    if required.is_empty() {
        return None;
    }
    let mut raw = item.raw.to_vec();
    for name in &required {
        let escaped = escape_key(name);
        let prop = parent_props.get(&escaped);
        if !prop.exists() {
            return None;
        }
        schema_set_raw(&mut raw, &[&b"properties."[..], &escaped].concat(), &prop.raw);
    }
    schema_set_str(&mut raw, "type", "object");
    Some(gj::parse(&raw).raw.to_vec())
}

fn is_null_only_branch(item: &Res<'_>) -> bool {
    if item.get("properties").exists() || item.get("items").exists() {
        return false;
    }
    let t = item.get("type");
    if t.kind == Kind::String {
        return t.bytes().as_ref() == b"null";
    }
    t.is_array() && !t.array().is_empty() && t.array().iter().all(|e| e.bytes().as_ref() == b"null")
}

/// partitionUnionBranches: the non-null branches (raw), whether a null branch was seen,
/// and whether any branch was dropped or rewritten.
fn partition_union_branches(items: &[Res<'_>], parent_props: &Res<'_>) -> (Vec<Vec<u8>>, bool, bool) {
    let (mut non_null, mut has_null, mut filtered) = (vec![], false, false);
    for item in items {
        if item.kind == Kind::True {
            non_null.push(b"{}".to_vec());
            filtered = true;
        } else if !item.is_object() {
            filtered = true;
        } else if is_null_only_branch(item) {
            has_null = true;
        } else if is_required_only_union_branch(item) {
            if let Some(materialized) = materialize_required_only_branch(item, parent_props) {
                non_null.push(materialized);
            }
            filtered = true;
        } else {
            non_null.push(item.raw.to_vec());
        }
    }
    (non_null, has_null, filtered)
}

/// flattenAnyOfOneOf: multi-branch unions are kept (oneOf becomes anyOf on native Schema
/// carriers) unless `flatten_multi_branch`; other unions project onto the strongest branch.
fn flatten_any_of_one_of(mut json: Vec<u8>, preserve_one_of: bool, flatten_multi_branch: bool) -> Vec<u8> {
    let mut by_field = find_paths_by_fields(&json, &[b"oneOf", b"anyOf"]);
    let mut paths = by_field.remove(&b"oneOf"[..]).unwrap_or_default();
    paths.extend(by_field.remove(&b"anyOf"[..]).unwrap_or_default());
    for p in depth_sorted(paths) {
        let key: &[u8] = if p == b"oneOf" || p.ends_with(b".oneOf") {
            b"oneOf"
        } else {
            b"anyOf"
        };
        let arr = schema_get(&json, &p).into_owned();
        if !arr.is_array() || arr.array().is_empty() {
            continue;
        }
        let parent_path = trim_suffix(&p, &[b".", key].concat()).to_vec();
        if is_property_definition(&parent_path) {
            continue;
        }
        let parent = if parent_path.is_empty() {
            gj::parse(&json).into_owned()
        } else {
            schema_get(&json, &parent_path).into_owned()
        };
        // A native Schema has a single anyOf: a sibling oneOf moves to a hint.
        if key == b"oneOf" && !flatten_multi_branch && !preserve_one_of && parent.get("anyOf").is_array() {
            json = append_hint(json, &parent_path, &[&b"oneOf: "[..], &arr.raw].concat());
            json = delete(json, &p);
            continue;
        }
        let items = arr.array();
        let parent_props = parent.get("properties");
        let (non_null, has_null, filtered) = partition_union_branches(&items, &parent_props);
        // An earlier pass may already have recorded the null branch as nullable:true.
        let nullable = has_null || parent.get("nullable").bool();

        if non_null.len() >= 2 && !flatten_multi_branch {
            let target_key: &[u8] = if key == b"oneOf" && !preserve_one_of {
                b"anyOf"
            } else {
                key
            };
            if target_key != key || has_null || filtered {
                // sjson.SetBytes with []json.RawMessage: Marshal compacts each branch.
                let mut list = b"[".to_vec();
                for (i, raw) in non_null.iter().enumerate() {
                    if i > 0 {
                        list.push(b',');
                    }
                    list.extend(gj::compact(raw, true));
                }
                list.push(b']');
                schema_set_raw(&mut json, &join_path(&parent_path, target_key), &list);
                if target_key != key {
                    json = delete(json, &p);
                }
            }
            if has_null {
                schema_set_bool(&mut json, &join_path(&parent_path, b"nullable"), true);
                json = append_hint(json, &parent_path, b"(nullable)");
            }
            continue;
        }

        if parent_props.is_object() {
            for raw in &non_null {
                let branch = gj::parse(raw);
                let branch_props = branch.get("properties");
                if branch_props.is_object() {
                    branch_props.each(|prop_key, prop_value| {
                        let dest = join_path(
                            &parent_path,
                            &[&b"properties."[..], &escape_key(&prop_key.bytes())].concat(),
                        );
                        json = merge_missing_schema(std::mem::take(&mut json), &dest, &prop_value);
                        true
                    });
                }
            }
            if nullable {
                schema_set_bool(&mut json, &join_path(&parent_path, b"nullable"), true);
            }
            if parent.get("type").bytes().is_empty() {
                schema_set_str(&mut json, &join_path(&parent_path, b"type"), "object");
            }
            json = delete(json, &p);
            continue;
        }

        if non_null.is_empty() {
            if !has_null {
                json = delete(json, &p);
                continue;
            }
            let mut selected = br#"{"type":"null"}"#.to_vec();
            let parent_desc = schema_get(&json, &description_path(&parent_path)).bytes().into_owned();
            if !parent_desc.is_empty() {
                selected = merge_description_raw(&selected, &parent_desc);
            }
            json = set_raw_at(json, &parent_path, &selected);
            continue;
        }

        let parsed: Vec<Res<'_>> = non_null.iter().map(|raw| gj::parse(raw)).collect();
        let null_branch = gj::parse(br#"{"type":"null"}"#);
        let all_types = if filtered || non_null.len() >= 2 {
            let mut candidates = parsed.clone();
            if has_null {
                candidates.push(null_branch);
            }
            select_best(&candidates).1
        } else {
            select_best(&items).1
        };
        let parent_desc = schema_get(&json, &description_path(&parent_path)).bytes().into_owned();
        let mut selected = non_null[select_best(&parsed).0].clone();
        if nullable {
            schema_set_bool(&mut selected, "nullable", true);
        }
        if !parent_desc.is_empty() {
            selected = merge_description_raw(&selected, &parent_desc);
        }
        let hint = if all_types.len() > 1 {
            Some([&b"Accepts: "[..], &all_types.join(&b" | "[..])].concat())
        } else if nullable {
            Some(b"(nullable)".to_vec())
        } else {
            None
        };
        if let Some(hint) = hint {
            let merged = merge_hint(&schema_get(&selected, "description").bytes(), &hint);
            schema_set_str(&mut selected, "description", merged);
        }
        json = set_raw_at(json, &parent_path, &selected);
    }
    json
}

fn flatten_type_arrays(mut json: Vec<u8>, preserve_native_nullable: bool) -> Vec<u8> {
    let mut nullable_fields: Vec<(Vec<u8>, Vec<Vec<u8>>)> = vec![];
    for p in depth_sorted(find_paths(&json, b"type")) {
        let res = schema_get(&json, &p);
        if !res.is_array() {
            continue;
        }
        let values = res.array();
        if values.is_empty() {
            continue;
        }
        let mut has_null = false;
        let mut non_null: Vec<Vec<u8>> = vec![];
        for item in &values {
            let s = item.bytes();
            if s.as_ref() == b"null" {
                has_null = true;
            } else if !s.is_empty() {
                non_null.push(s.into_owned());
            }
        }
        let parent = trim_suffix(&p, b".type").to_vec();
        let items_path = join_path(&parent, b"items");
        let first = if non_null.is_empty() {
            b"string".to_vec()
        } else if schema_get(&json, &items_path).exists() && non_null.iter().any(|t| t == b"array") {
            b"array".to_vec()
        } else if schema_get(&json, &join_path(&parent, b"properties")).exists()
            && non_null.iter().any(|t| t == b"object")
        {
            b"object".to_vec()
        } else {
            non_null[0].clone()
        };
        schema_set_str(&mut json, &p, &first);
        if first != b"array" && schema_get(&json, &items_path).exists() {
            schema_delete(&mut json, &items_path);
        }
        if non_null.len() > 1 {
            json = append_hint(
                json,
                &parent,
                &[&b"Accepts: "[..], &non_null.join(&b" | "[..])].concat(),
            );
        }
        if has_null {
            if preserve_native_nullable {
                schema_set_bool(&mut json, &join_path(&parent, b"nullable"), true);
                json = append_hint(json, &parent, b"(nullable)");
                continue;
            }
            let parts = split_path(&p);
            if parts.len() >= 3 && parts[parts.len() - 3] == b"properties" {
                let escaped = parts[parts.len() - 2].to_vec();
                let object_path = parts[..parts.len() - 3].join(&b'.');
                let field = unescape_key(&escaped);
                match nullable_fields.iter_mut().find(|(o, _)| *o == object_path) {
                    Some((_, fields)) => fields.push(field),
                    None => nullable_fields.push((object_path.clone(), vec![field])),
                }
                let target = join_path(&object_path, &[&b"properties."[..], &escaped].concat());
                json = append_hint(json, &target, b"(nullable)");
            }
        }
    }
    for (object_path, fields) in nullable_fields {
        let path = join_path(&object_path, b"required");
        let required = schema_get(&json, &path);
        if !required.is_array() {
            continue;
        }
        let kept: Vec<Vec<u8>> = required
            .array()
            .iter()
            .map(|r| r.bytes().into_owned())
            .filter(|r| !fields.contains(r))
            .collect();
        if kept.is_empty() {
            schema_delete(&mut json, &path);
        } else {
            schema_set_strs(&mut json, &path, &kept);
        }
    }
    json
}

// ---------------------------------------------------------------------------------------
// Phase 3: cleanup

fn delete_keyword_paths(mut json: Vec<u8>, keywords: &[&[u8]], keep: impl Fn(&[u8], &[u8], &[u8]) -> bool) -> Vec<u8> {
    let by_field = find_paths_by_fields(&json, keywords);
    let mut paths = vec![];
    for key in keywords {
        for p in by_field.get(*key).into_iter().flatten() {
            if is_property_definition(trim_suffix(p, &[b".", *key].concat())) || keep(key, p, &json) {
                continue;
            }
            paths.push(p.clone());
        }
    }
    for p in depth_sorted(paths) {
        schema_delete(&mut json, &p);
    }
    json
}

fn remove_unsupported_keywords(json: Vec<u8>, options: Options) -> Vec<u8> {
    let mut keywords = constraint_keywords(options);
    keywords.extend([
        &b"$schema"[..],
        b"$defs",
        b"definitions",
        b"const",
        b"$ref",
        b"$id",
        b"id",
        b"additionalProperties",
        b"$anchor",
        b"$vocabulary",
        b"$dynamicRef",
        b"$dynamicAnchor",
        b"propertyNames",
        b"patternProperties",
        b"dependentRequired",
        b"dependentSchemas",
        b"dependencies",
        b"if",
        b"then",
        b"else",
        b"$comment",
        b"enumDescriptions",
        b"enumTitles",
        b"prefill",
        b"deprecated",
        b"encrypted",
        b"discriminator",
        b"readOnly",
        b"writeOnly",
        b"additionalItems",
        b"unevaluatedProperties",
        b"unevaluatedItems",
        b"contentSchema",
        b"contentMediaType",
        b"contentEncoding",
    ]);
    if options.antigravity_semantics {
        keywords.push(b"not");
    }
    let json = delete_keyword_paths(json, &keywords, |key, p, json| {
        key == b"additionalProperties"
            && (options.preserve_all_additional_properties
                || (options.preserve_additional_properties_false && schema_get(json, p).kind == Kind::False))
    });
    remove_extension_fields(json)
}

fn remove_extension_fields(mut json: Vec<u8>) -> Vec<u8> {
    let mut paths = vec![];
    walk_extensions(&gj::parse(&json), b"", &mut paths);
    for p in paths {
        schema_delete(&mut json, &p);
    }
    json
}

fn walk_extensions(value: &Res<'_>, path: &[u8], paths: &mut Vec<Vec<u8>>) {
    let mut work = vec![(value.clone(), path.to_vec())];
    while let Some((value, path)) = work.pop() {
        if value.is_array() {
            for (i, item) in value.array().into_iter().enumerate() {
                work.push((item, join_path(&path, i.to_string().as_bytes())));
            }
        } else if value.is_object() {
            let mut children = Vec::new();
            value.each(|key, child| {
                let key = key.bytes();
                let child_path = join_path(&path, &escape_key(&key));
                if key.starts_with(b"x-") && !is_property_definition(&path) {
                    paths.push(child_path);
                } else if !(is_literal_value_keyword(&key) && !is_property_definition(&path)) {
                    children.push((child, child_path));
                }
                true
            });
            work.extend(children.into_iter().rev());
        }
    }
}

fn remove_keywords(json: Vec<u8>, keywords: &[&[u8]]) -> Vec<u8> {
    delete_keyword_paths(json, keywords, |_, _, _| false)
}

fn filter_required(json: &mut Vec<u8>, parent: &[u8], name: &[u8]) {
    let path = join_path(parent, b"required");
    let required = schema_get(json, &path);
    if !required.is_array() {
        return;
    }
    let kept: Vec<Vec<u8>> = required
        .array()
        .iter()
        .map(|r| r.bytes().into_owned())
        .filter(|r| r != name)
        .collect();
    if kept.is_empty() {
        schema_delete(json, &path);
    } else {
        schema_set_strs(json, &path, &kept);
    }
}

fn remove_placeholder_fields(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"_")) {
        let Some(parent) = p.strip_suffix(b".properties._") else {
            continue;
        };
        schema_delete(&mut json, &p);
        filter_required(&mut json, parent, b"_");
    }
    for p in depth_sorted(find_paths(&json, b"reason")) {
        let Some(parent) = p.strip_suffix(b".properties.reason") else {
            continue;
        };
        let props = schema_get(&json, &join_path(parent, b"properties"));
        let distinct: std::collections::HashSet<Vec<u8>> = props.map().into_iter().map(|(k, _)| k).collect();
        if !props.is_object() || distinct.len() != 1 {
            continue;
        }
        if schema_get(&json, &[&p[..], b".description"].concat()).bytes().as_ref() != PLACEHOLDER_REASON_DESCRIPTION {
            continue;
        }
        schema_delete(&mut json, &p);
        filter_required(&mut json, parent, b"reason");
    }
    json
}

fn cleanup_required_fields(mut json: Vec<u8>) -> Vec<u8> {
    for p in find_paths(&json, b"required") {
        let parent = trim_suffix(&p, b".required").to_vec();
        let required = schema_get(&json, &p).into_owned();
        let props = schema_get(&json, &join_path(&parent, b"properties")).into_owned();
        if !required.is_array() {
            continue;
        }
        if !props.is_object() {
            schema_delete(&mut json, &p);
            continue;
        }
        let names = required.array();
        let mut literal_names = std::collections::HashSet::new();
        let mut literal = true;
        props.each(|key, _| {
            let name = key.bytes();
            literal &= plain_name(&name);
            literal_names.insert(name.into_owned());
            true
        });
        let valid: Vec<Vec<u8>> = names
            .iter()
            .map(|r| r.bytes().into_owned())
            .filter(|key| {
                if literal && plain_name(key) {
                    literal_names.contains(key)
                } else {
                    props.get(&escape_key(key)).exists()
                }
            })
            .collect();
        if valid.len() != names.len() {
            if valid.is_empty() {
                schema_delete(&mut json, &p);
            } else {
                schema_set_strs(&mut json, &p, &valid);
            }
        }
    }
    json
}

/// sanitizeObjectProperties: a node declaring `properties` is typed `object`.
fn sanitize_object_properties(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"properties")) {
        let parent = trim_suffix(&p, b".properties").to_vec();
        if is_property_definition(&parent) || !schema_get(&json, &p).is_object() {
            continue;
        }
        let type_path = join_path(&parent, b"type");
        if !schema_get(&json, &type_path).bytes().eq_ignore_ascii_case(b"object") {
            schema_set_str(&mut json, &type_path, "object");
        }
    }
    json
}

/// sanitizeArrayItems; with `add_missing`, an array node still lacking `items` after
/// reference inlining gets Phase 0's placeholder.
fn sanitize_array_items(mut json: Vec<u8>, add_missing: bool) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"items")) {
        let parent = trim_suffix(&p, b".items").to_vec();
        if is_property_definition(&parent) {
            continue;
        }
        let type_path = join_path(&parent, b"type");
        let t = schema_get(&json, &type_path).bytes().into_owned();
        if t.is_empty() {
            schema_set_str(&mut json, &type_path, "array");
        } else if !t.eq_ignore_ascii_case(b"array") {
            schema_delete(&mut json, &p);
        }
    }
    if !add_missing {
        return json;
    }
    for p in depth_sorted(find_paths(&json, b"type")) {
        let parent = trim_suffix(&p, b".type").to_vec();
        if is_property_definition(&parent) || !is_array_declared(&schema_get(&json, &p)) {
            continue;
        }
        if schema_get(&json, &join_path(&parent, b"items")).exists()
            || schema_get(&json, &join_path(&parent, b"$ref")).exists()
        {
            continue;
        }
        schema_set_raw(&mut json, &join_path(&parent, b"items"), br#"{"type":"string"}"#);
    }
    json
}

/// isArrayDeclaredType on a gjson value.
fn is_array_declared(t: &Res<'_>) -> bool {
    match t.kind {
        Kind::String => t.bytes().eq_ignore_ascii_case(b"array"),
        _ if t.is_array() => t
            .array()
            .iter()
            .any(|i| i.kind == Kind::String && i.bytes().eq_ignore_ascii_case(b"array")),
        _ => false,
    }
}

fn add_empty_schema_placeholder(mut json: Vec<u8>) -> Vec<u8> {
    for p in depth_sorted(find_paths(&json, b"type")) {
        if schema_get(&json, &p).bytes().as_ref() != b"object" {
            continue;
        }
        let parent = trim_suffix(&p, b".type").to_vec();
        let props_path = join_path(&parent, b"properties");
        let props = schema_get(&json, &props_path).into_owned();
        let required_path = join_path(&parent, b"required");
        let required = schema_get(&json, &required_path);
        let has_required = required.is_array() && !required.array().is_empty();
        if schema_get(&json, &join_path(&parent, b"anyOf")).is_array()
            || schema_get(&json, &join_path(&parent, b"oneOf")).is_array()
        {
            continue;
        }
        if !props.exists()
            || (props.is_object() && {
                let mut empty = true;
                props.each(|_, _| {
                    empty = false;
                    false
                });
                empty
            })
        {
            let reason = join_path(&props_path, b"reason");
            schema_set_str(&mut json, &[&reason[..], b".type"].concat(), "string");
            schema_set_str(
                &mut json,
                &[&reason[..], b".description"].concat(),
                PLACEHOLDER_REASON_DESCRIPTION,
            );
            schema_set_strs(&mut json, &required_path, &["reason"]);
            continue;
        }
        if props.is_object() && !has_required {
            if parent.is_empty() {
                continue;
            }
            let placeholder = join_path(&props_path, b"_");
            if !schema_get(&json, &placeholder).exists() {
                schema_set_str(&mut json, &[&placeholder[..], b".type"].concat(), "boolean");
            }
            schema_set_strs(&mut json, &required_path, &["_"]);
        }
    }
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's TestIssue6732_CleanInliningStripsDefinitionsFromIntermediate: the cleaning
    /// inliner drops definition containers, so a large unused definition is never copied
    /// into the intermediate document, and both references still expand.
    #[test]
    fn clean_inlining_strips_definitions_from_intermediate() {
        let person = format!(
            r#"{{"type":"object","properties":{{"name":{{"type":"string"}}}},"$defs":{{"Unused":{{"type":"string","description":"{}"}}}}}}"#,
            "x".repeat(280 * 1024)
        );
        let input = format!(
            r##"{{"type":"object","properties":{{"p0":{{"$ref":"#/$defs/Person"}},"p1":{{"$ref":"#/$defs/Person"}}}},"$defs":{{"Person":{person}}}}}"##
        );
        let got = inline_refs_for_clean(input.into_bytes());
        assert!(got.len() < 64 * 1024, "intermediate is {} bytes", got.len());
        assert!(!schema_get(&got, "$defs").exists());
        assert!(!schema_get(&got, "properties.p0.$defs").exists());
        assert_eq!(
            schema_get(&got, "properties.p1.properties.name.type").bytes().as_ref(),
            b"string"
        );
    }
}
