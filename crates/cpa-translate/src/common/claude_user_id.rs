use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::config::go_trim_space;
use sha2::{Digest, Sha256};

use super::norm;

// ---------------------------------------------------------------------------------------
// Claude user ID (common/claude_user_id.go)

pub fn derive_claude_user_id(raw: &[u8]) -> Vec<u8> {
    let root = gj::parse(raw);
    for path in ["metadata.user_id", "user"] {
        let v = root.get(path);
        if v.kind == Kind::String && !go_trim_space(&v.s).is_empty() {
            return v.s.to_vec();
        }
    }
    let mut seed: Vec<u8> = vec![];
    let write = |seed: &mut Vec<u8>, prefix: &str, value: &[u8]| {
        seed.extend_from_slice(prefix.as_bytes());
        seed.extend_from_slice(value);
    };
    let v = root.get("prompt_cache_key");
    if v.exists() && !go_trim_space(&v.bytes()).is_empty() {
        write(&mut seed, "prompt_cache_key:", go_trim_space(&v.bytes()));
    }
    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            let v = root.get(path);
            if v.exists() && !go_trim_space(&v.bytes()).is_empty() {
                write(&mut seed, "session_id:", go_trim_space(&v.bytes()));
                break;
            }
        }
    }
    if seed.is_empty() {
        let conversation = root.get("conversation");
        let id = conversation.get("id").bytes();
        if !go_trim_space(&id).is_empty() {
            write(&mut seed, "conversation_id:", go_trim_space(&id));
        } else if conversation.kind == Kind::String {
            let text = conversation.bytes();
            if !go_trim_space(&text).is_empty() {
                write(&mut seed, "conversation_id:", go_trim_space(&text));
            }
        } else {
            let v = root.get("conversation_id");
            if v.exists() && !go_trim_space(&v.bytes()).is_empty() {
                write(&mut seed, "conversation_id:", go_trim_space(&v.bytes()));
            }
        }
    }
    if seed.is_empty() {
        let content = first_stable_request_content(&root);
        if !content.is_empty() {
            write(&mut seed, "content:", &content);
        }
    }
    if seed.is_empty() {
        let v = root.get("model");
        if v.exists() && !go_trim_space(&v.bytes()).is_empty() {
            write(&mut seed, "model:", go_trim_space(&v.bytes()));
        }
        for path in ["instructions", "system", "systemInstruction", "system_instruction"] {
            let v = root.get(path);
            if v.exists() {
                write(&mut seed, &format!(";{path}:"), &v.bytes());
            }
        }
    }
    if seed.is_empty() {
        return b"unknown".to_vec();
    }
    cpa_core::hex::encode(Sha256::digest(&seed)).into_bytes()
}

fn joined_texts(content: &Res<'_>, accept: impl Fn(&Res<'_>) -> bool) -> Vec<u8> {
    if content.kind == Kind::String {
        return go_trim_space(&content.bytes()).to_vec();
    }
    if !content.is_array() {
        return vec![];
    }
    let mut texts: Vec<Vec<u8>> = vec![];
    content.each(|_, part| {
        if accept(&part) {
            let text = part.get("text");
            if text.exists() {
                let value = go_trim_space(&text.bytes()).to_vec();
                if !value.is_empty() {
                    texts.push(value);
                }
            }
        }
        true
    });
    go_trim_space(&texts.join(&b'\n')).to_vec()
}

fn first_stable_request_content(root: &Res<'_>) -> Vec<u8> {
    let messages = root.get("messages");
    if messages.is_array() {
        let mut content = vec![];
        messages.each(|_, message| {
            if norm(&message.get("role")) == "user" {
                content = joined_texts(&message.get("content"), |p| p.get("type").str() == "text");
                if !content.is_empty() {
                    return false;
                }
            }
            true
        });
        if !content.is_empty() {
            return content;
        }
    }
    let input = root.get("input");
    if input.exists() {
        if input.kind == Kind::String {
            let text = go_trim_space(&input.bytes()).to_vec();
            if !text.is_empty() {
                return text;
            }
        } else if input.is_array() {
            let mut content = vec![];
            input.each(|_, item| {
                if is_responses_user_item(&item) {
                    content = joined_texts(&item.get("content"), |p| {
                        matches!(&*p.get("type").str(), "input_text" | "output_text" | "text")
                    });
                    if !content.is_empty() {
                        return false;
                    }
                }
                true
            });
            if !content.is_empty() {
                return content;
            }
        }
    }
    let contents = root.get("contents");
    if contents.is_array() {
        let mut content = vec![];
        contents.each(|_, item| {
            let role = norm(&item.get("role"));
            if role.is_empty() || role == "user" {
                let parts = item.get("parts");
                if parts.is_array() {
                    let mut texts: Vec<Vec<u8>> = vec![];
                    parts.each(|_, part| {
                        if is_gemini_thought_part(&part) {
                            return true;
                        }
                        let text = part.get("text");
                        if text.exists() {
                            let value = go_trim_space(&text.bytes()).to_vec();
                            if !value.is_empty() {
                                texts.push(value);
                            }
                        }
                        true
                    });
                    if !texts.is_empty() {
                        content = texts.join(&b'\n');
                        return false;
                    }
                }
            }
            true
        });
        if !content.is_empty() {
            return content;
        }
    }
    vec![]
}

fn is_responses_user_item(item: &Res<'_>) -> bool {
    match norm(&item.get("role")).as_str() {
        "user" => true,
        "system" | "developer" | "assistant" => false,
        _ => norm(&item.get("type")) == "message",
    }
}

pub fn is_gemini_thought_part(part: &Res<'_>) -> bool {
    part.get("thought").bool()
}
