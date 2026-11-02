//! Go `encoding/json` output built by hand: struct-ordered objects and indentation.

use std::collections::BTreeMap;
use std::fmt::Display;

use super::{compact, marshal_str};

/// A JSON object written in Go struct-field order with `json.Marshal`'s escaping (HTML
/// characters, U+2028 and U+2029 escaped; invalid UTF-8 as U+FFFD).
pub struct Object(Vec<u8>);

impl Default for Object {
    fn default() -> Self {
        Self::new()
    }
}

impl Object {
    pub fn new() -> Self {
        Self(vec![b'{'])
    }

    fn key(&mut self, key: &str) -> &mut Vec<u8> {
        if self.0.len() > 1 {
            self.0.push(b',');
        }
        marshal_str(&mut self.0, key.as_bytes(), true);
        self.0.push(b':');
        &mut self.0
    }

    /// A field whose value is already JSON.
    pub fn raw(mut self, key: &str, json: impl AsRef<[u8]>) -> Self {
        self.key(key).extend_from_slice(json.as_ref());
        self
    }

    pub fn str(mut self, key: &str, value: impl AsRef<[u8]>) -> Self {
        marshal_str(self.key(key), value.as_ref(), true);
        self
    }

    /// A string field with `omitempty`.
    pub fn str_opt(self, key: &str, value: impl AsRef<[u8]>) -> Self {
        if value.as_ref().is_empty() {
            self
        } else {
            self.str(key, value)
        }
    }

    pub fn int(mut self, key: &str, value: impl Display) -> Self {
        let text = value.to_string();
        self.key(key).extend_from_slice(text.as_bytes());
        self
    }

    /// A `[]string` field.
    pub fn strs<S: AsRef<[u8]>>(self, key: &str, items: &[S]) -> Self {
        let mut raw = vec![b'['];
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                raw.push(b',');
            }
            marshal_str(&mut raw, item.as_ref(), true);
        }
        raw.push(b']');
        self.raw(key, raw)
    }

    /// A `map[string]string,omitempty` field: keys sorted, as Go marshals maps.
    pub fn map_opt(self, key: &str, map: &BTreeMap<String, String>) -> Self {
        if map.is_empty() {
            return self;
        }
        let inner = map.iter().fold(Object::new(), |o, (k, v)| o.str(k, v));
        self.raw(key, inner.finish())
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.0.push(b'}');
        self.0
    }

    /// [`Object::finish`] as text; every field written here is valid UTF-8.
    pub fn finish_string(self) -> String {
        String::from_utf8(self.finish()).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }
}

/// Go `json.Indent(dst, src, "", "  ")`: whitespace outside strings dropped, each
/// element on its own line, empty objects and arrays kept as `{}` and `[]`. `None` when
/// Go's scanner rejects `src`.
pub fn indent(src: &[u8]) -> Option<Vec<u8>> {
    if !super::std_valid(src) {
        return None;
    }
    fn newline(out: &mut Vec<u8>, depth: usize) {
        out.push(b'\n');
        for _ in 0..depth {
            out.extend_from_slice(b"  ");
        }
    }
    let mut out = Vec::with_capacity(src.len() * 2);
    let (mut depth, mut need_indent, mut in_string, mut escaped) = (0usize, false, false, false);
    for &c in src {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        if need_indent && c != b'}' && c != b']' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(c);
            }
            b'{' | b'[' => {
                need_indent = true;
                out.push(c);
            }
            b',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'}' | b']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth -= 1;
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Go `json.MarshalIndent(v, "", "  ")` given the compact encoding of `v` in field order
/// (for example serde's): HTML-escaped as `json.Marshal` does, then indented. Invalid
/// JSON comes back unchanged.
pub fn marshal_indent(json: &[u8]) -> Vec<u8> {
    let escaped = compact(json, true);
    indent(&escaped).unwrap_or(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_matches_go_marshal() {
        let mut map = BTreeMap::new();
        map.insert("b".to_owned(), "2".to_owned());
        map.insert("a".to_owned(), "<1>".to_owned());
        let json = Object::new()
            .str("s", "x&y")
            .str_opt("empty", "")
            .int("n", -3)
            .map_opt("m", &map)
            .strs("l", &["\u{2028}"])
            .str("bad", b"\xff")
            .finish_string();
        assert_eq!(
            json,
            r#"{"s":"x\u0026y","n":-3,"m":{"a":"\u003c1\u003e","b":"2"},"l":["\u2028"],"bad":"\ufffd"}"#
        );
        assert_eq!(Object::new().finish(), b"{}");
    }

    #[test]
    fn marshal_indent_matches_go() {
        // json.MarshalIndent(struct{Z any; A []int; S string}{map[string]any{}, nil...}).
        let compact = "{\"z\":{},\"a\":[1,[]],\"s\":\"<\u{2028}\u{2029}&>\"}";
        assert_eq!(
            String::from_utf8(marshal_indent(compact.as_bytes())).unwrap(),
            "{\n  \"z\": {},\n  \"a\": [\n    1,\n    []\n  ],\n  \"s\": \"\\u003c\\u2028\\u2029\\u0026\\u003e\"\n}"
        );
        assert_eq!(indent(b"{"), None);
    }
}
