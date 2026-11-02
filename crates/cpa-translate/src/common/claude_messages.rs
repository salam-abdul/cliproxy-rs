use cpa_common::json::{self as gj, Kind, Res};

// ---------------------------------------------------------------------------------------
// Claude messages (common/claude_messages.go)

/// Merges consecutive same-role Claude messages; assistant tool_use parts move after the
/// other parts of the merged turn.
#[derive(Default)]
pub struct ClaudeMessages {
    messages: Vec<Vec<u8>>,
    role: Vec<u8>,
    content: Vec<Vec<u8>>,
    tool_use: Vec<Vec<u8>>,
}

impl ClaudeMessages {
    pub fn append(&mut self, message: &[u8]) {
        if message.is_empty() {
            return;
        }
        let root = gj::parse(message);
        let role = root.get("role").bytes().into_owned();
        if role != b"user" && role != b"assistant" {
            return;
        }
        let parts = content_parts(&root.get("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        for part in parts {
            if role == b"assistant" && gj::get(&part, "type").str() == "tool_use" {
                self.tool_use.push(part);
            } else {
                self.content.push(part);
            }
        }
        self.role = role;
    }

    pub fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_use);
        if !parts.is_empty() {
            let mut message = br#"{"role":"","content":[]}"#.to_vec();
            gj::set_str(&mut message, "role", &self.role);
            gj::set_raw(&mut message, "content", gj::join(&parts));
            self.messages.push(message);
        }
        self.role.clear();
    }

    pub fn messages(mut self) -> Vec<Vec<u8>> {
        self.flush();
        self.messages
    }
}

fn content_parts(content: &Res<'_>) -> Vec<Vec<u8>> {
    if !content.exists() || content.kind == Kind::Null {
        return vec![];
    }
    if content.kind == Kind::String {
        if content.s.is_empty() {
            return vec![];
        }
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", content.bytes());
        return vec![part];
    }
    if !content.is_array() {
        return vec![];
    }
    let mut parts = vec![];
    content.each(|_, part| {
        if part.is_object() {
            parts.push(part.raw.to_vec());
        }
        true
    });
    parts
}
