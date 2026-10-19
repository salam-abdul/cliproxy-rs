//! Go-compatible JSON for declarative quota responses: field matching, duplicate
//! members, null handling, number validation and stable encoding.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

mod node;
pub use node::{Node, parse, parse_first};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// A value with Go `encoding/json` semantics.
pub trait GoJson: Sized + Default {
    /// Go type name, for error messages.
    const GO_TYPE: &'static str;
    /// The Go type as decode errors spell it; containers name their element type
    /// (`[]string`, `map[string]interface {}`).
    fn go_type() -> String {
        Self::GO_TYPE.to_owned()
    }
    /// Appends `json.Marshal` output.
    fn encode(&self, out: &mut Vec<u8>);
    /// `json.Unmarshal` into a zero value.
    fn decode(v: &Node) -> Result<Self, DecodeError> {
        let mut out = Self::default();
        out.decode_into(v)?;
        Ok(out)
    }
    /// `json.Unmarshal` into an existing value. `null` resets slices, maps, pointers and
    /// `any`, and leaves everything else alone.
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        if v.is_null() {
            if Self::NULL_CLEARS {
                *self = Self::default();
            }
            return Ok(());
        }
        *self = Self::decode_value(v)?;
        Ok(())
    }
    /// Decodes a non-null value from scratch. Implement this or [`Self::decode_into`].
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        let mut out = Self::default();
        out.decode_into(v)?;
        Ok(out)
    }
    /// Go `omitempty`: false, 0, "", nil or empty slices and maps, nil pointers. Structs
    /// (including `time.Time`) are never empty.
    fn is_empty(&self) -> bool;
    /// Whether `null` resets the field (slices, maps, pointers, `any`); scalars and
    /// structs keep their value.
    const NULL_CLEARS: bool = false;
}

/// `json.Marshal`.
pub fn to_vec<T: GoJson>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    v.encode(&mut out);
    out
}

/// `json.Unmarshal` of a complete document.
pub fn from_slice<T: GoJson>(raw: &[u8]) -> Result<T, DecodeError> {
    T::decode(&parse(raw)?)
}

/// `json.NewDecoder(bytes.NewReader(raw)).Decode`: the first value; see [`parse_first`].
pub fn from_slice_first<T: GoJson>(raw: &[u8]) -> Result<T, DecodeError> {
    T::decode(&parse_first(raw)?)
}

fn kind(v: &Node) -> &'static str {
    match v {
        Node::Null => "null",
        Node::Bool(_) => "bool",
        Node::Number(_) => "number",
        Node::String(_) => "string",
        Node::Array(_) => "array",
        Node::Object(_) => "object",
    }
}

/// A value of the wrong JSON kind for `go_type`. Go names a number without its
/// literal here; [`number_error`] is the numeric-range case that includes it.
pub fn type_error(v: &Node, go_type: &str) -> DecodeError {
    let what = match v {
        Node::Number(_) => "number",
        other => kind(other),
    };
    DecodeError(format!("json: cannot unmarshal {what} into Go value of type {go_type}"))
}

/// A number that does not fit the numeric `go_type` (Go `"number " + literal`).
pub fn number_error(literal: &str, go_type: &str) -> DecodeError {
    DecodeError(format!(
        "json: cannot unmarshal number {literal} into Go value of type {go_type}"
    ))
}

/// Adds the struct field to a decode error, like Go's `UnmarshalTypeError`: the
/// innermost struct's name and the field path from the outermost struct (array
/// indexes and map keys are not part of it).
pub fn in_field(err: DecodeError, strukt: &str, field: &str) -> DecodeError {
    if let Some((head, ty)) = err.0.split_once(" into Go value of type ") {
        return DecodeError(format!("{head} into Go struct field {strukt}.{field} of type {ty}"));
    }
    // A field of a nested struct: this field goes in front of its path.
    if let Some((head, rest)) = err.0.split_once(" into Go struct field ")
        && let Some((inner, path)) = rest.split_once('.')
    {
        return DecodeError(format!("{head} into Go struct field {inner}.{field}.{path}"));
    }
    err
}

/// Writes one JSON object field by field.
pub struct ObjWriter<'a> {
    out: &'a mut Vec<u8>,
    first: bool,
}

impl<'a> ObjWriter<'a> {
    pub fn begin(out: &'a mut Vec<u8>) -> Self {
        out.push(b'{');
        Self { out, first: true }
    }

    fn key(&mut self, name: &str) {
        if !self.first {
            self.out.push(b',');
        }
        self.first = false;
        crate::json::marshal_str(self.out, name.as_bytes(), true);
        self.out.push(b':');
    }

    pub fn field<T: GoJson>(&mut self, name: &str, v: &T, omitempty: bool) {
        if omitempty && v.is_empty() {
            return;
        }
        self.key(name);
        v.encode(self.out);
    }

    /// Fields of an embedded struct, flattened like Go embedding.
    pub fn embed<T: GoStruct>(&mut self, v: &T) {
        let mut inner = ObjWriter {
            out: &mut *self.out,
            first: self.first,
        };
        v.encode_fields(&mut inner);
        self.first = inner.first;
    }

    pub fn end(self) {
        self.out.push(b'}');
    }
}

/// A Go struct: encodes its fields in order and decodes them by name.
pub trait GoStruct: GoJson {
    fn encode_fields(&self, w: &mut ObjWriter<'_>);
    /// Decodes `v` into the field named `key` (exact match first, then
    /// case-insensitive). `None` when no field matches.
    fn decode_field(&mut self, key: &str, v: &Node) -> Option<Result<(), DecodeError>>;
}

/// Decodes an object into a fresh struct.
pub fn decode_struct<T: GoStruct>(v: &Node) -> Result<T, DecodeError> {
    let mut out = T::default();
    decode_struct_into(&mut out, v)?;
    Ok(out)
}

/// Decodes an object member by member into an existing struct; later duplicates decode
/// over earlier ones.
pub fn decode_struct_into<T: GoStruct>(out: &mut T, v: &Node) -> Result<(), DecodeError> {
    match v {
        Node::Null => Ok(()),
        Node::Object(members) => {
            // Go keeps decoding after a type mismatch and reports the first one.
            let mut first_err = None;
            for (key, value) in members {
                if let Some(Err(e)) = out.decode_field(key, value) {
                    first_err.get_or_insert(e);
                }
            }
            first_err.map_or(Ok(()), Err)
        }
        other => Err(type_error(other, &T::go_type())),
    }
}

/// Declares a Go struct with its wire names. Append `omitempty` after a name for Go's
/// `,omitempty`.
///
/// ```ignore
/// go_struct! {
///     pub struct PayloadResponse("quota.PayloadResponse") {
///         "Value" => value: f64,
///         "path" omitempty => path: String,
///     }
/// }
/// ```
#[macro_export]
macro_rules! go_struct {
    (
        $(#[$meta:meta])*
        pub struct $name:ident ($go_type:literal) {
            $( $(#[$fmeta:meta])* $wire:literal $($omit:ident)? => $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct $name {
            $( $(#[$fmeta])* pub $field: $ty, )*
        }

        impl $crate::wirejson::GoJson for $name {
            const GO_TYPE: &'static str = $go_type;
            fn encode(&self, out: &mut Vec<u8>) {
                let mut w = $crate::wirejson::ObjWriter::begin(out);
                $crate::wirejson::GoStruct::encode_fields(self, &mut w);
                w.end();
            }
            fn decode_into(&mut self, v: &$crate::wirejson::Node) -> Result<(), $crate::wirejson::DecodeError> {
                $crate::wirejson::decode_struct_into(self, v)
            }
            fn is_empty(&self) -> bool {
                false
            }
        }

        impl $crate::wirejson::GoStruct for $name {
            #[allow(unused_variables)]
            fn encode_fields(&self, w: &mut $crate::wirejson::ObjWriter<'_>) {
                $( w.field($wire, &self.$field, $crate::go_struct!(@omit $($omit)?)); )*
            }
            #[allow(unused_variables)]
            fn decode_field(
                &mut self,
                key: &str,
                v: &$crate::wirejson::Node,
            ) -> Option<Result<(), $crate::wirejson::DecodeError>> {
                $(
                    if key == $wire {
                        return Some($crate::wirejson::decode_field_value(&mut self.$field, v, $go_type, $wire));
                    }
                )*
                $(
                    if key.eq_ignore_ascii_case($wire) {
                        return Some($crate::wirejson::decode_field_value(&mut self.$field, v, $go_type, $wire));
                    }
                )*
                None
            }
        }
    };
    (@omit omitempty) => { true };
    (@omit) => { false };
}

/// Decodes one field into its current value.
pub fn decode_field_value<T: GoJson>(slot: &mut T, v: &Node, strukt: &str, field: &str) -> Result<(), DecodeError> {
    slot.decode_into(v)
        .map_err(|e| in_field(e, strukt.rsplit('.').next().unwrap_or(strukt), field))
}

// ----------------------------------------------------------------------------------------
// Scalars

impl GoJson for String {
    const GO_TYPE: &'static str = "string";
    fn encode(&self, out: &mut Vec<u8>) {
        crate::json::marshal_str(out, self.as_bytes(), true);
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::String(s) => Ok(s.clone()),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl GoJson for bool {
    const GO_TYPE: &'static str = "bool";
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(if *self { b"true" } else { b"false" });
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::Bool(b) => Ok(*b),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        !*self
    }
}

macro_rules! go_int {
    ($t:ty, $go:literal) => {
        impl GoJson for $t {
            const GO_TYPE: &'static str = $go;
            fn encode(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(self.to_string().as_bytes());
            }
            /// Go parses the literal with `strconv.ParseInt`/`ParseUint`: `1.0`, `1e2` and
            /// out-of-range values are errors.
            fn decode_value(v: &Node) -> Result<Self, DecodeError> {
                match v {
                    Node::Number(n) => n.parse::<$t>().map_err(|_| number_error(n, Self::GO_TYPE)),
                    other => Err(type_error(other, Self::GO_TYPE)),
                }
            }
            fn is_empty(&self) -> bool {
                *self == 0
            }
        }
    };
}

// Go `int` has the platform's pointer width, like `isize`.
go_int!(isize, "int");
go_int!(i64, "int64");
go_int!(i32, "int32");
go_int!(u32, "uint32");
go_int!(u64, "uint64");

impl GoJson for f64 {
    const GO_TYPE: &'static str = "float64";
    fn encode(&self, out: &mut Vec<u8>) {
        // json.Marshal rejects NaN and Inf; the host never produces them.
        out.extend_from_slice(crate::json::json_float(*self).unwrap_or_else(|| "0".into()).as_bytes());
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        match v {
            Node::Number(n) => node::parse_f64(n),
            other => Err(type_error(other, Self::GO_TYPE)),
        }
    }
    fn is_empty(&self) -> bool {
        *self == 0.0
    }
}

/// `any` / `map[string]any` values: Go decodes numbers to float64 and encodes maps with
/// sorted keys.
impl GoJson for Value {
    const GO_TYPE: &'static str = "interface {}";
    const NULL_CLEARS: bool = true;
    fn encode(&self, out: &mut Vec<u8>) {
        encode_any(self, out);
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        v.to_value()
    }
    fn is_empty(&self) -> bool {
        self.is_null()
    }
}

/// Go's encoding of a value decoded into `any`.
pub fn encode_any(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => b.encode(out),
        Value::Number(n) => match n.as_f64() {
            Some(f) => f.encode(out),
            None => out.extend_from_slice(n.to_string().as_bytes()),
        },
        Value::String(s) => crate::json::marshal_str(out, s.as_bytes(), true),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                encode_any(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push(b'{');
            for (i, (key, item)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                crate::json::marshal_str(out, key.as_bytes(), true);
                out.push(b':');
                encode_any(item, out);
            }
            out.push(b'}');
        }
    }
}

// ----------------------------------------------------------------------------------------
// Containers

/// A slice: decoding reuses existing elements and truncates to the array's length.
impl<T: GoJson> GoJson for Vec<T> {
    const GO_TYPE: &'static str = "slice";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        format!("[]{}", T::go_type())
    }
    fn encode(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            out.extend_from_slice(b"null");
            return;
        }
        encode_items(self, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        match v {
            Node::Null => {
                self.clear();
                Ok(())
            }
            Node::Array(items) => {
                let mut first_err = None;
                for (i, item) in items.iter().enumerate() {
                    if i == self.len() {
                        self.push(T::default());
                    }
                    if let Err(e) = self[i].decode_into(item) {
                        first_err.get_or_insert(e);
                    }
                }
                self.truncate(items.len());
                first_err.map_or(Ok(()), Err)
            }
            other => Err(type_error(other, &Self::go_type())),
        }
    }
    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

fn encode_items<T: GoJson>(items: &[T], out: &mut Vec<u8>) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        item.encode(out);
    }
    out.push(b']');
}

/// A pointer: `null` makes it nil; a value decodes into the existing pointee.
impl<T: GoJson> GoJson for Option<T> {
    const GO_TYPE: &'static str = T::GO_TYPE;
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        T::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            None => out.extend_from_slice(b"null"),
            Some(v) => v.encode(out),
        }
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        if v.is_null() {
            *self = None;
            return Ok(());
        }
        self.get_or_insert_with(T::default).decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.is_none()
    }
}

/// A map: keys merge into the existing map; each element is decoded fresh.
impl<T: GoJson> GoJson for BTreeMap<String, T> {
    const GO_TYPE: &'static str = "map";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        format!("map[string]{}", T::go_type())
    }
    fn encode(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            out.extend_from_slice(b"null");
            return;
        }
        encode_map(self, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        match v {
            Node::Null => {
                self.clear();
                Ok(())
            }
            Node::Object(members) => {
                let mut first_err = None;
                for (k, item) in members {
                    match T::decode(item) {
                        Ok(value) => {
                            self.insert(k.clone(), value);
                        }
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                first_err.map_or(Ok(()), Err)
            }
            other => Err(type_error(other, &Self::go_type())),
        }
    }
    fn is_empty(&self) -> bool {
        BTreeMap::is_empty(self)
    }
}

fn encode_map<T: GoJson>(map: &BTreeMap<String, T>, out: &mut Vec<u8>) {
    out.push(b'{');
    for (i, (key, item)) in map.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        crate::json::marshal_str(out, key.as_bytes(), true);
        out.push(b':');
        item.encode(out);
    }
    out.push(b'}');
}

/// A slice or map Go builds non-nil even when empty: encodes `[]` / `{}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NonNil<T>(pub T);

impl<T: GoJson> GoJson for NonNil<Vec<T>> {
    const GO_TYPE: &'static str = "slice";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        Vec::<T>::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        encode_items(&self.0, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        self.0.decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: GoJson> GoJson for NonNil<BTreeMap<String, T>> {
    const GO_TYPE: &'static str = "map";
    const NULL_CLEARS: bool = true;
    fn go_type() -> String {
        BTreeMap::<String, T>::go_type()
    }
    fn encode(&self, out: &mut Vec<u8>) {
        encode_map(&self.0, out);
    }
    fn decode_into(&mut self, v: &Node) -> Result<(), DecodeError> {
        self.0.decode_into(v)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `http.Header` and `url.Values`.
pub type Header = BTreeMap<String, Vec<String>>;
/// `map[string]any`.
pub type Metadata = BTreeMap<String, Value>;
/// `map[string]string`.
pub type StringMap = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    crate::go_struct! {
        pub struct Sample("quota.Sample") {
            "AuthID" => auth_id: String,
            "Headers" => headers: Header,
            "Count" => count: i64,
            "path" omitempty => path: String,
            "Meta" => meta: Metadata,
            "Items" omitempty => items: Vec<String>,
        }
    }

    #[test]
    fn encodes_like_json_marshal() {
        let mut headers = Header::new();
        headers.insert("X-B".into(), vec!["2".into()]);
        headers.insert("A".into(), vec!["<1>".into()]);
        let s = Sample {
            auth_id: "a&b".into(),
            headers,
            count: 7,
            path: String::new(),
            meta: [
                ("z".to_owned(), serde_json::json!(1)),
                ("a".to_owned(), serde_json::json!(0.5)),
            ]
            .into(),
            items: vec![],
        };
        assert_eq!(
            String::from_utf8(to_vec(&s)).unwrap(),
            r#"{"AuthID":"a\u0026b","Headers":{"A":["\u003c1\u003e"],"X-B":["2"]},"Count":7,"Meta":{"a":0.5,"z":1}}"#
        );
    }

    /// Go matches keys exactly first, then case-insensitively; unknown keys are ignored;
    /// integer fields reject non-integer literals.
    #[test]
    fn decodes_like_json_unmarshal() {
        let s: Sample = from_slice(br#"{"authid":"x","AuthID":"y","body":"aGk=","unknown":1,"count":3,"PATH":"/p","at":"2026-01-02T03:04:05.5+02:00"}"#).unwrap();
        assert_eq!(s.auth_id, "y");
        assert_eq!(s.count, 3);
        assert_eq!(s.path, "/p");
        let err = from_slice::<Sample>(br#"{"Count":1.5}"#).unwrap_err();
        assert!(err.0.contains("Go struct field Sample.Count of type int64"), "{err}");
        let s: Sample = from_slice(br#"{"Count":null}"#).unwrap();
        assert_eq!(s.count, 0);
    }

    /// Go decodes member by member into the existing value: a second spelling of a map
    /// field merges keys; a later struct member decodes over the earlier one field by
    /// field.
    #[test]
    fn decodes_into_existing_values_like_go() {
        crate::go_struct! {
            pub struct Resp("quota.Response") {
                "Headers" => headers: Header,
                    "Inner" => inner: Option<Sample>,
            }
        }
        let r: Resp = from_slice(
            br#"{"Headers":{"A":["1"]},"headers":{"B":["2"]},"Body":"Zh==","Inner":{"Count":1},"inner":{"Path":"/p"}}"#,
        )
        .unwrap();
        assert_eq!(r.headers.keys().collect::<Vec<_>>(), ["A", "B"]);
        let inner = r.inner.unwrap();
        assert_eq!((inner.count, inner.path.as_str()), (1, "/p"));
        // An unknown member never needs to fit a Go type.
        assert!(from_slice::<Resp>(br#"{"unused":1e400}"#).is_ok());
    }

    #[test]
    fn non_nil_and_option_encode_empty_values() {
        assert_eq!(to_vec(&NonNil(Vec::<String>::new())), b"[]");
        assert_eq!(to_vec(&None::<Vec<String>>), b"null");
        assert_eq!(to_vec(&Vec::<String>::new()), b"null");
    }
}
