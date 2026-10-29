//! Claude tool names and arguments on the way back to a Claude client
//! (internal/util/translator.go, 202b8b8c): exact declared names win over canonical
//! restoration, and the `reason` / `_` placeholders a Gemini-family schema cleaner
//! injected for empty object schemas are removed from the returned arguments, judged
//! against the client's original schema and, when present, the schema actually sent.
//!
//! Schemas the analysis does not model (unknown keywords, multi-branch unions it
//! cannot resolve, unresolvable `$ref`) leave the arguments untouched.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{self, canonical_tool_name, trim_space};

/// util.placeholderReasonDescription.
const PLACEHOLDER_REASON_DESCRIPTION: &[u8] = b"Brief explanation of why you are calling this tool";

/// A tool's declared name: `name`, else `function.name`, trimmed.
fn declared_name(tool: &Res<'_>) -> Vec<u8> {
    let name = trim_space(&tool.get("name").bytes()).to_vec();
    if name.is_empty() {
        trim_space(&tool.get("function.name").bytes()).to_vec()
    } else {
        name
    }
}

/// claudeObjectSchemaInfo.
#[derive(Default, Clone)]
struct Info<'a> {
    properties: HashMap<Vec<u8>, Vec<Res<'a>>>,
    required: HashSet<Vec<u8>>,
    has_properties: bool,
    allows_dynamic: bool,
    forbids_dynamic: bool,
    has_pattern_properties: bool,
    complex: bool,
    unsafe_complex: bool,
    declares_object: bool,
    branch_infos: Vec<Info<'a>>,
}

trait ObjectKeys {
    fn contains_key(&self, key: &[u8]) -> bool;
}
impl ObjectKeys for Res<'_> {
    fn contains_key(&self, key: &[u8]) -> bool {
        let mut found = false;
        if self.is_object() {
            self.each(|name, _| {
                found = name.bytes().as_ref() == key;
                !found
            });
        }
        found
    }
}
fn has_key(object: &impl ObjectKeys, key: &[u8]) -> bool {
    object.contains_key(key)
}

fn has_pattern_properties(schema: &Res<'_>) -> bool {
    let patterns = schema.get("patternProperties");
    let mut found = false;
    if patterns.is_object() {
        patterns.each(|_, _| {
            found = true;
            false
        });
    }
    found
}

/// claudeSchemaAllowsDynamic: pattern properties, or `additionalProperties` other
/// than `false`.
fn allows_dynamic(schema: &Res<'_>) -> bool {
    let additional = schema.get("additionalProperties");
    has_pattern_properties(schema) || (additional.exists() && additional.kind != Kind::False)
}

fn forbids_dynamic(schema: &Res<'_>) -> bool {
    let additional = schema.get("additionalProperties");
    additional.exists() && additional.kind == Kind::False
}

/// claudeResolveLocalRef: a `#/...` JSON pointer within `root`.
fn resolve_local_ref<'a>(root: &Res<'a>, reference: &[u8]) -> Option<Res<'a>> {
    let pointer = reference.strip_prefix(b"#/")?;
    let mut current = root.clone();
    for segment in pointer.split(|&c| c == b'/') {
        let segment = String::from_utf8_lossy(segment).replace("~1", "/").replace("~0", "~");
        let mut next = None;
        if current.is_object() {
            current.each(|key, value| {
                if key.bytes().as_ref() == segment.as_bytes() {
                    next = Some(value);
                    false
                } else {
                    true
                }
            });
        }
        current = match next {
            Some(next) => next,
            None if current.is_array() => {
                let items = current.array();
                let index: usize = segment.parse().ok()?;
                items.get(index)?.clone()
            }
            None => return None,
        };
    }
    Some(current)
}

fn declares_object(schema: &Res<'_>) -> bool {
    let kind = schema.get("type");
    &*kind.bytes() == b"object" || (kind.is_array() && kind.array().iter().any(|t| &*t.bytes() == b"object"))
}

/// claudeUnionBranches: the non-null `anyOf`/`oneOf` branches, deduplicated; `None`
/// when a union keyword is not an array.
fn union_branches<'a>(schema: &Res<'a>) -> Option<Vec<Res<'a>>> {
    let mut branches = vec![];
    let mut seen = HashSet::new();
    for keyword in ["anyOf", "oneOf"] {
        let union = schema.get(keyword);
        if !union.exists() {
            continue;
        }
        if !union.is_array() {
            return None;
        }
        for branch in union.array() {
            let kind = branch.get("type");
            if &*kind.bytes() == b"null" {
                continue;
            }
            if kind.is_array() {
                let kinds = kind.array();
                if kinds.len() == 1 && &*kinds[0].bytes() == b"null" {
                    continue;
                }
            }
            if seen.insert(branch.raw.to_vec()) {
                branches.push(branch);
            }
        }
    }
    Some(branches)
}

/// claudeAnalyzedSchemaKeywords.
const ANALYZED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "title",
    "properties",
    "required",
    "items",
    "additionalProperties",
    "patternProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "$ref",
    "$defs",
    "definitions",
    "nullable",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "pattern",
    "format",
    "default",
    "examples",
    "minItems",
    "maxItems",
    "maxProperties",
    "$schema",
    "$id",
    "id",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "example",
    "contentEncoding",
    "contentMediaType",
];

fn has_unanalyzed_keyword(schema: &Res<'_>) -> bool {
    let mut found = false;
    schema.each(|key, _| {
        let key = key.bytes();
        found = !key.starts_with(b"x-") && !ANALYZED_KEYWORDS.iter().any(|k| k.as_bytes() == key.as_ref());
        !found
    });
    found
}

fn push_unique<'a>(schemas: &mut Vec<Res<'a>>, value: Res<'a>) {
    if !schemas.iter().any(|s| s.raw == value.raw) {
        schemas.push(value);
    }
}

/// mergeClaudeObjectSchemaInfo.
fn merge<'a>(dst: &mut Info<'a>, src: Info<'a>) {
    if src.has_properties {
        dst.has_properties = true;
        for (key, values) in src.properties {
            let entry = dst.properties.entry(key).or_default();
            for value in values {
                push_unique(entry, value);
            }
        }
    }
    dst.required.extend(src.required);
    dst.branch_infos.extend(src.branch_infos);
    dst.allows_dynamic |= src.allows_dynamic;
    dst.forbids_dynamic |= src.forbids_dynamic;
    dst.has_pattern_properties |= src.has_pattern_properties;
    dst.complex |= src.complex;
    dst.unsafe_complex |= src.unsafe_complex;
    dst.declares_object |= src.declares_object;
}

fn collect_properties<'a>(info: &mut Info<'a>, props: &Res<'a>) {
    info.has_properties = true;
    info.declares_object = true;
    let mut seen = HashSet::new();
    props.each(|key, value| {
        let key = key.bytes().into_owned();
        if seen.insert(key.clone()) {
            push_unique(info.properties.entry(key).or_default(), value);
        }
        true
    });
}

fn collect_required(info: &mut Info<'_>, schema: &Res<'_>) {
    for name in schema.get("required").array() {
        info.required.insert(name.bytes().into_owned());
    }
}

/// analyzeClaudeObjectSchema.
fn analyze<'a>(schema: &Res<'a>, root: &Res<'a>) -> Info<'a> {
    analyze_depth(schema, root, 0, &mut HashSet::new())
}

/// Resolves `$ref` against `root` and merges the target's analysis; `false` when the
/// reference cycles or does not resolve.
fn merge_ref<'a>(
    info: &mut Info<'a>,
    reference: &Res<'a>,
    root: &Res<'a>,
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> bool {
    let value = reference.bytes().into_owned();
    if visited.contains(&value) {
        return false;
    }
    let Some(target) = resolve_local_ref(root, &value) else {
        return false;
    };
    visited.insert(value.clone());
    let resolved = analyze_depth(&target, root, depth + 1, visited);
    visited.remove(&value);
    merge(info, resolved);
    true
}

/// analyzeClaudeObjectSchemaDepth.
fn analyze_depth<'a>(schema: &Res<'a>, root: &Res<'a>, depth: usize, visited: &mut HashSet<Vec<u8>>) -> Info<'a> {
    let mut info = Info::default();
    if depth > 16 || !schema.exists() || !schema.is_object() {
        info.complex = true;
        return info;
    }
    info.declares_object = declares_object(schema);
    if has_unanalyzed_keyword(schema) {
        // Keywords outside the analyzed set (enum, const, conditionals, ...) may constrain
        // legal values in ways this analysis does not model: never clean such a schema.
        info.complex = true;
        info.unsafe_complex = true;
        return info;
    }
    if schema.get("anyOf").exists() || schema.get("oneOf").exists() {
        let branches = match union_branches(schema) {
            Some(branches) if !branches.is_empty() => branches,
            _ => {
                info.complex = true;
                return info;
            }
        };
        if branches.len() > 1 {
            // Multi-branch unions stay complex, but every branch's declared properties
            // are collected; the branch infos are kept apart because branches are
            // alternatives, not simultaneous constraints.
            for branch in &branches {
                let branch_info = analyze_depth(branch, root, depth + 1, visited);
                info.branch_infos.push(branch_info.clone());
                merge(&mut info, branch_info);
            }
            info.allows_dynamic |= allows_dynamic(schema);
            info.forbids_dynamic |= forbids_dynamic(schema);
            info.has_pattern_properties |= has_pattern_properties(schema);
            collect_required(&mut info, schema);
            let props = schema.get("properties");
            if props.is_object() {
                collect_properties(&mut info, &props);
            }
            let reference = schema.get("$ref");
            if reference.exists() {
                merge_ref(&mut info, &reference, root, depth, visited);
            }
            let all_of = schema.get("allOf");
            if all_of.is_array() {
                for branch in all_of.array() {
                    let branch_info = analyze_depth(&branch, root, depth + 1, visited);
                    merge(&mut info, branch_info);
                }
            }
            info.complex = true;
            return info;
        }
        let branch_info = analyze_depth(&branches[0], root, depth + 1, visited);
        merge(&mut info, branch_info);
    }
    let reference = schema.get("$ref");
    if reference.exists() && !merge_ref(&mut info, &reference, root, depth, visited) {
        info.complex = true;
        return info;
    }
    let all_of = schema.get("allOf");
    if all_of.is_array() {
        for branch in all_of.array() {
            let branch_info = analyze_depth(&branch, root, depth + 1, visited);
            merge(&mut info, branch_info);
        }
    } else if all_of.exists() {
        info.complex = true;
    }
    info.allows_dynamic |= allows_dynamic(schema);
    info.forbids_dynamic |= forbids_dynamic(schema);
    info.has_pattern_properties |= has_pattern_properties(schema);
    let props = schema.get("properties");
    if props.exists() {
        if props.is_object() {
            collect_properties(&mut info, &props);
        } else {
            info.complex = true;
        }
    }
    collect_required(&mut info, schema);
    info
}

/// findClaudeToolSchema: the input schema of the original tool named `name` (exactly,
/// else its only canonical match), when unambiguous. `Some(None)` means the tool
/// declares no schema.
fn find_tool_schema<'a>(original: &'a [u8], name: &[u8]) -> Option<Option<Res<'a>>> {
    if name.is_empty() || original.is_empty() || !gj::valid(original) {
        return None;
    }
    let tools = gj::parse(original).get("tools");
    if !tools.is_array() {
        return None;
    }
    let canonical = canonical_tool_name(name);
    let mut exact: Vec<Res<'a>> = vec![];
    let mut canonical_matches: Vec<(Vec<u8>, Vec<Res<'a>>)> = vec![];
    for tool in tools.array() {
        let declared = declared_name(&tool);
        if declared.is_empty() {
            continue;
        }
        let mut schema = tool.get("input_schema");
        if !schema.exists() {
            schema = tool.get("parameters");
        }
        if !schema.exists() {
            schema = tool.get("function.parameters");
        }
        if declared == name {
            exact.push(schema.clone());
        }
        if canonical_tool_name(&declared) == canonical {
            match canonical_matches.iter_mut().find(|(n, _)| *n == declared) {
                Some((_, list)) => list.push(schema),
                None => canonical_matches.push((declared, vec![schema])),
            }
        }
    }
    let identical = |list: &[Res<'a>]| list.iter().all(|s| s.raw == list[0].raw);
    let found = |schema: &Res<'a>| Some(schema.exists().then(|| schema.clone()));
    if !exact.is_empty() {
        return if identical(&exact) { found(&exact[0]) } else { None };
    }
    match canonical_matches.as_slice() {
        [(_, list)] if identical(list) => found(&list[0]),
        _ => None,
    }
}

/// util.ClaudeToolIsParameterless: the original tool declared an empty or missing
/// object schema, so the arguments Claude receives must be `{}`.
pub fn claude_tool_is_parameterless(name: &[u8], original: &[u8]) -> bool {
    let Some(schema) = find_tool_schema(original, name) else {
        return false;
    };
    let Some(schema) = schema else {
        return true;
    };
    let info = analyze(&schema, &schema);
    if info.complex || any_branch_allows_dynamic(&info) || !info.declares_object || !info.forbids_dynamic {
        return false;
    }
    info.required.is_empty() && (!info.has_properties || info.properties.is_empty())
}

/// resolveClaudeArrayItemsSchema: the item schemas of an array schema, and whether
/// they could be resolved deterministically.
fn array_items<'a>(
    schema: &Res<'a>,
    root: &Res<'a>,
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> Option<(Vec<Res<'a>>, bool)> {
    if depth > 16 || !schema.exists() || !schema.is_object() {
        return None;
    }
    let mut items = vec![];
    let mut deterministic = true;
    let reference = schema.get("$ref");
    if reference.exists() {
        let value = reference.bytes().into_owned();
        if visited.contains(&value) {
            return None;
        }
        let target = resolve_local_ref(root, &value)?;
        visited.insert(value.clone());
        let resolved = array_items(&target, root, depth + 1, visited);
        visited.remove(&value);
        match resolved {
            Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
            _ => deterministic = false,
        }
    }
    if schema.get("anyOf").exists() || schema.get("oneOf").exists() {
        match union_branches(schema) {
            Some(branches) if branches.len() == 1 => match array_items(&branches[0], root, depth + 1, visited) {
                Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
                _ => deterministic = false,
            },
            _ => deterministic = false,
        }
    }
    let all_of = schema.get("allOf");
    if all_of.is_array() {
        for branch in all_of.array() {
            match array_items(&branch, root, depth + 1, visited) {
                Some((found, true)) => found.into_iter().for_each(|i| push_unique(&mut items, i)),
                _ => deterministic = false,
            }
        }
    } else if all_of.exists() {
        deterministic = false;
    }
    let direct = schema.get("items");
    if direct.exists() {
        push_unique(&mut items, direct);
    }
    Some((items, deterministic))
}

/// Go's `([]gjson.Result, bool)` from resolveClaudeArrayItemsSchema.
fn resolve_items<'a>(schema: &Res<'a>, root: &Res<'a>) -> (Vec<Res<'a>>, bool) {
    array_items(schema, root, 0, &mut HashSet::new()).unwrap_or((vec![], false))
}

/// joinPath.
fn any_branch_allows_dynamic(info: &Info<'_>) -> bool {
    info.has_pattern_properties
        || info.branch_infos.iter().any(any_branch_allows_dynamic)
        || (info.allows_dynamic && !info.forbids_dynamic)
}

fn carries_required(info: &Info<'_>, current: &impl ObjectKeys) -> bool {
    info.required.iter().all(|name| has_key(current, name))
}

/// claudeBranchMayAccept.
fn branch_may_accept(info: &Info<'_>, key: &[u8], current: &impl ObjectKeys) -> bool {
    if info.has_pattern_properties {
        return true;
    }
    // An open branch accepts the key by default, but only counts when the current
    // object carries every property the branch requires.
    if !info.forbids_dynamic && carries_required(info, current) {
        return true;
    }
    if info.allows_dynamic && !info.forbids_dynamic {
        return true;
    }
    info.branch_infos
        .iter()
        .any(|branch| !branch.properties.contains_key(key) && branch_may_accept(branch, key, current))
}

/// claudeParentBranchAcceptsChild.
fn parent_branch_accepts_child(info: &Info<'_>, key: &[u8], current: &impl ObjectKeys) -> bool {
    info.branch_infos
        .iter()
        .any(|branch| !branch.properties.contains_key(key) && branch_may_accept(branch, key, current))
}

/// claudeCandidateMayPermitChild.
fn candidate_may_permit_child(info: &Info<'_>, key: &[u8], current: &impl ObjectKeys) -> bool {
    if info.properties.contains_key(key) {
        return false;
    }
    if info.has_pattern_properties {
        return true;
    }
    !info.forbids_dynamic && carries_required(info, current)
}

/// claudeOriginalsMayPermitChild.
fn is_empty_schema(info: &Info<'_>) -> bool {
    info.required.is_empty() && (!info.has_properties || info.properties.is_empty())
}

fn clean_object<'a>(
    json: Vec<u8>,
    schemas: &[Res<'a>],
    root: &Res<'a>,
    path: &[u8],
    top_level: bool,
) -> Option<Vec<u8>> {
    clean_iterative(json, None, schemas, root, path, top_level)
}

/// findTranslatedClaudeToolSchemas: the declarations the translated (sent) request
/// gave this tool, matched by the name actually sent when it can be derived.
fn find_translated_schemas<'a>(translated: &'a [u8], name: &[u8], original: &[u8]) -> Vec<Res<'a>> {
    if name.is_empty() || translated.is_empty() || !gj::valid(translated) {
        return vec![];
    }
    let root = gj::parse(translated);
    let mut named: Vec<(Vec<u8>, Res<'a>)> = vec![];
    for tools in [root.get("tools"), root.get("request.tools")] {
        if !tools.is_array() {
            continue;
        }
        for group in tools.array() {
            let declarations = group.get("functionDeclarations");
            if !declarations.is_array() {
                continue;
            }
            for declaration in declarations.array() {
                let declared = trim_space(&declaration.get("name").bytes()).to_vec();
                if declared.is_empty() {
                    continue;
                }
                let mut schema = declaration.get("parametersJsonSchema");
                if !schema.exists() {
                    schema = declaration.get("parameters");
                }
                if schema.exists() && schema.is_object() {
                    named.push((declared, schema));
                }
            }
        }
    }
    if named.is_empty() {
        return vec![];
    }
    let canonical = canonical_tool_name(name);
    let sanitized = common::sanitize_function_name(name);
    let sent = crate::responses_tools::sanitized_function_name_map(original)
        .remove(name)
        .unwrap_or_default();
    let mut exact = vec![];
    let mut by_name: Vec<(Vec<u8>, Vec<Res<'a>>)> = vec![];
    for (declared, schema) in named {
        if declared == name || (!sent.is_empty() && declared == sent) {
            exact.push(schema.clone());
        }
        if canonical_tool_name(&declared) == canonical
            || declared == sanitized
            || common::sanitize_function_name(&declared) == sanitized
        {
            match by_name.iter_mut().find(|(n, _)| *n == declared) {
                Some((_, list)) => list.push(schema),
                None => by_name.push((declared, vec![schema])),
            }
        }
    }
    if !exact.is_empty() {
        return exact;
    }
    match by_name.len() {
        1 => by_name.pop().map(|(_, list)| list).unwrap_or_default(),
        _ => vec![],
    }
}

/// claudeOriginalDeclaresPropertyDeep.
fn declares_property_deep(
    schema: &Res<'_>,
    root: &Res<'_>,
    property: &[u8],
    depth: usize,
    visited: &mut HashSet<Vec<u8>>,
) -> bool {
    if depth > 16 || !schema.exists() || !schema.is_object() {
        return false;
    }
    let reference = schema.get("$ref");
    if reference.exists() {
        let value = reference.bytes().into_owned();
        if !visited.contains(&value)
            && let Some(target) = resolve_local_ref(root, &value)
        {
            visited.insert(value.clone());
            let declared = declares_property_deep(&target, root, property, depth + 1, visited);
            visited.remove(&value);
            if declared {
                return true;
            }
        }
    }
    if has_key(&schema.get("properties"), property)
        || schema.get("required").array().iter().any(|r| &*r.bytes() == property)
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].iter().any(|keyword| {
        schema
            .get(*keyword)
            .array()
            .iter()
            .any(|branch| declares_property_deep(branch, root, property, depth + 1, visited))
    })
}

fn requires(schema: &Res<'_>, name: &[u8]) -> bool {
    schema.get("required").array().iter().any(|r| &*r.bytes() == name)
}

/// Whether the translated schemas show that `placeholder` was injected for this object.
fn placeholder_injected(placeholder: &[u8], translated: &[Res<'_>], top_level: bool, forbids: bool) -> bool {
    for schema in translated {
        let info = analyze(schema, schema);
        let Some(placeholder_schemas) = info.properties.get(placeholder) else {
            continue;
        };
        if placeholder == b"reason" {
            if requires(schema, b"reason")
                || placeholder_schemas
                    .iter()
                    .any(|p| &*p.get("description").bytes() == PLACEHOLDER_REASON_DESCRIPTION)
            {
                return true;
            }
        } else {
            if requires(schema, b"_") {
                return true;
            }
            // Without `required` (dropped by branch flattening), a boolean `_` marks the
            // injection, but only for nested objects.
            if !top_level
                && placeholder_schemas
                    .iter()
                    .any(|p| &*p.get("type").bytes() == b"boolean")
            {
                return true;
            }
        }
    }
    // Union branches: the original declares the field nowhere, so a required
    // occurrence in a sent branch is the injected one.
    for schema in translated {
        for keyword in ["anyOf", "oneOf"] {
            for branch in schema.get(keyword).array() {
                if analyze(&branch, &branch).properties.contains_key(placeholder) && requires(&branch, placeholder) {
                    return true;
                }
            }
        }
    }
    // The Antigravity executor re-cleans wrapped tool schemas after translation and can
    // inject placeholders the translated request does not show yet; simulated only for
    // closed objects, where the key cannot be legitimate user data.
    if top_level && forbids {
        for schema in translated {
            let mut wrapped = b"{}".to_vec();
            if !gj::set_raw(&mut wrapped, "schema", &schema.raw) {
                continue;
            }
            let recleaned_doc = cpa_common::gemini_schema::for_antigravity(&wrapped);
            let recleaned = gj::get(&recleaned_doc, "schema");
            if !recleaned.exists() {
                continue;
            }
            if analyze(&recleaned, &recleaned).properties.contains_key(placeholder) && requires(&recleaned, placeholder)
            {
                return true;
            }
        }
    }
    false
}

fn clean_by_translated<'a>(
    json: Vec<u8>,
    translated: &[Res<'a>],
    originals: &[Res<'a>],
    root: &Res<'a>,
    path: &[u8],
    top_level: bool,
) -> Option<Vec<u8>> {
    clean_iterative(json, Some(translated), originals, root, path, top_level)
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SchemaIdentity(usize, usize, usize, usize);
fn schema_identity(schema: &Res<'_>, root: &Res<'_>) -> SchemaIdentity {
    SchemaIdentity(
        schema.raw.as_ptr() as usize,
        schema.raw.len(),
        root.raw.as_ptr() as usize,
        root.raw.len(),
    )
}
#[derive(Default)]
struct SchemaCache<'a> {
    analyzed: HashMap<SchemaIdentity, std::sync::Arc<Info<'a>>>,
    merged: HashMap<Vec<SchemaIdentity>, std::sync::Arc<Info<'a>>>,
    items: HashMap<SchemaIdentity, (std::sync::Arc<[Res<'a>]>, bool)>,
    injected: HashMap<(Vec<SchemaIdentity>, Vec<u8>, bool, bool), bool>,
    declared: HashMap<(SchemaIdentity, Vec<u8>), bool>,
}
impl<'a> SchemaCache<'a> {
    fn analyze(&mut self, schema: &Res<'a>, root: &Res<'a>) -> std::sync::Arc<Info<'a>> {
        self.analyzed
            .entry(schema_identity(schema, root))
            .or_insert_with(|| std::sync::Arc::new(analyze(schema, root)))
            .clone()
    }
    fn merged(&mut self, schemas: &[Res<'a>], root: &Res<'a>) -> std::sync::Arc<Info<'a>> {
        let key: Vec<_> = schemas.iter().map(|s| schema_identity(s, root)).collect();
        if let Some(info) = self.merged.get(&key) {
            return info.clone();
        }
        let mut info = Info::default();
        for schema in schemas {
            merge(&mut info, (*self.analyze(schema, root)).clone());
        }
        let info = std::sync::Arc::new(info);
        self.merged.insert(key, info.clone());
        info
    }
    fn items(&mut self, schema: &Res<'a>, root: &Res<'a>) -> (std::sync::Arc<[Res<'a>]>, bool) {
        self.items
            .entry(schema_identity(schema, root))
            .or_insert_with(|| {
                let (items, ok) = resolve_items(schema, root);
                (items.into(), ok)
            })
            .clone()
    }
    fn allows_dynamic(&mut self, schemas: &[Res<'a>], root: &Res<'a>) -> bool {
        schemas
            .iter()
            .any(|s| any_branch_allows_dynamic(&self.analyze(s, root)))
    }
    fn all_object(&mut self, schemas: &[Res<'a>], root: &Res<'a>) -> bool {
        schemas.iter().all(|s| self.analyze(s, root).declares_object)
    }
    fn accepts_implicitly(&mut self, schemas: &[Res<'a>], root: &Res<'a>, current: &impl ObjectKeys) -> bool {
        schemas.len() > 1
            && schemas.iter().any(|s| {
                let info = self.analyze(s, root);
                !info.complex && !info.forbids_dynamic && carries_required(&info, current)
            })
    }
    fn permits_child(&mut self, schemas: &[Res<'a>], root: &Res<'a>, key: &[u8], current: &impl ObjectKeys) -> bool {
        schemas.len() > 1
            && schemas.iter().any(|s| {
                let info = self.analyze(s, root);
                !info.complex && candidate_may_permit_child(&info, key, current)
            })
    }
    fn declares(&mut self, schemas: &[Res<'a>], root: &Res<'a>, key: &[u8]) -> bool {
        schemas.iter().any(|s| {
            *self
                .declared
                .entry((schema_identity(s, root), key.to_vec()))
                .or_insert_with(|| declares_property_deep(s, root, key, 0, &mut HashSet::new()))
        })
    }
    fn injected(&mut self, key: &[u8], schemas: &[Res<'a>], top: bool, forbids: bool) -> bool {
        let identity = schemas.iter().map(|s| schema_identity(s, s)).collect();
        *self
            .injected
            .entry((identity, key.to_vec(), top, forbids))
            .or_insert_with(|| placeholder_injected(key, schemas, top, forbids))
    }
}

#[derive(Default)]
struct ArgumentNode {
    start: usize,
    end: usize,
    object: bool,
    array: bool,
    members: Vec<ArgumentMember>,
    names: HashMap<Vec<u8>, Vec<usize>>,
}
struct ArgumentMember {
    key: Vec<u8>,
    key_start: usize,
    child: usize,
}
struct ArgumentDocument<'a> {
    raw: &'a [u8],
    nodes: Vec<ArgumentNode>,
    deleted: HashMap<usize, HashSet<usize>>,
    deletion_order: HashMap<usize, Vec<usize>>,
    replacements: HashMap<usize, Vec<u8>>,
}
impl<'a> ArgumentDocument<'a> {
    fn new(raw: &'a [u8]) -> Self {
        struct Frame {
            node: usize,
            pending: Option<(Vec<u8>, usize, usize)>,
        }
        let mut nodes = Vec::<ArgumentNode>::new();
        let mut stack = Vec::<Frame>::new();
        let mut i = 0;
        while i < raw.len() {
            let start = i;
            match raw[i] {
                b' ' | b'\t' | b'\r' | b'\n' | b':' => {
                    i += 1;
                    continue;
                }
                b',' => {
                    i += 1;
                    continue;
                }
                b'}' | b']' => {
                    let frame = stack.pop().expect("validated JSON container");
                    nodes[frame.node].end = i + 1;
                    i += 1;
                    continue;
                }
                b'"' => {
                    i += 1;
                    while i < raw.len() {
                        match raw[i] {
                            b'\\' => i += 2,
                            b'"' => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                    if let Some(frame) = stack.last_mut()
                        && nodes[frame.node].object
                        && frame.pending.is_none()
                    {
                        frame.pending = Some((gj::parse(&raw[start..i]).bytes().into_owned(), start, i));
                        continue;
                    }
                }
                b'{' | b'[' => {
                    i += 1;
                }
                _ => {
                    while i < raw.len() && !matches!(raw[i], b',' | b'}' | b']' | b' ' | b'\r' | b'\n' | b'\t') {
                        i += 1;
                    }
                }
            }
            let id = nodes.len();
            let object = raw[start] == b'{';
            let array = raw[start] == b'[';
            nodes.push(ArgumentNode {
                start,
                end: i,
                object,
                array,
                ..ArgumentNode::default()
            });
            if let Some(frame) = stack.last_mut() {
                let parent = &mut nodes[frame.node];
                let (key, key_start, _key_end) = frame.pending.take().unwrap_or_default();
                if parent.object {
                    parent.names.entry(key.clone()).or_default().push(parent.members.len());
                }
                parent.members.push(ArgumentMember {
                    key,
                    key_start,
                    child: id,
                });
            }
            if object || array {
                stack.push(Frame {
                    node: id,
                    pending: None,
                });
            }
        }
        Self {
            raw,
            nodes,
            deleted: HashMap::new(),
            deletion_order: HashMap::new(),
            replacements: HashMap::new(),
        }
    }
    fn object(&self, node: usize) -> bool {
        self.nodes[node].object
    }
    fn array(&self, node: usize) -> bool {
        self.nodes[node].array
    }
    fn current(&self, node: usize) -> ArgumentKeys<'_, 'a> {
        ArgumentKeys { document: self, node }
    }
    fn child(&self, node: usize, key: &[u8]) -> Option<usize> {
        self.nodes[node]
            .names
            .get(key)
            .and_then(|items| {
                items
                    .iter()
                    .find(|i| self.deleted.get(&node).is_none_or(|d| !d.contains(i)))
            })
            .map(|i| self.nodes[node].members[*i].child)
    }
    fn ambiguous_prefix(&self, node: usize, key: &[u8]) -> bool {
        let Some(items) = self.nodes[node].names.get(key) else {
            return false;
        };
        items.len() > 1
            && items
                .iter()
                .filter(|i| {
                    let child = self.nodes[node].members[**i].child;
                    self.object(child) || self.array(child)
                })
                .count()
                > 1
    }
    fn remove(&mut self, node: usize, key: &[u8]) {
        let Some(index) = self.nodes[node]
            .names
            .get(key)
            .and_then(|items| {
                items
                    .iter()
                    .find(|i| self.deleted.get(&node).is_none_or(|d| !d.contains(i)))
            })
            .copied()
        else {
            return;
        };
        if self.deleted.entry(node).or_default().insert(index) {
            self.deletion_order.entry(node).or_default().push(index);
        }
    }
    fn empty(&mut self, node: usize) {
        self.replacements.insert(node, b"{}".to_vec());
    }
    fn finish(self) -> Vec<u8> {
        let mut edits: Vec<(usize, usize, Vec<u8>)> = self
            .replacements
            .into_iter()
            .map(|(id, raw)| (self.nodes[id].start, self.nodes[id].end, raw))
            .collect();
        for (id, order) in self.deletion_order {
            let node = &self.nodes[id];
            let mut removed: Vec<(usize, usize)> = Vec::new();
            for index in order {
                let member = &node.members[index];
                let mut cursor = member.key_start;
                let start = loop {
                    cursor -= 1;
                    if let Some((start, _)) = removed.iter().find(|(a, b)| *a <= cursor && cursor < *b) {
                        cursor = *start;
                        continue;
                    }
                    match self.raw[cursor] {
                        b',' => break cursor,
                        b'{' => break cursor + 1,
                        _ => {}
                    }
                };
                let mut end = self.nodes[member.child].end;
                if start == node.start + 1 {
                    let mut after = end;
                    loop {
                        if let Some((_, end)) = removed.iter().find(|(a, b)| *a <= after && after < *b) {
                            after = *end;
                            continue;
                        }
                        if after < self.raw.len() && self.raw[after] <= b' ' {
                            after += 1;
                            continue;
                        }
                        if self.raw.get(after) == Some(&b',') {
                            end = after + 1;
                        }
                        break;
                    }
                }
                removed.push((start, end));
            }
            removed.sort_unstable();
            let mut combined: Vec<(usize, usize)> = Vec::new();
            for (start, end) in removed {
                if let Some((_, last)) = combined.last_mut()
                    && start <= *last
                {
                    *last = (*last).max(end);
                } else {
                    combined.push((start, end));
                }
            }
            edits.extend(combined.into_iter().map(|(start, end)| (start, end, vec![])));
        }
        let mut starts = vec![false; self.raw.len() + 1];
        let mut by_start: HashMap<usize, (usize, Vec<u8>)> = HashMap::with_capacity(edits.len());
        for (start, end, replacement) in edits {
            starts[start] = true;
            let entry = by_start.entry(start).or_insert((end, replacement.clone()));
            if end > entry.0 {
                *entry = (end, replacement);
            }
        }
        let mut output = Vec::with_capacity(self.raw.len());
        let mut cursor = 0;
        let mut copied = 0;
        while cursor < self.raw.len() {
            if starts[cursor] {
                let (end, replacement) = by_start.remove(&cursor).unwrap();
                output.extend_from_slice(&self.raw[copied..cursor]);
                output.extend_from_slice(&replacement);
                cursor = end;
                copied = end;
            } else {
                cursor += 1;
            }
        }
        output.extend_from_slice(&self.raw[copied..]);
        let parsed = gj::parse(&output);
        let mut empty = parsed.is_object();
        parsed.each(|_, _| {
            empty = false;
            false
        });
        if empty { b"{}".to_vec() } else { output }
    }
}
struct ArgumentKeys<'d, 'a> {
    document: &'d ArgumentDocument<'a>,
    node: usize,
}
impl ObjectKeys for ArgumentKeys<'_, '_> {
    fn contains_key(&self, key: &[u8]) -> bool {
        self.document.child(self.node, key).is_some()
    }
}

fn argument_operator_path(key: &[u8]) -> bool {
    if key.is_empty() || key.first() == Some(&b':') || key.contains(&b'|') || key.contains(&b'\\') {
        return true;
    }
    let modifier = key
        .strip_prefix(b"@")
        .map(|k| k.split(|c| *c == b':').next().unwrap_or_default());
    modifier.is_some_and(|m| {
        matches!(
            m,
            b"pretty"
                | b"ugly"
                | b"reverse"
                | b"this"
                | b"flatten"
                | b"join"
                | b"valid"
                | b"keys"
                | b"values"
                | b"tostr"
                | b"fromstr"
                | b"group"
                | b"dig"
        )
    })
}

fn clean_iterative<'a>(
    json: Vec<u8>,
    translated: Option<&[Res<'a>]>,
    originals: &[Res<'a>],
    root: &Res<'a>,
    _path: &[u8],
    top: bool,
) -> Option<Vec<u8>> {
    use std::sync::Arc;
    type Schemas<'a> = Arc<[Res<'a>]>;
    struct Object<'a> {
        schemas: Schemas<'a>,
        translated: Option<Schemas<'a>>,
        node: usize,
        top: bool,
        complex: bool,
        info: Option<Arc<Info<'a>>>,
        keys: Vec<Vec<u8>>,
        next: usize,
    }
    enum Work<'a> {
        Value(Schemas<'a>, Option<Schemas<'a>>, usize, bool, bool),
        Object(Object<'a>),
        Array(Schemas<'a>, Option<Schemas<'a>>, usize, bool),
    }
    let mut cache = SchemaCache::default();
    let mut document = ArgumentDocument::new(&json);
    let mut work = vec![Work::Value(
        Arc::from(originals),
        translated.map(Arc::from),
        0,
        top,
        false,
    )];
    'work: while let Some(task) = work.pop() {
        match task {
            Work::Value(schemas, translated, node, top, complex) => {
                if document.object(node) {
                    work.push(Work::Object(Object {
                        schemas,
                        translated,
                        node,
                        top,
                        complex,
                        info: None,
                        keys: vec![],
                        next: 0,
                    }));
                } else if document.array(node) {
                    work.push(Work::Array(schemas, translated, node, complex));
                }
            }
            Work::Array(schemas, translated, node, complex) => {
                if schemas.iter().any(|schema| cache.analyze(schema, root).unsafe_complex) {
                    continue;
                }
                let mut items = vec![];
                let mut translated_items = vec![];
                let mut deterministic = true;
                for schema in schemas.iter() {
                    let (found, ok) = cache.items(schema, root);
                    deterministic &= ok;
                    if ok && found.is_empty() {
                        push_unique(&mut items, schema.clone());
                    } else {
                        for item in found.iter().cloned() {
                            push_unique(&mut items, item);
                        }
                    }
                }
                if let Some(translated) = &translated {
                    for schema in translated.iter() {
                        let (found, ok) = cache.items(schema, schema);
                        deterministic &= ok;
                        for item in found.iter().cloned() {
                            push_unique(&mut translated_items, item);
                        }
                    }
                }
                if !deterministic || items.is_empty() || (translated.is_some() && translated_items.is_empty()) {
                    continue;
                }
                let items: Schemas<'a> = Arc::from(items);
                let translated_items = translated.map(|_| Arc::from(translated_items));
                let count = document.nodes[node].members.len();
                for index in (0..count).rev() {
                    work.push(Work::Value(
                        items.clone(),
                        translated_items.clone(),
                        document.nodes[node].members[index].child,
                        false,
                        complex,
                    ));
                }
            }
            Work::Object(mut state) => {
                if state.info.is_none() {
                    let args = document.current(state.node);
                    if !document.object(state.node) || state.schemas.is_empty() {
                        continue;
                    }
                    let info = cache.merged(&state.schemas, root);
                    if let Some(translated) = &state.translated {
                        if translated.is_empty()
                            || state.schemas.iter().any(|s| !s.is_object())
                            || info.unsafe_complex
                            || (info.complex
                                && !state
                                    .schemas
                                    .iter()
                                    .any(|s| s.get("anyOf").is_array() || s.get("oneOf").is_array()))
                        {
                            continue;
                        }
                    } else {
                        if info.complex || !info.declares_object {
                            continue;
                        }
                        if is_empty_schema(&info)
                            && cache.all_object(&state.schemas, root)
                            && info.forbids_dynamic
                            && !any_branch_allows_dynamic(&info)
                            && !cache.allows_dynamic(&state.schemas, root)
                            && !cache.accepts_implicitly(&state.schemas, root, &args)
                        {
                            if !document.nodes[state.node].members.is_empty() {
                                document.empty(state.node);
                            }
                            continue;
                        }
                    }
                    let mut seen = HashSet::new();
                    state.keys = document.nodes[state.node]
                        .members
                        .iter()
                        .filter(|member| info.properties.contains_key(&member.key) && seen.insert(member.key.clone()))
                        .map(|member| member.key.clone())
                        .collect();
                    state.info = Some(info);
                }
                let info = state.info.as_ref().unwrap();
                while state.next < state.keys.len() {
                    let key = &state.keys[state.next];
                    state.next += 1;
                    let Some(child) = document.child(state.node, key) else {
                        continue;
                    };
                    let current = document.current(state.node);
                    if parent_branch_accepts_child(info, key, &current)
                        || cache.permits_child(&state.schemas, root, key, &current)
                    {
                        continue;
                    }
                    let children = info.properties[key].clone();
                    let mut translated_children = vec![];
                    if let Some(translated) = &state.translated {
                        for schema in translated.iter() {
                            if let Some(found) = cache.analyze(schema, schema).properties.get(key) {
                                for child in found {
                                    push_unique(&mut translated_children, child.clone());
                                }
                            }
                        }
                        if children.is_empty() || translated_children.is_empty() {
                            continue;
                        }
                    }
                    if !document.object(child) && !document.array(child) {
                        continue;
                    }
                    if (document.object(child) || document.array(child))
                        && (document.ambiguous_prefix(state.node, key) || argument_operator_path(key))
                    {
                        // ponytail: ambiguous duplicate prefixes and operator paths retain the whole original argument document.
                        // Upgrade by indexing gjson read candidates and sjson write targets separately.
                        return None;
                    }
                    let translated_children = state.translated.as_ref().map(|_| Arc::from(translated_children));
                    let complex = state.complex || key.iter().any(|c| matches!(c, b'@' | b'#' | b'|'));
                    work.push(Work::Object(state));
                    work.push(Work::Value(
                        Arc::from(children),
                        translated_children,
                        child,
                        false,
                        complex,
                    ));
                    continue 'work;
                }
                if state.translated.is_none() && any_branch_allows_dynamic(info) {
                    continue;
                }
                if !document.object(state.node) {
                    continue;
                }
                for placeholder in [&b"reason"[..], b"_"] {
                    let current = document.current(state.node);
                    if info.properties.contains_key(placeholder)
                        || info.required.contains(placeholder)
                        || any_branch_allows_dynamic(info)
                        || cache.allows_dynamic(&state.schemas, root)
                        || cache.accepts_implicitly(&state.schemas, root, &current)
                        || parent_branch_accepts_child(info, placeholder, &current)
                        || !has_key(&current, placeholder)
                    {
                        continue;
                    }
                    if let Some(translated) = &state.translated
                        && (cache.declares(&state.schemas, root, placeholder)
                            || !cache.injected(placeholder, translated, state.top, info.forbids_dynamic))
                    {
                        continue;
                    }
                    if !state.complex {
                        document.remove(state.node, placeholder);
                    }
                }
                let current = document.current(state.node);
                if state.translated.is_some()
                    && !info.complex
                    && cache.all_object(&state.schemas, root)
                    && info.forbids_dynamic
                    && !any_branch_allows_dynamic(info)
                    && !cache.allows_dynamic(&state.schemas, root)
                    && !cache.accepts_implicitly(&state.schemas, root, &current)
                    && info.declares_object
                    && is_empty_schema(info)
                {
                    document.empty(state.node);
                }
            }
        }
    }
    Some(document.finish())
}

/// The open streamed tool_use block (CurrentToolName, CurrentToolIsParameterless,
/// CurrentToolInputEmitted in Go's Gemini and Antigravity Claude response params).
#[derive(Default)]
pub(crate) struct OpenTool {
    pub name: Vec<u8>,
    parameterless: bool,
    emitted: bool,
    pending_empty: bool,
}

impl OpenTool {
    pub fn open(&mut self, name: &[u8], original: &[u8]) {
        self.name = name.to_vec();
        self.parameterless = claude_tool_is_parameterless(name, original);
        self.emitted = false;
        self.pending_empty = false;
    }

    /// The cleaned `partial_json` for streamed arguments: a parameterless tool emits its
    /// input once, and later chunks emit an empty string.
    pub fn input(&mut self, args: &[u8], original: &[u8], translated: &[u8]) -> Option<Vec<u8>> {
        let mut partial = clean_claude_tool_arguments_with_translated(args, &self.name, original, translated);
        if self.parameterless {
            if self.emitted {
                partial.clear();
            }
            self.emitted = true;
            self.pending_empty = false;
        } else if partial == b"{}" {
            if !self.emitted {
                self.pending_empty = true;
                return None;
            }
            partial.clear();
        } else if !partial.is_empty() {
            self.emitted = true;
            self.pending_empty = false;
        }
        Some(partial)
    }
    pub fn finish(&mut self) -> Option<&'static [u8]> {
        if self.pending_empty && !self.emitted {
            self.pending_empty = false;
            self.emitted = true;
            Some(b"{}")
        } else {
            None
        }
    }
}

/// util.CleanClaudeToolArgumentsWithTranslated: placeholders are removed only where
/// the translated (sent) schema shows they were injected; without a translated schema,
/// [`clean_claude_tool_arguments`]. Parameterless tools always get `{}`.
pub fn clean_claude_tool_arguments_with_translated(
    raw: &[u8],
    name: &[u8],
    original: &[u8],
    translated: &[u8],
) -> Vec<u8> {
    let schemas = find_translated_schemas(translated, name, original);
    if schemas.is_empty() {
        return clean_claude_tool_arguments(raw, name, original);
    }
    let original_raw = raw;
    let raw = trim_space(raw);
    if raw.is_empty() || raw == b"{}" {
        return b"{}".to_vec();
    }
    if !gj::valid(raw) || !gj::parse(raw).is_object() {
        return raw.to_vec();
    }
    match find_tool_schema(original, name) {
        None => raw.to_vec(),
        Some(None) => b"{}".to_vec(),
        Some(Some(schema)) => clean_by_translated(
            raw.to_vec(),
            &schemas,
            std::slice::from_ref(&schema),
            &schema,
            b"",
            true,
        )
        .unwrap_or_else(|| original_raw.to_vec()),
    }
}

/// util.CleanClaudeToolArguments: placeholders injected for empty object schemas are
/// removed, judged from the original declaration; parameterless tools get `{}`.
pub fn clean_claude_tool_arguments(raw: &[u8], name: &[u8], original: &[u8]) -> Vec<u8> {
    let original_raw = raw;
    let raw = trim_space(raw);
    if raw.is_empty() || raw == b"{}" {
        return b"{}".to_vec();
    }
    if !gj::valid(raw) {
        return raw.to_vec();
    }
    let schema = match find_tool_schema(original, name) {
        None => return raw.to_vec(),
        Some(None) => return b"{}".to_vec(),
        Some(Some(schema)) => schema,
    };
    if !gj::parse(raw).is_object() {
        return raw.to_vec();
    }
    let info = analyze(&schema, &schema);
    if info.complex || !info.declares_object {
        return raw.to_vec();
    }
    if is_empty_schema(&info) && info.forbids_dynamic && !any_branch_allows_dynamic(&info) {
        return b"{}".to_vec();
    }
    clean_object(raw.to_vec(), std::slice::from_ref(&schema), &schema, b"", true)
        .unwrap_or_else(|| original_raw.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argument_cleanup_handles_depth_on_small_stacks() {
        const FLAG: &str = "CPA_ARGUMENT_SMALL_STACK";
        if std::env::var_os(FLAG).is_some() {
            std::thread::Builder::new().stack_size(128*1024).spawn(|| {
                let original = br##"{"tools":[{"name":"t","input_schema":{"$ref":"#/$defs/node","$defs":{"node":{"type":"object","properties":{"next":{"$ref":"#/$defs/node"}},"additionalProperties":false}}}}]}"##;
                let depth = std::env::var("CPA_ARGUMENT_DEPTH").ok().and_then(|v|v.parse::<usize>().ok()).unwrap_or(256);
                let raw = format!("{}{{\"reason\":\"synthetic\"}}{}", "{\"next\":".repeat(depth), "}".repeat(depth));
                let expected = format!("{}{{}}{}", "{\"next\":".repeat(depth), "}".repeat(depth));
                assert!(clean_claude_tool_arguments(raw.as_bytes(),b"t",original) == expected.as_bytes());
            }).unwrap().join().unwrap();
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "claude_tool_args::tests::argument_cleanup_handles_depth_on_small_stacks",
                "--nocapture",
            ])
            .env(FLAG, "1")
            .status()
            .unwrap();
        assert!(status.success(), "argument subprocess failed: {status}");
    }

    #[test]
    fn schema_analysis_retains_go_sixteen_level_ceiling() {
        for depth in [16, 17] {
            let schema = format!(
                "{}{{\"type\":\"object\",\"additionalProperties\":false}}{}",
                "{\"allOf\":[".repeat(depth),
                "]}".repeat(depth)
            );
            let original = format!(r#"{{"tools":[{{"name":"t","input_schema":{schema}}}]}}"#);
            assert_eq!(claude_tool_is_parameterless(b"t", original.as_bytes()), depth == 16);
            let want = if depth == 16 {
                b"{}".as_slice()
            } else {
                br#"{"reason":"keep"}"#
            };
            assert_eq!(
                clean_claude_tool_arguments(br#"{"reason":"keep"}"#, b"t", original.as_bytes()),
                want
            );
        }
    }

    #[test]
    fn ambiguous_argument_paths_preserve_the_whole_original_document() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/claude_arguments.json")).unwrap();
        let mut checked = 0;
        for case in cases {
            if case["fork"] != "passthrough" {
                continue;
            }
            let bytes = |key: &str| case[key].as_str().unwrap().as_bytes();
            assert_ne!(bytes("raw"), bytes("want"), "Go edge answer must remain recorded");
            assert_eq!(
                clean_claude_tool_arguments_with_translated(
                    bytes("raw"),
                    bytes("tool"),
                    bytes("original"),
                    bytes("translated")
                ),
                bytes("raw"),
                "{}",
                case["name"]
            );
            let padded = [b" \n".as_slice(), bytes("raw"), b"\t "].concat();
            assert_eq!(
                clean_claude_tool_arguments_with_translated(
                    &padded,
                    bytes("tool"),
                    bytes("original"),
                    bytes("translated")
                ),
                padded
            );
            checked += 1;
        }
        assert!(checked >= 2);
    }

    #[test]
    #[ignore = "manual argument-width scaling probe"]
    fn argument_cleanup_scaling_probe() {
        let count = std::env::var("CPA_ARGUMENT_COUNT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1000);
        let width = std::env::var("CPA_ARGUMENT_WIDTH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1000);
        let properties = (0..width)
            .map(|i| format!(r#""k{i}":{{"type":"string"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let original = format!(
            r#"{{"tools":[{{"name":"t","input_schema":{{"type":"object","properties":{{"rows":{{"type":"array","items":{{"type":"object","properties":{{{properties}}},"additionalProperties":false}}}}}}}}}}]}}"#
        );
        let raw = format!(r#"{{"rows":[{}]}}"#, vec![r#"{"reason":"synthetic"}"#; count].join(","));
        let want = format!(r#"{{"rows":[{}]}}"#, vec!["{}"; count].join(","));
        let started = std::time::Instant::now();
        let got = clean_claude_tool_arguments(raw.as_bytes(), b"t", original.as_bytes());
        println!(
            "count={count} width={width} elapsed_us={}",
            started.elapsed().as_micros()
        );
        assert!(got == want.as_bytes());
    }

    #[test]
    fn matches_final_go_argument_cleaner() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/claude_arguments.json")).unwrap();
        assert!(cases.len() >= 71);
        for case in cases {
            if case["fork"] == "passthrough" {
                continue;
            }
            let bytes = |key: &str| case[key].as_str().unwrap().as_bytes();
            assert_eq!(
                clean_claude_tool_arguments_with_translated(
                    bytes("raw"),
                    bytes("tool"),
                    bytes("original"),
                    bytes("translated")
                ),
                bytes("want"),
                "{}",
                case["name"]
            );
        }
    }
}
