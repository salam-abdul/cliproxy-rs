//! Fixed semantic passes over a literal-key arena. Small-to-large structural merges
//! move map entries O(n log n) in the worst case; rendering also costs generated output.
//! Growing conjunctions move containers, and union descriptions accumulate until rendering.
use super::*;
use crate::schema_path::Document;
use std::collections::HashSet;
use std::collections::VecDeque;

type Id = usize;
type SchemaEdge = (Id, Option<(Id, Vec<u8>)>);

struct Description {
    parts: VecDeque<Vec<u8>>,
    len: usize,
    hints: HashSet<Vec<u8>>,
}

impl Description {
    fn new(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            parts: VecDeque::from([bytes]),
            len,
            hints: HashSet::new(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.len);
        for part in &self.parts {
            output.extend_from_slice(part);
        }
        output
    }

    fn prepend(&mut self, parent: Self) {
        if parent.len == 0 {
            return;
        }
        if self.len == 0 || (self.len == parent.len && self.bytes() == parent.bytes()) {
            *self = parent;
            return;
        }
        self.len += parent.len + 3;
        self.parts.push_front(b" (".to_vec());
        self.parts.push_back(b")".to_vec());
        for part in parent.parts.into_iter().rev() {
            self.parts.push_front(part);
        }
        self.hints.extend(parent.hints);
    }

    fn hint(&mut self, hint: &[u8]) {
        if !self.hints.insert(hint.to_vec()) || (self.len == hint.len() && self.bytes() == hint) {
            return;
        }
        if self.len > 0 {
            self.parts.push_back(b" (".to_vec());
            self.parts.push_back(hint.to_vec());
            self.parts.push_back(b")".to_vec());
            self.len += hint.len() + 3;
        } else {
            self.parts.push_back(hint.to_vec());
            self.len = hint.len();
        }
    }
}

#[derive(Clone)]
struct Node {
    raw: Option<Vec<u8>>,
    object: bool,
    children: Vec<(Vec<u8>, Option<Id>)>,
    fields: HashMap<Vec<u8>, usize>,
}

struct Tree {
    nodes: Vec<Node>,
    required_names: HashMap<Id, HashSet<Vec<u8>>>,
    descriptions: HashMap<Id, Description>,
}

impl Tree {
    fn parse(raw: &[u8]) -> Option<Self> {
        let document = Document::parse(raw)?;
        let mut nodes = Vec::with_capacity(document.nodes.len());
        for node in document.nodes {
            let first = raw[node.span.start];
            let object = first == b'{';
            let mut fields = HashMap::new();
            let mut children: Vec<(Vec<u8>, Option<Id>)> = Vec::new();
            for (key, child) in node.children {
                if object && let Some(slot) = fields.get(&key).copied() {
                    children[slot] = (key, Some(child));
                } else {
                    if object {
                        fields.insert(key.clone(), children.len());
                    }
                    children.push((key, Some(child)));
                }
            }
            nodes.push(Node {
                raw: (!matches!(first, b'{' | b'[')).then(|| raw[node.span].to_vec()),
                object,
                children,
                fields,
            });
        }
        Some(Self {
            nodes,
            required_names: HashMap::new(),
            descriptions: HashMap::new(),
        })
    }

    fn field(&self, node: Id, key: &[u8]) -> Option<Id> {
        let node = &self.nodes[node];
        node.fields.get(key).and_then(|slot| node.children[*slot].1)
    }

    fn fields(&self, node: Id) -> Vec<(Vec<u8>, Id)> {
        self.nodes[node]
            .children
            .iter()
            .filter_map(|(key, value)| value.map(|value| (key.clone(), value)))
            .collect()
    }

    fn object(&self, node: Id) -> bool {
        self.nodes[node].object && self.nodes[node].raw.is_none()
    }

    fn array(&self, node: Id) -> bool {
        !self.nodes[node].object && self.nodes[node].raw.is_none()
    }

    fn items(&self, node: Id) -> Vec<Id> {
        self.nodes[node]
            .children
            .iter()
            .filter_map(|(_, value)| *value)
            .collect()
    }

    fn bytes(&self, node: Id) -> Vec<u8> {
        match &self.nodes[node].raw {
            Some(raw) => gj::parse(raw).bytes().into_owned(),
            None => self.render(node),
        }
    }

    fn text(&self, node: Id, key: &[u8]) -> Vec<u8> {
        self.field(node, key).map(|child| self.bytes(child)).unwrap_or_default()
    }

    fn truth(&self, node: Id, key: &[u8]) -> bool {
        self.field(node, key)
            .is_some_and(|child| self.nodes[child].raw.as_deref() == Some(b"true"))
    }

    fn add_raw(&mut self, raw: Vec<u8>) -> Id {
        let id = self.nodes.len();
        self.nodes.push(Node {
            raw: Some(raw),
            object: false,
            children: Vec::new(),
            fields: HashMap::new(),
        });
        id
    }

    fn add_object(&mut self) -> Id {
        let id = self.nodes.len();
        self.nodes.push(Node {
            raw: None,
            object: true,
            children: Vec::new(),
            fields: HashMap::new(),
        });
        id
    }

    fn add_array(&mut self, values: Vec<Id>) -> Id {
        let id = self.nodes.len();
        self.nodes.push(Node {
            raw: None,
            object: false,
            children: values.into_iter().map(|value| (Vec::new(), Some(value))).collect(),
            fields: HashMap::new(),
        });
        id
    }

    fn set(&mut self, node: Id, key: &[u8], value: Id) {
        let node = &mut self.nodes[node];
        if let Some(slot) = node.fields.get(key).copied() {
            node.children[slot].1 = Some(value);
        } else {
            node.fields.insert(key.to_vec(), node.children.len());
            node.children.push((key.to_vec(), Some(value)));
        }
    }

    fn set_raw(&mut self, node: Id, key: &[u8], raw: Vec<u8>) {
        let value = self.add_raw(raw);
        self.set(node, key, value);
    }

    fn set_text(&mut self, node: Id, key: &[u8], text: &[u8]) {
        self.set_raw(node, key, gj::stringify(text));
    }

    fn remove(&mut self, node: Id, key: &[u8]) {
        if let Some(slot) = self.nodes[node].fields.get(key).copied() {
            self.nodes[node].children[slot].1 = None;
        }
    }

    fn replace(&mut self, node: Id, source: Id) {
        if node == source {
            return;
        }
        let empty = Node {
            raw: Some(b"null".to_vec()),
            object: false,
            children: Vec::new(),
            fields: HashMap::new(),
        };
        let value = std::mem::replace(&mut self.nodes[source], empty);
        self.nodes[node] = value;
    }

    fn clone_tree(&mut self, root: Id) -> Id {
        let mut work = vec![(root, false)];
        let mut result = Vec::new();
        while let Some((node, finish)) = work.pop() {
            if finish {
                let fields = self.fields(node);
                let copied = result.split_off(result.len() - fields.len());
                let mut value = self.nodes[node].clone();
                value.children.clear();
                value.fields.clear();
                for ((key, _), child) in fields.into_iter().zip(copied) {
                    if value.object {
                        value.fields.insert(key.clone(), value.children.len());
                    }
                    value.children.push((key, Some(child)));
                }
                let id = self.nodes.len();
                self.nodes.push(value);
                result.push(id);
            } else if self.nodes[node].raw.is_some() {
                let value = self.nodes[node].clone();
                let id = self.nodes.len();
                self.nodes.push(value);
                result.push(id);
            } else {
                work.push((node, true));
                for (_, child) in self.fields(node).into_iter().rev() {
                    work.push((child, false));
                }
            }
        }
        result.pop().unwrap()
    }

    fn render(&self, root: Id) -> Vec<u8> {
        enum Work<'a> {
            Node(Id),
            Bytes(&'a [u8]),
            Key(&'a [u8]),
        }
        let mut output = Vec::new();
        let mut work = vec![Work::Node(root)];
        while let Some(task) = work.pop() {
            match task {
                Work::Bytes(bytes) => output.extend_from_slice(bytes),
                Work::Key(key) => {
                    output.extend(gj::stringify(key));
                    output.push(b':');
                }
                Work::Node(id) => {
                    let node = &self.nodes[id];
                    if let Some(raw) = &node.raw {
                        output.extend_from_slice(raw);
                        continue;
                    }
                    output.push(if node.object { b'{' } else { b'[' });
                    work.push(Work::Bytes(if node.object { b"}" } else { b"]" }));
                    let mut last = true;
                    for (key, child) in node.children.iter().rev() {
                        if let Some(child) = child {
                            if !last {
                                work.push(Work::Bytes(b","));
                            }
                            last = false;
                            work.push(Work::Node(*child));
                            if node.object {
                                work.push(Work::Key(key));
                            }
                        }
                    }
                }
            }
        }
        output
    }

    /// Schema-aware traversal with name-map parity and explicit parent edges.
    fn schemas(&self) -> Vec<SchemaEdge> {
        enum Mode {
            Schema,
            Names,
            List,
        }
        let mut work = vec![(0, Mode::Schema, None)];
        let mut result = Vec::new();
        while let Some((node, mode, parent)) = work.pop() {
            if self.nodes[node].raw.is_some() {
                continue;
            }
            match mode {
                Mode::Names => {
                    for (key, child) in self.fields(node).into_iter().rev() {
                        work.push((
                            child,
                            Mode::Schema,
                            parent.as_ref().map(|(owner, _): &(Id, Vec<u8>)| (*owner, key)),
                        ));
                    }
                }
                Mode::List => {
                    for child in self.items(node).into_iter().rev() {
                        work.push((child, Mode::Schema, None));
                    }
                }
                Mode::Schema => {
                    if !self.object(node) {
                        continue;
                    }
                    result.push((node, parent));
                    for (key, child) in self.fields(node).into_iter().rev() {
                        if is_literal_value_keyword(&key) {
                            continue;
                        }
                        let mode = if matches!(
                            key.as_slice(),
                            b"properties"
                                | b"patternProperties"
                                | b"dependentSchemas"
                                | b"dependencies"
                                | b"$defs"
                                | b"definitions"
                        ) && self.object(child)
                        {
                            Mode::Names
                        } else if self.array(child) {
                            Mode::List
                        } else {
                            Mode::Schema
                        };
                        let parent = (key == b"properties").then(|| (node, Vec::new()));
                        work.push((child, mode, parent));
                    }
                }
            }
        }
        result
    }

    fn hint(&mut self, node: Id, hint: &[u8]) {
        let merged = merge_hint(&self.text(node, b"description"), hint);
        self.set_text(node, b"description", &merged);
    }

    fn take_description(&mut self, node: Id) -> Description {
        self.descriptions
            .remove(&node)
            .unwrap_or_else(|| Description::new(self.text(node, b"description")))
    }

    fn defer_hint(&mut self, node: Id, hint: &[u8]) {
        let mut description = self.take_description(node);
        description.hint(hint);
        self.descriptions.insert(node, description);
    }

    fn flush_descriptions(&mut self, root: Id) {
        let mut work = vec![root];
        while let Some(node) = work.pop() {
            if let Some(description) = self.descriptions.remove(&node)
                && description.len > 0
            {
                self.set_text(node, b"description", &description.bytes());
            }
            work.extend(self.items(node));
        }
    }

    fn strings(&self, node: Id) -> Vec<Vec<u8>> {
        self.items(node).into_iter().map(|item| self.bytes(item)).collect()
    }

    fn set_strings(&mut self, node: Id, key: &[u8], values: &[Vec<u8>]) {
        let values = values.iter().map(|value| self.add_raw(gj::stringify(value))).collect();
        let array = self.add_array(values);
        self.set(node, key, array);
    }

    fn merge_missing(&mut self, dest: Id, incoming: Id) {
        self.merge_fields(dest, incoming, true);
    }

    fn merge_fields(&mut self, dest: Id, incoming: Id, recursive: bool) {
        let mut work = vec![(dest, incoming)];
        while let Some((dest, incoming)) = work.pop() {
            if dest == incoming {
                continue;
            }
            if self.nodes[dest].children.len() < self.nodes[incoming].children.len() {
                let (low, high) = if dest < incoming {
                    let (left, right) = self.nodes.split_at_mut(incoming);
                    (&mut left[dest], &mut right[0])
                } else {
                    let (left, right) = self.nodes.split_at_mut(dest);
                    (&mut right[0], &mut left[incoming])
                };
                std::mem::swap(low, high);
                for (key, preferred) in self.fields(incoming) {
                    if recursive
                        && let Some(existing) = self.field(dest, &key)
                        && self.object(preferred)
                        && self.object(existing)
                    {
                        work.push((preferred, existing));
                    }
                    self.set(dest, &key, preferred);
                }
                continue;
            }
            for (key, value) in self.fields(incoming) {
                match self.field(dest, &key) {
                    None => self.set(dest, &key, value),
                    Some(existing) if recursive && self.object(existing) && self.object(value) => {
                        work.push((existing, value))
                    }
                    _ => {}
                }
            }
        }
    }

    fn merge_required(&mut self, left: Id, right: Id) -> Id {
        if left == right {
            return left;
        }
        let (target, source) = if self.nodes[left].children.len() >= self.nodes[right].children.len() {
            (left, right)
        } else {
            (right, left)
        };
        if !self.required_names.contains_key(&target) {
            let names = self.strings(target).into_iter().collect();
            self.required_names.insert(target, names);
        }
        for item in self.items(source) {
            let name = self.bytes(item);
            if self.required_names.get_mut(&target).unwrap().insert(name) {
                self.nodes[target].children.push((Vec::new(), Some(item)));
            }
        }
        target
    }
}

pub(super) fn clean(raw: &[u8], options: Options) -> Option<Vec<u8>> {
    let mut tree = Tree::parse(raw)?;
    if !tree.object(0) {
        return Some(raw.to_vec());
    }
    clean_hints(&mut tree, options);
    let preserve_conjunctions = options.preserve_standard_constraints
        && tree
            .schemas()
            .iter()
            .any(|(node, _)| tree.field(*node, b"allOf").is_some_and(|child| tree.array(child)));
    merge_conditionals_indexed(&mut tree);
    if !options.preserve_standard_constraints {
        merge_all_of_indexed(&mut tree);
    }
    if options.flatten_unions && !preserve_conjunctions {
        flatten_unions_indexed(&mut tree, options);
    }
    if !preserve_conjunctions {
        flatten_types_indexed(&mut tree, options.antigravity_semantics);
    }
    remove_keywords_indexed(&mut tree, options);
    if !preserve_conjunctions {
        sanitize_indexed(&mut tree, options);
    }
    if options.add_placeholder {
        placeholders_indexed(&mut tree);
    }
    Some(tree.render(0))
}

fn clean_hints(tree: &mut Tree, options: Options) {
    for (node, _) in tree.schemas().into_iter().rev() {
        if let Some(reference) = tree.field(node, b"$ref") {
            let hint = [b"See: ".as_slice(), &ref_name(&tree.bytes(reference))].concat();
            if options.antigravity_semantics {
                tree.remove(node, b"$ref");
                tree.hint(node, &hint);
            } else {
                let desc = tree.text(node, b"description");
                let replacement = tree.add_object();
                tree.set_text(replacement, b"type", b"object");
                tree.set_text(replacement, b"description", &merge_hint(&desc, &hint));
                tree.replace(node, replacement);
            }
        }
    }
    for (node, _) in tree.schemas() {
        if let Some(value) = tree.field(node, b"const")
            && tree.field(node, b"enum").is_none()
            && let Some(value) = AnyValue::from_res(&gj::parse(&tree.render(value))).marshal()
        {
            let value = tree.add_raw(value);
            let array = tree.add_array(vec![value]);
            tree.set(node, b"enum", array);
        }
    }
    for (node, _) in tree.schemas() {
        if let Some(value) = tree.field(node, b"enum")
            && tree.array(value)
        {
            let values = tree.strings(value);
            if values.is_empty() {
                tree.set_raw(node, b"enum", b"null".to_vec());
            } else {
                tree.set_strings(node, b"enum", &values);
            }
            if options.force_enum_string_type {
                tree.set_text(node, b"type", b"string");
            }
        }
    }
    for (node, _) in tree.schemas() {
        if let Some(value) = tree.field(node, b"enum")
            && tree.array(value)
        {
            let values = tree.strings(value);
            if (2..=10).contains(&values.len()) {
                tree.hint(node, &[b"Allowed: ".as_slice(), &values.join(&b", "[..])].concat());
            }
        }
    }
    for (node, _) in tree.schemas() {
        if options.drop_all_enums || (options.drop_boolean_enums && tree.text(node, b"type") == b"boolean") {
            if let Some(value) = tree.field(node, b"enum")
                && tree.array(value)
            {
                let values = tree.strings(value);
                if values.len() == 1 {
                    tree.hint(node, &[b"Allowed: ".as_slice(), &values[0]].concat());
                }
            }
            tree.remove(node, b"enum");
        }
    }
    for (node, _) in tree.schemas() {
        if !options.preserve_additional_properties_false
            && !options.preserve_all_additional_properties
            && tree
                .field(node, b"additionalProperties")
                .is_some_and(|value| tree.nodes[value].raw.as_deref() == Some(b"false"))
        {
            tree.hint(node, b"No extra properties allowed");
        }
    }
    for key in constraint_keywords(options) {
        for (node, _) in tree.schemas() {
            if let Some(value) = tree.field(node, key) {
                tree.hint(node, &[key, b": ", &tree.bytes(value)].concat());
            }
        }
    }
    for (node, _) in tree.schemas() {
        if options.antigravity_semantics
            && let Some(value) = tree.field(node, b"not")
        {
            tree.hint(node, &[b"not: ".as_slice(), &tree.render(value)].concat());
        }
    }
}

fn merge_conditionals_indexed(tree: &mut Tree) {
    for (node, _) in tree.schemas().into_iter().rev() {
        for key in [b"then".as_slice(), b"else"] {
            if let Some(branch) = tree.field(node, key)
                && let Some(props) = tree.field(branch, b"properties")
                && tree.object(props)
            {
                let dest = tree.field(node, b"properties").unwrap_or_else(|| {
                    let props = tree.add_object();
                    tree.set(node, b"properties", props);
                    props
                });
                tree.merge_fields(dest, props, false);
            }
            tree.remove(node, key);
        }
    }
}

fn merge_all_of_indexed(tree: &mut Tree) {
    for (node, _) in tree.schemas().into_iter().rev() {
        let Some(array) = tree.field(node, b"allOf").filter(|array| tree.array(*array)) else {
            continue;
        };
        let branches = tree.items(array);
        for branch in branches {
            if !tree.object(branch) {
                continue;
            }
            let required = match (tree.field(node, b"required"), tree.field(branch, b"required")) {
                (Some(left), Some(right)) if tree.array(left) && tree.array(right) => {
                    Some(tree.merge_required(left, right))
                }
                (_, Some(right)) if tree.array(right) => Some(right),
                (left, _) => left,
            };
            // Move the growing branch into the parent, then overlay the smaller
            // parent. Required order is immaterial; its larger backing array is reused.
            if tree.nodes[node].children.len() < tree.nodes[branch].children.len() + 2 {
                let own = tree.fields(node);
                tree.replace(node, branch);
                for key in [b"if".as_slice(), b"then", b"else", b"allOf"] {
                    tree.remove(node, key);
                }
                for (key, value) in own {
                    if key == b"allOf" || key == b"required" {
                        continue;
                    }
                    if let Some(incoming) = tree.field(node, &key)
                        && tree.object(value)
                        && tree.object(incoming)
                    {
                        tree.merge_missing(value, incoming);
                    }
                    tree.set(node, &key, value);
                }
                if let Some(required) = required {
                    tree.set(node, b"required", required);
                }
                continue;
            }
            for (key, value) in tree.fields(branch) {
                match key.as_slice() {
                    b"required" | b"if" | b"then" | b"else" | b"allOf" => {}
                    _ => match tree.field(node, &key) {
                        None => tree.set(node, &key, value),
                        Some(existing) if tree.object(existing) && tree.object(value) => {
                            tree.merge_missing(existing, value)
                        }
                        _ => {}
                    },
                }
            }
            if let Some(required) = required {
                tree.set(node, b"required", required);
            }
        }
        tree.remove(node, b"allOf");
    }
}

fn select_branch(tree: &Tree, branches: &[Id]) -> (usize, Vec<Vec<u8>>) {
    let mut best = (0, -1);
    let mut types = Vec::new();
    for (index, branch) in branches.iter().enumerate() {
        let mut kind = tree.text(*branch, b"type");
        let score = if kind == b"object" || tree.field(*branch, b"properties").is_some() {
            if kind.is_empty() {
                kind = b"object".to_vec();
            }
            3
        } else if kind == b"array" || tree.field(*branch, b"items").is_some() {
            if kind.is_empty() {
                kind = b"array".to_vec();
            }
            2
        } else if !kind.is_empty() && kind != b"null" {
            1
        } else {
            0
        };
        if !kind.is_empty() {
            types.push(kind);
        }
        if score > best.1 {
            best = (index, score);
        }
    }
    (best.0, types)
}

fn flatten_unions_indexed(tree: &mut Tree, options: Options) {
    for (node, _) in tree.schemas().into_iter().rev() {
        for key in [b"oneOf".as_slice(), b"anyOf"] {
            let Some(array) = tree.field(node, key).filter(|array| tree.array(*array)) else {
                continue;
            };
            if tree.nodes[array].children.is_empty() {
                continue;
            }
            if key == b"oneOf"
                && !options.flatten_multi_branch_unions
                && !options.preserve_standard_constraints
                && tree.field(node, b"anyOf").is_some_and(|value| tree.array(value))
            {
                tree.flush_descriptions(array);
                tree.defer_hint(node, &[b"oneOf: ".as_slice(), &tree.render(array)].concat());
                tree.remove(node, key);
                continue;
            }
            let props = tree.field(node, b"properties").filter(|props| tree.object(*props));
            let mut non_null = Vec::new();
            let mut has_null = false;
            let mut filtered = false;
            for mut branch in tree.items(array) {
                if tree.nodes[branch].raw.as_deref() == Some(b"true") {
                    branch = tree.add_object();
                    filtered = true;
                } else if !tree.object(branch) {
                    filtered = true;
                    continue;
                }
                let required_only = tree.object(branch)
                    && tree.field(branch, b"required").is_some()
                    && [
                        b"type".as_slice(),
                        b"properties",
                        b"items",
                        b"enum",
                        b"anyOf",
                        b"oneOf",
                        b"allOf",
                    ]
                    .iter()
                    .all(|key| tree.field(branch, key).is_none());
                if required_only {
                    let required = tree
                        .field(branch, b"required")
                        .filter(|value| tree.array(*value))
                        .map(|value| tree.strings(value))
                        .unwrap_or_default();
                    if let Some(props) = props
                        && !required.is_empty()
                        && required.iter().all(|name| tree.field(props, name).is_some())
                    {
                        let copy = tree.clone_tree(branch);
                        let selected = tree.add_object();
                        for name in required {
                            let child = tree.clone_tree(tree.field(props, &name).unwrap());
                            tree.set(selected, &name, child);
                        }
                        tree.set(copy, b"properties", selected);
                        tree.set_text(copy, b"type", b"object");
                        branch = copy;
                        filtered = true;
                    } else {
                        filtered = true;
                        continue;
                    }
                }
                let kind = tree.field(branch, b"type");
                let null_only = tree.field(branch, b"properties").is_none()
                    && tree.field(branch, b"items").is_none()
                    && kind.is_some_and(|kind| {
                        if tree.array(kind) {
                            let values = tree.strings(kind);
                            !values.is_empty() && values.iter().all(|kind| kind == b"null")
                        } else {
                            tree.bytes(kind) == b"null"
                        }
                    });
                if null_only {
                    has_null = true;
                } else {
                    non_null.push(branch);
                }
            }
            let nullable = has_null || tree.truth(node, b"nullable");
            if non_null.len() >= 2 && !options.flatten_multi_branch_unions {
                let target = if key == b"oneOf" && !options.preserve_standard_constraints {
                    b"anyOf".as_slice()
                } else {
                    key
                };
                if target != key || has_null || filtered {
                    let value = tree.add_array(non_null);
                    tree.set(node, target, value);
                    if target != key {
                        tree.remove(node, key);
                    }
                }
                if has_null {
                    tree.set_raw(node, b"nullable", b"true".to_vec());
                    tree.defer_hint(node, b"(nullable)");
                }
                continue;
            }
            if let Some(props) = props {
                for branch in &non_null {
                    if let Some(incoming) = tree
                        .field(*branch, b"properties")
                        .filter(|incoming| tree.object(*incoming))
                    {
                        tree.merge_missing(props, incoming);
                    }
                }
                if nullable {
                    tree.set_raw(node, b"nullable", b"true".to_vec());
                }
                if tree.text(node, b"type").is_empty() {
                    tree.set_text(node, b"type", b"object");
                }
                tree.remove(node, key);
                continue;
            }
            if non_null.is_empty() {
                if has_null {
                    let description = tree.take_description(node);
                    let replacement = tree.add_object();
                    tree.set_text(replacement, b"type", b"null");
                    tree.replace(node, replacement);
                    tree.descriptions.insert(node, description);
                } else {
                    tree.remove(node, key);
                }
                continue;
            }
            let (best, mut types) = select_branch(tree, &non_null);
            if has_null && (filtered || non_null.len() >= 2) && !types.iter().any(|kind| kind == b"null") {
                types.push(b"null".to_vec());
            }
            if !filtered && non_null.len() == 1 {
                types = select_branch(tree, &tree.items(array)).1;
            }
            let parent_description = tree.take_description(node);
            let selected = non_null[best];
            if nullable {
                tree.set_raw(selected, b"nullable", b"true".to_vec());
            }
            let mut description = tree.take_description(selected);
            description.prepend(parent_description);
            if types.len() > 1 {
                description.hint(&[b"Accepts: ".as_slice(), &types.join(&b" | "[..])].concat());
            } else if nullable {
                description.hint(b"(nullable)");
            }
            tree.replace(node, selected);
            tree.descriptions.insert(node, description);
        }
    }
    tree.flush_descriptions(0);
}

fn flatten_types_indexed(tree: &mut Tree, native_nullable: bool) {
    let mut nullable_fields: HashMap<Id, HashSet<Vec<u8>>> = HashMap::new();
    for (node, parent) in tree.schemas().into_iter().rev() {
        let Some(array) = tree.field(node, b"type").filter(|array| tree.array(*array)) else {
            continue;
        };
        let values = tree.strings(array);
        if values.is_empty() {
            continue;
        }
        let has_null = values.iter().any(|value| value == b"null");
        let non_null: Vec<_> = values
            .into_iter()
            .filter(|value| !value.is_empty() && value != b"null")
            .collect();
        let first: &[u8] = if non_null.is_empty() {
            b"string".as_slice()
        } else if tree.field(node, b"items").is_some() && non_null.iter().any(|kind| kind == b"array") {
            b"array"
        } else if tree.field(node, b"properties").is_some() && non_null.iter().any(|kind| kind == b"object") {
            b"object"
        } else {
            &non_null[0]
        };
        tree.set_text(node, b"type", first);
        if first != b"array" {
            tree.remove(node, b"items");
        }
        if non_null.len() > 1 {
            tree.hint(node, &[b"Accepts: ".as_slice(), &non_null.join(&b" | "[..])].concat());
        }
        if has_null {
            if native_nullable {
                tree.set_raw(node, b"nullable", b"true".to_vec());
                tree.hint(node, b"(nullable)");
            } else if let Some((parent, name)) = parent {
                nullable_fields.entry(parent).or_default().insert(name);
                tree.hint(node, b"(nullable)");
            }
        }
    }
    for (parent, fields) in nullable_fields {
        if let Some(required) = tree.field(parent, b"required").filter(|required| tree.array(*required)) {
            let kept: Vec<_> = tree
                .strings(required)
                .into_iter()
                .filter(|value| !fields.contains(value))
                .collect();
            if kept.is_empty() {
                tree.remove(parent, b"required");
            } else {
                tree.set_strings(parent, b"required", &kept);
            }
        }
    }
}

fn remove_keywords_indexed(tree: &mut Tree, options: Options) {
    let mut keys = constraint_keywords(options);
    keys.extend([
        b"$schema".as_slice(),
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
        keys.push(b"not");
    }
    if options.remove_gemini_metadata {
        keys.extend([b"nullable".as_slice(), b"title"]);
    } else if options.remove_tool_title {
        keys.push(b"title");
    }
    let keys: HashSet<&[u8]> = keys.into_iter().collect();
    for (node, _) in tree.schemas() {
        for (key, value) in tree.fields(node) {
            let preserve = key == b"additionalProperties"
                && (options.preserve_all_additional_properties
                    || (options.preserve_additional_properties_false
                        && tree.nodes[value].raw.as_deref() == Some(b"false")));
            if !preserve && (keys.contains(key.as_slice()) || key.starts_with(b"x-")) {
                tree.remove(node, &key);
            }
        }
        if options.remove_gemini_metadata
            && let Some(props) = tree.field(node, b"properties").filter(|props| tree.object(*props))
        {
            let mut removed = Vec::new();
            if tree.field(props, b"_").is_some() {
                tree.remove(props, b"_");
                removed.push(b"_".to_vec());
            }
            if tree.fields(props).len() == 1
                && let Some(reason) = tree.field(props, b"reason")
                && tree.text(reason, b"description") == PLACEHOLDER_REASON_DESCRIPTION
            {
                tree.remove(props, b"reason");
                removed.push(b"reason".to_vec());
            }
            if !removed.is_empty()
                && let Some(required) = tree.field(node, b"required").filter(|required| tree.array(*required))
            {
                let kept: Vec<_> = tree
                    .strings(required)
                    .into_iter()
                    .filter(|value| !removed.contains(value))
                    .collect();
                if kept.is_empty() {
                    tree.remove(node, b"required");
                } else {
                    tree.set_strings(node, b"required", &kept);
                }
            }
        }
    }
}

fn sanitize_indexed(tree: &mut Tree, options: Options) {
    for (node, _) in tree.schemas() {
        let props = tree.field(node, b"properties").filter(|props| tree.object(*props));
        if let Some(required) = tree.field(node, b"required").filter(|required| tree.array(*required)) {
            if let Some(props) = props {
                let names = tree.strings(required);
                let kept: Vec<_> = names
                    .iter()
                    .filter(|name| tree.field(props, name).is_some())
                    .cloned()
                    .collect();
                if kept.len() != names.len() {
                    if kept.is_empty() {
                        tree.remove(node, b"required");
                    } else {
                        tree.set_strings(node, b"required", &kept);
                    }
                }
            } else {
                tree.remove(node, b"required");
            }
        }
        if props.is_some() && !tree.text(node, b"type").eq_ignore_ascii_case(b"object") {
            tree.set_text(node, b"type", b"object");
        }
        if tree.field(node, b"items").is_some() {
            let kind = tree.text(node, b"type");
            if kind.is_empty() {
                tree.set_text(node, b"type", b"array");
            } else if !kind.eq_ignore_ascii_case(b"array") {
                tree.remove(node, b"items");
            }
        }
        if options.add_missing_array_items
            && tree.text(node, b"type").eq_ignore_ascii_case(b"array")
            && tree.field(node, b"items").is_none()
            && tree.field(node, b"$ref").is_none()
        {
            let items = tree.add_object();
            tree.set_text(items, b"type", b"string");
            tree.set(node, b"items", items);
        }
    }
}

fn placeholders_indexed(tree: &mut Tree) {
    for (node, _) in tree.schemas().into_iter().rev() {
        if tree.text(node, b"type") != b"object"
            || [b"anyOf".as_slice(), b"oneOf"]
                .iter()
                .any(|key| tree.field(node, key).is_some_and(|value| tree.array(value)))
        {
            continue;
        }
        let props = tree.field(node, b"properties");
        let has_required = tree
            .field(node, b"required")
            .is_some_and(|value| tree.array(value) && !tree.items(value).is_empty());
        if props.is_none() || props.is_some_and(|props| tree.object(props) && tree.fields(props).is_empty()) {
            let props = props.unwrap_or_else(|| {
                let props = tree.add_object();
                tree.set(node, b"properties", props);
                props
            });
            let reason = tree.add_object();
            tree.set_text(reason, b"type", b"string");
            tree.set_text(reason, b"description", PLACEHOLDER_REASON_DESCRIPTION);
            tree.set(props, b"reason", reason);
            tree.set_strings(node, b"required", &[b"reason".to_vec()]);
        } else if let Some(props) = props.filter(|props| tree.object(*props))
            && !has_required
            && node != 0
        {
            if tree.field(props, b"_").is_none() {
                let placeholder = tree.add_object();
                tree.set_text(placeholder, b"type", b"boolean");
                tree.set(props, b"_", placeholder);
            }
            tree.set_strings(node, b"required", &[b"_".to_vec()]);
        }
    }
}
