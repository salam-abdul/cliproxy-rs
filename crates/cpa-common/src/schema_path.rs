//! Literal schema-path lookup and edits without call-stack descent. Complex gjson paths retain core semantics.
use crate::json::{self as gj, Res};
fn special_path(path: &[u8]) -> bool {
    let mut escaped = false;
    path.iter().any(|&c| {
        if escaped {
            escaped = false;
            return false;
        }
        if c == b'\\' {
            escaped = true;
            return false;
        }
        matches!(c, b'|' | b'#' | b'@' | b'*' | b'?' | b':')
    })
}
pub fn get<'a>(json: &'a [u8], path: &(impl gj::JsonPath + ?Sized)) -> Res<'a> {
    let path = path.as_path();
    if path == b"@this" {
        return gj::parse(json);
    }
    if special_path(path) {
        return gj::get(json, path);
    }
    let parts = split_path(path);
    if parts.is_empty() {
        return gj::get(json, path);
    }
    let mut work = vec![(gj::parse(json), 0usize)];
    while let Some((value, depth)) = work.pop() {
        if depth == parts.len() {
            return value;
        }
        if value.is_array() {
            let child = value.get(parts[depth]);
            if child.exists() {
                work.push((child, depth + 1));
            }
        } else if value.is_object() {
            let wanted = unescape_key(parts[depth]);
            let mut children = Vec::new();
            value.each(|key, child| {
                if key.bytes().as_ref() == wanted {
                    children.push(child);
                }
                true
            });
            for child in children.into_iter().rev() {
                work.push((child, depth + 1));
            }
        }
    }
    Res::default()
}

fn edit(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized), raw: Option<&[u8]>) -> bool {
    let path = path.as_path();
    let parts = split_path(path);
    if special_path(path) || !gj::valid(json) {
        return match raw {
            Some(raw) => gj::set_raw(json, path, raw),
            None => gj::delete(json, path),
        };
    }
    if parts.is_empty() {
        return match raw {
            Some(raw) => gj::set_raw(json, path, raw),
            None => gj::delete(json, path),
        };
    }
    let mut parent_parts = parts.len() - 1;
    let parent = loop {
        let parent_path = parts[..parent_parts].join(&b'.');
        let parent = if parent_parts == 0 {
            gj::parse(json)
        } else {
            get(json, &parent_path)
        };
        if parent.exists() {
            break parent;
        }
        if raw.is_none() {
            return true;
        }
        if parent_parts == 0 {
            return false;
        }
        parent_parts -= 1;
    };
    if parent.raw.first() == Some(&b'{') && parent.raw.last() != Some(&b'}') {
        return match raw {
            Some(raw) => gj::set_raw(json, path, raw),
            None => gj::delete(json, path),
        };
    }
    let index = parent.index;
    let length = parent.raw.len();
    let mut replacement = parent.raw.to_vec();
    let Some(raw) = raw else {
        if !gj::delete(&mut replacement, parts[parent_parts]) {
            return false;
        }
        json.splice(index..index + length, replacement);
        return true;
    };
    let mut nested = raw.to_vec();
    for part in parts[parent_parts + 1..].iter().rev() {
        let key = unescape_key(part);
        let mut value = if key.iter().all(u8::is_ascii_digit) && !key.is_empty() {
            b"[]".to_vec()
        } else {
            b"{}".to_vec()
        };
        if !gj::set_raw(&mut value, *part, &nested) {
            return false;
        }
        nested = value;
    }
    if !gj::set_raw(&mut replacement, parts[parent_parts], &nested) {
        return false;
    }
    json.splice(index..index + length, replacement);
    true
}
pub fn set_raw(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized), raw: impl AsRef<[u8]>) -> bool {
    edit(json, path, Some(raw.as_ref()))
}
pub fn set_str(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized), text: impl AsRef<[u8]>) -> bool {
    if special_path(path.as_path()) || !gj::valid(json) {
        return gj::set_str(json, path, text);
    }
    set_raw(json, path, gj::stringify(text.as_ref()))
}
pub fn set_strs<S: AsRef<[u8]>>(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized), items: &[S]) -> bool {
    set_raw(json, path, gj::quote_all(items))
}
pub fn set_bool(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized), value: bool) -> bool {
    set_raw(json, path, if value { b"true".as_slice() } else { b"false" })
}
pub fn delete(json: &mut Vec<u8>, path: &(impl gj::JsonPath + ?Sized)) -> bool {
    edit(json, path, None)
}

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
