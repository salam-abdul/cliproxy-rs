use cpa_common::json::{self as gj, Kind, Res};

// ---------------------------------------------------------------------------------------
// Cache control (common/cache_control.go)

fn valid_cache_control(cc: &Res<'_>) -> bool {
    if !cc.exists() || cc.kind == Kind::Null || !cc.is_object() {
        return false;
    }
    let t = cc.get("type");
    t.kind == Kind::String && &*t.bytes() == b"ephemeral"
}

pub fn attach_cache_control(mut dst: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let cc = src.get("cache_control");
    if valid_cache_control(&cc) {
        gj::set_raw(&mut dst, "cache_control", &cc.raw);
    }
    dst
}

pub fn attach_message_cache_control(msg: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let cc = src.get("cache_control");
    if !valid_cache_control(&cc) {
        return msg;
    }
    let content = gj::get(&msg, "content");
    if content.is_array() {
        let arr = content.array();
        if arr.is_empty() || arr[arr.len() - 1].get("cache_control").exists() {
            return msg;
        }
        let path = format!("content.{}.cache_control", arr.len() - 1);
        let mut out = msg.clone();
        return if gj::set_raw(&mut out, &path, &cc.raw) {
            out
        } else {
            msg
        };
    }
    if content.kind != Kind::String {
        return msg;
    }
    let mut part = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut part, "text", content.bytes());
    if !gj::set_raw(&mut part, "cache_control", &cc.raw) {
        return msg;
    }
    let mut out = msg.clone();
    if !gj::set_raw(&mut out, "content", "[]") {
        return msg;
    }
    gj::set_raw(&mut out, "content.-1", part);
    out
}

pub fn attach_tool_message_cache_control(msg: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let mut raw_cc = first_part_cache_control(src);
    if raw_cc.is_empty() {
        let cc = src.get("cache_control");
        if valid_cache_control(&cc) {
            raw_cc = cc.raw.to_vec();
        }
    }
    if raw_cc.is_empty() {
        return msg;
    }
    let content = gj::get(&msg, "content");
    if content.is_array() {
        let arr = content.array();
        let Some(target) = arr.iter().position(|b| b.get("type").str() == "tool_result") else {
            return msg;
        };
        let mut out = msg.clone();
        if gj::set_raw(&mut out, &format!("content.{target}.cache_control"), &raw_cc) {
            return out;
        }
    }
    msg
}

fn first_part_cache_control(src: &Res<'_>) -> Vec<u8> {
    let mut content = src.get("content");
    if !content.exists() {
        content = src.clone();
    }
    if content.is_array() {
        let mut raw = vec![];
        content.each(|_, part| {
            let cc = part.get("cache_control");
            if valid_cache_control(&cc) {
                raw = cc.raw.to_vec();
                return false;
            }
            true
        });
        return raw;
    }
    if content.is_object() {
        let cc = content.get("cache_control");
        if valid_cache_control(&cc) {
            return cc.raw.to_vec();
        }
    }
    vec![]
}
