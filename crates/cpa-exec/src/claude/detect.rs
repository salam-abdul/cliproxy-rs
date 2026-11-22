//! Native Claude Code detection (helps/claude_client_detection.go).
//!
//! A request is confirmed native only with all strong signals (x-app=cli or cli-bg, a native
//! User-Agent, the claude-code beta and a valid metadata.user_id; count_tokens omits
//! the last) from a verified entrypoint, or as one of the exactly measured Haiku helper
//! shapes. Without `stabilize-device-profile` the User-Agent may be any release at or
//! above the measured one within its major version, and a newer one also needs
//! well-formed Stainless version headers. With it, Go's rule applies: the same
//! major.minor, patch at or above the measured one, and no Stainless requirement.
//! Copying the User-Agent alone is not enough.

use http::HeaderMap;

use super::profile::{self, Profile};
use super::settings::Settings;
use crate::rawjson;

/// Entrypoints whose wire behaviour was captured and reviewed for passthrough.
pub(crate) const NATIVE_ENTRYPOINTS: &[&str] = &["cli", "sdk-cli", "claude-vscode"];
pub(crate) const HELPER_MODEL: &str = "claude-haiku-4-5-20251001";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Detection {
    pub confirmed: bool,
    pub helper_profile: bool,
    pub entrypoint: String,
}

/// First non-empty value of `name`, trimmed (`headerValueCaseInsensitive`).
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go `headerValue`: the first value, untrimmed.
pub(crate) fn raw_header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

/// All non-empty trimmed values of `name` (`HeaderValuesCaseInsensitive`).
pub(crate) fn header_values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect()
}

/// The entrypoint of a well-formed Claude Code User-Agent, or an empty string.
pub(crate) fn user_agent_entrypoint(user_agent: &str) -> String {
    parse_user_agent(user_agent).map_or_else(String::new, |ua| ua.entrypoint)
}

/// A Claude Code User-Agent: `claude-cli/N.N.N (external, <entrypoint>[, name/value]...)`.
/// Well-formed is not native: the version and entrypoint rules still apply.
#[derive(Debug)]
pub(crate) struct ClaudeUserAgent {
    pub version: profile::Version,
    pub entrypoint: String,
    /// The User-Agent without its request fields: the identity a stabilized profile
    /// may store.
    pub profile_user_agent: String,
    /// `name/value` fields other than `agent-sdk` (`workload/cron`), as sent with their
    /// leading space. They describe one request, never the client.
    pub request_fields: Vec<String>,
}

/// Go `parseClaudeCodeUserAgent`.
pub(crate) fn parse_user_agent(user_agent: &str) -> Option<ClaudeUserAgent> {
    if user_agent.chars().any(|c| (c < ' ' && c != '\t') || c == '\x7f') {
        return None;
    }
    let user_agent = user_agent.trim();
    let (prefix, comment) = user_agent.split_once('(')?;
    let comment = comment.strip_suffix(')')?;
    if prefix == prefix.trim() || comment.contains(['(', ')']) {
        return None;
    }
    let product = prefix.trim();
    if !product
        .strip_prefix("claude-cli/")
        .is_some_and(profile::package_version_ok)
    {
        return None;
    }
    let version = profile::version(product)?;
    let fields: Vec<&str> = comment.split(',').collect();
    if fields.len() < 2 || !fields[0].eq_ignore_ascii_case("external") {
        return None;
    }
    let entrypoint = fields[1].trim().to_lowercase();
    if entrypoint.is_empty() {
        return None;
    }
    let mut parsed = ClaudeUserAgent {
        version,
        entrypoint,
        profile_user_agent: user_agent.to_owned(),
        request_fields: Vec::new(),
    };
    let mut stable = vec![fields[0], fields[1]];
    let mut seen = std::collections::HashSet::new();
    for field in &fields[2..] {
        let (name, value) = field.trim().split_once('/')?;
        if !token(name) || value.trim().is_empty() || !seen.insert(name.to_lowercase()) {
            return None;
        }
        if name.eq_ignore_ascii_case("agent-sdk") {
            if !profile::package_version_ok(value) {
                return None;
            }
            stable.push(field);
        } else {
            parsed.request_fields.push((*field).to_owned());
        }
    }
    if !parsed.request_fields.is_empty() {
        parsed.profile_user_agent = format!("{prefix}({})", stable.join(","));
    }
    Some(parsed)
}

/// An HTTP token: field names are tokens, values stay opaque.
fn token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `X-App` values Claude Code sends: `cli`, and `cli-bg` from background sessions.
fn native_x_app(value: &str) -> bool {
    matches!(value, "cli" | "cli-bg")
}

fn has_claude_code_beta(headers: &HeaderMap) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|b| b.trim() == "claude-code-20250219")
}

/// `isValidUserID`: Claude Code's JSON metadata.user_id.
pub(crate) fn valid_user_id(user_id: &str) -> bool {
    let Ok(serde_json::Value::Object(v)) = serde_json::from_str::<serde_json::Value>(user_id) else {
        return false;
    };
    let field = |k: &str| match v.get(k) {
        None | Some(serde_json::Value::Null) => Some(""),
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let (Some(device), Some(session), Some(account)) = (field("device_id"), field("session_id"), field("account_uuid"))
    else {
        return false;
    };
    device.len() == 64
        && device.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && uuid::Uuid::parse_str(session).is_ok()
        && (account.is_empty() || uuid::Uuid::parse_str(account).is_ok())
}

pub(crate) fn detect(headers: &HeaderMap, payload: &str, count_tokens: bool, settings: &Settings) -> Detection {
    let user_agent = raw_header(headers, "user-agent");
    let entrypoint = user_agent_entrypoint(user_agent);
    let x_app = native_x_app(raw_header(headers, "x-app"));
    // A newer release counts only with its own well-formed SDK and runtime versions:
    // otherwise its forwarded User-Agent would go upstream with baseline versions.
    let ua = profile::accepted_user_agent(headers, user_agent, settings);
    let betas = has_claude_code_beta(headers);
    let user_id = rawjson::get(payload, "metadata.user_id");
    let metadata_user_id = user_id.kind() == gjson::Kind::String && valid_user_id(user_id.str());
    let native = NATIVE_ENTRYPOINTS.contains(&entrypoint.as_str());
    let standard = x_app && ua && betas && (count_tokens || metadata_user_id);
    let helper = native
        && !count_tokens
        && entrypoint == "cli"
        && !betas
        && x_app
        && ua
        && metadata_user_id
        && helper_profile(headers, payload, settings);
    Detection {
        confirmed: (standard || helper) && native,
        helper_profile: helper,
        entrypoint,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Minimal,
    Structured,
    Title280,
}

fn helper_betas(redact: bool, trailing: &[&str]) -> String {
    let mut b = vec!["oauth-2025-04-20", "interleaved-thinking-2025-05-14"];
    if redact {
        b.push("redact-thinking-2026-02-12");
    }
    b.extend([
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    b.extend(trailing);
    b.join(",")
}

fn measured_beta_shape(betas: &str) -> Option<Shape> {
    let so = "structured-outputs-2025-12-15";
    let profiles = [
        (helper_betas(true, &[]), Shape::Minimal),
        (helper_betas(false, &[]), Shape::Minimal),
        (
            helper_betas(true, &["advisor-tool-2026-03-01", so, "cache-diagnosis-2026-04-07"]),
            Shape::Structured,
        ),
        (
            helper_betas(true, &[so, "fallback-credit-2026-06-01"]),
            Shape::Structured,
        ),
        (helper_betas(true, &[so]), Shape::Structured),
        (helper_betas(false, &[so]), Shape::Structured),
        (
            helper_betas(
                true,
                &[
                    so,
                    "server-side-fallback-2026-06-01",
                    "fallback-credit-2026-06-01",
                    "cache-diagnosis-2026-04-07",
                ],
            ),
            Shape::Title280,
        ),
    ];
    profiles.into_iter().find(|(p, _)| p == betas).map(|(_, s)| s)
}

fn joined_betas(headers: &HeaderMap) -> String {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn helper_profile(headers: &HeaderMap, payload: &str, settings: &Settings) -> bool {
    let Some(shape) = measured_beta_shape(&joined_betas(headers)) else {
        return false;
    };
    body_shape(payload) == Some(shape) && helper_headers(headers, settings) && helper_session(headers, payload)
}

fn helper_headers(headers: &HeaderMap, settings: &Settings) -> bool {
    let expected = [
        ("accept", "application/json"),
        ("content-type", "application/json"),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-timeout", "600"),
        ("anthropic-version", super::ANTHROPIC_VERSION),
        ("anthropic-dangerous-direct-browser-access", "true"),
    ];
    if expected.iter().any(|(n, v)| raw_header(headers, n) != *v) {
        return false;
    }
    if [
        "x-stainless-package-version",
        "x-stainless-runtime-version",
        "x-stainless-os",
        "x-stainless-arch",
    ]
    .iter()
    .any(|n| raw_header(headers, n).is_empty())
    {
        return false;
    }
    let baseline = Profile::default_for(settings);
    let user_agent = raw_header(headers, "user-agent");
    let candidate = (
        profile::version(user_agent),
        raw_header(headers, "x-stainless-package-version"),
        raw_header(headers, "x-stainless-runtime-version"),
    );
    let exact = candidate.0.is_some()
        && candidate.0 == profile::version(&baseline.user_agent)
        && candidate.1 == baseline.package_version
        && candidate.2 == baseline.runtime_version;
    // A newer release's helper is confirmed with its own versions, as its main requests
    // are; the measured beta and body shapes still guard.
    let newer = profile::forwards_own_versions(user_agent, settings) && profile::stainless_versions_ok(headers);
    if !exact && !newer {
        return false;
    }
    if !raw_header(headers, "x-stainless-async").is_empty()
        || raw_header(headers, "accept-encoding") != "gzip, deflate, br, zstd"
    {
        return false;
    }
    let request_id = raw_header(headers, "x-client-request-id");
    request_id.is_empty() || uuid::Uuid::parse_str(request_id).is_ok()
}

fn helper_session(headers: &HeaderMap, payload: &str) -> bool {
    let metadata = rawjson::get(payload, "metadata");
    if metadata.kind() != gjson::Kind::Object || !keys_exactly(metadata.json(), &["user_id"]) {
        return false;
    }
    let user_id = metadata.get("user_id");
    if user_id.kind() != gjson::Kind::String || !valid_user_id(user_id.str()) {
        return false;
    }
    let identity = user_id.str();
    if !keys_exactly(identity, &["device_id", "account_uuid", "session_id"])
        && !keys_exactly(
            identity,
            &["device_id", "account_uuid", "session_id", "parent_session_id"],
        )
    {
        return false;
    }
    raw_header(headers, "x-claude-code-session-id") == gjson::get(identity, "session_id").str()
}

/// `claudeJSONObjectHasKeys`: valid JSON object whose keys are exactly `want`, in order.
pub(crate) fn keys_exactly(raw: &str, want: &[&str]) -> bool {
    if !gjson::valid(raw) || gjson::parse(raw).kind() != gjson::Kind::Object {
        return false;
    }
    let mut keys = Vec::new();
    gjson::parse(raw).each(|k, _| {
        keys.push(k.str().to_owned());
        true
    });
    keys.len() == want.len() && keys.iter().zip(want).all(|(a, b)| a == b)
}

fn body_shape(payload: &str) -> Option<Shape> {
    let minimal = ["model", "max_tokens", "messages", "metadata"];
    let structured = [
        "model",
        "messages",
        "system",
        "tools",
        "metadata",
        "max_tokens",
        "thinking",
        "temperature",
        "output_config",
        "stream",
    ];
    let title = ["model", "max_tokens", "messages", "metadata", "output_config"];
    let shape = if keys_exactly(payload, &minimal) {
        Shape::Minimal
    } else if keys_exactly(payload, &structured) {
        Shape::Structured
    } else if keys_exactly(payload, &title) {
        Shape::Title280
    } else {
        return None;
    };
    let get = |p: &'static str| rawjson::get(payload, p);
    let max_tokens = get("max_tokens");
    if get("model").str() != HELPER_MODEL || max_tokens.kind() != gjson::Kind::Number {
        return None;
    }
    let messages = get("messages");
    if messages.kind() != gjson::Kind::Array || messages.array().len() != 1 {
        return None;
    }
    let message = messages.get("0");
    if !keys_exactly(message.json(), &["role", "content"]) || message.get("role").str() != "user" {
        return None;
    }
    let content = message.get("content");
    let schema_object = |oc: &gjson::Value| {
        let format = oc.get("format");
        keys_exactly(oc.json(), &["format"])
            && keys_exactly(format.json(), &["type", "schema"])
            && format.get("type").str() == "json_schema"
    };
    match shape {
        Shape::Minimal => (max_tokens.json() == "1" && content.kind() == gjson::Kind::String).then_some(shape),
        Shape::Title280 => {
            let oc = get("output_config");
            let schema = oc.get("format.schema");
            (max_tokens.json() == "80"
                && content.kind() == gjson::Kind::String
                && schema_object(&oc)
                && keys_exactly(schema.json(), &["type"])
                && schema.get("type").str() == "object")
                .then_some(shape)
        }
        Shape::Structured => {
            let blocks = content.array();
            if content.kind() != gjson::Kind::Array || blocks.len() != 1 {
                return None;
            }
            if !keys_exactly(blocks[0].json(), &["type", "text"]) || blocks[0].get("type").str() != "text" {
                return None;
            }
            let system = get("system");
            let sys = system.array();
            if system.kind() != gjson::Kind::Array
                || sys.len() != 3
                || sys
                    .iter()
                    .any(|b| !keys_exactly(b.json(), &["type", "text"]) || b.get("type").str() != "text")
            {
                return None;
            }
            let billing = sys[0].get("text");
            if !billing.str().starts_with("x-anthropic-billing-header:")
                || !billing_has_cch(billing.str())
                || !sys[1].get("text").str().starts_with("You are Claude Code")
            {
                return None;
            }
            let tools = get("tools");
            if tools.kind() != gjson::Kind::Array || !tools.array().is_empty() {
                return None;
            }
            let thinking = get("thinking");
            if !keys_exactly(thinking.json(), &["type"]) || thinking.get("type").str() != "disabled" {
                return None;
            }
            let oc = get("output_config");
            let schema = oc.get("format.schema");
            let props = schema.get("properties");
            let title = props.get("title");
            let required = schema.get("required");
            let ok = schema_object(&oc)
                && keys_exactly(
                    schema.json(),
                    &["type", "properties", "required", "additionalProperties"],
                )
                && schema.get("type").str() == "object"
                && keys_exactly(props.json(), &["title"])
                && keys_exactly(title.json(), &["type"])
                && title.get("type").str() == "string"
                && required.kind() == gjson::Kind::Array
                && required.array().len() == 1
                && required.get("0").str() == "title"
                && schema.get("additionalProperties").kind() == gjson::Kind::False
                && max_tokens.json() == "32000"
                && get("temperature").json() == "1"
                && get("stream").kind() == gjson::Kind::True;
            ok.then_some(shape)
        }
    }
}

/// Five lowercase hex digits followed by `;` after ` cch=`.
pub(crate) fn billing_has_cch(billing: &str) -> bool {
    let Some(marker) = billing.find(" cch=") else {
        return false;
    };
    let start = marker + 5;
    let end = start + 5;
    end < billing.len()
        && billing.as_bytes()[end] == b';'
        && billing.as_bytes()[start..end]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
            .collect()
    }

    fn user_id() -> String {
        let identity = format!(
            r#"{{"device_id":"{}","account_uuid":"","session_id":"{SESSION}"}}"#,
            "a".repeat(64)
        );
        serde_json::to_string(&identity).unwrap()
    }

    /// A Haiku session-title helper in the measured Title280 shape.
    fn helper(user_agent: &str, package: &str, runtime: &str) -> (HeaderMap, String) {
        let betas = helper_betas(
            true,
            &[
                "structured-outputs-2025-12-15",
                "server-side-fallback-2026-06-01",
                "fallback-credit-2026-06-01",
                "cache-diagnosis-2026-04-07",
            ],
        );
        let mut pairs = vec![
            ("user-agent", user_agent),
            ("x-app", "cli"),
            ("accept", "application/json"),
            ("content-type", "application/json"),
            ("x-stainless-lang", "js"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-timeout", "600"),
            ("x-stainless-os", "MacOS"),
            ("x-stainless-arch", "arm64"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("anthropic-beta", betas.as_str()),
            ("x-claude-code-session-id", SESSION),
        ];
        if !package.is_empty() {
            pairs.push(("x-stainless-package-version", package));
        }
        if !runtime.is_empty() {
            pairs.push(("x-stainless-runtime-version", runtime));
        }
        let body = format!(
            r#"{{"model":"{HELPER_MODEL}","max_tokens":80,"messages":[{{"role":"user","content":"title"}}],"metadata":{{"user_id":{}}},"output_config":{{"format":{{"type":"json_schema","schema":{{"type":"object"}}}}}}}}"#,
            user_id()
        );
        (headers(&pairs), body)
    }

    /// A newer release's helper is confirmed with its own versions while its measured
    /// shape holds, like its main requests; without valid versions it is cloaked.
    #[test]
    fn newer_release_helpers_follow_the_main_requests() {
        let s = Settings::default();
        let confirmed = |ua: &str, package: &str, runtime: &str| {
            let (h, body) = helper(ua, package, runtime);
            let d = detect(&h, &body, false, &s);
            d.confirmed && d.helper_profile
        };
        assert!(
            confirmed("claude-cli/2.1.321 (external, cli)", "0.128.0", "v26.12.1"),
            "baseline"
        );
        assert!(confirmed("claude-cli/2.2.3 (external, cli)", "0.120.4", "v27.0.1"));
        assert!(!confirmed("claude-cli/2.2.3 (external, cli)", "", "v27.0.1"));
        assert!(!confirmed("claude-cli/2.2.3 (external, cli)", "0.120", "v27.0.1"));
        // The baseline release still needs the exact baseline tuple (Go).
        assert!(!confirmed("claude-cli/2.1.321 (external, cli)", "0.120.4", "v27.0.1"));
        // A changed shape falls back to cloaking.
        let (h, body) = helper("claude-cli/2.2.3 (external, cli)", "0.120.4", "v27.0.1");
        let body = body.replace(r#""max_tokens":80"#, r#""max_tokens":81"#);
        assert!(!detect(&h, &body, false, &s).confirmed);
    }

    /// A standard request from a newer release is confirmed only with both Stainless
    /// version headers well formed; the baseline release does not need them.
    #[test]
    fn newer_release_needs_valid_stainless_versions() {
        let s = Settings::default();
        let body = format!(
            r#"{{"model":"claude-sonnet-4-6","metadata":{{"user_id":{}}},"messages":[]}}"#,
            user_id()
        );
        let confirmed = |ua: &str, versions: &[(&str, &str)]| {
            let mut pairs = vec![
                ("user-agent", ua),
                ("x-app", "cli"),
                ("anthropic-beta", "claude-code-20250219"),
            ];
            pairs.extend_from_slice(versions);
            detect(&headers(&pairs), &body, false, &s).confirmed
        };
        let valid = [
            ("x-stainless-package-version", "0.120.4"),
            ("x-stainless-runtime-version", "v27.0.1"),
        ];
        assert!(confirmed("claude-cli/2.2.3 (external, cli)", &valid));
        assert!(!confirmed("claude-cli/2.2.3 (external, cli)", &valid[..1]));
        assert!(!confirmed(
            "claude-cli/2.2.3 (external, cli)",
            &[
                ("x-stainless-package-version", "0.120.4"),
                ("x-stainless-runtime-version", "27.0.1")
            ]
        ));
        assert!(confirmed("claude-cli/2.1.321 (external, cli)", &[]));
        // Forwarding reads each header's first value: an empty first value with a valid
        // second one would send the baseline SDK version under the 2.2.3 User-Agent.
        let mut h = headers(&[
            ("user-agent", "claude-cli/2.2.3 (external, cli)"),
            ("x-app", "cli"),
            ("anthropic-beta", "claude-code-20250219"),
            ("x-stainless-runtime-version", "v27.0.1"),
        ]);
        h.append("x-stainless-package-version", "".parse().unwrap());
        h.append("x-stainless-package-version", "0.120.4".parse().unwrap());
        assert!(!detect(&h, &body, false, &s).confirmed);
    }

    /// With a stabilized profile nothing newer is forwarded, so Go's rules apply: a
    /// 2.1.330 request is Claude Code without Stainless headers, and its helper is
    /// confirmed only with the exact baseline tuple.
    #[test]
    fn stabilized_profiles_keep_go_detection() {
        let mut s = Settings::default();
        s.header_defaults.stabilize_device_profile = true;
        let body = format!(
            r#"{{"model":"claude-sonnet-4-6","metadata":{{"user_id":{}}},"messages":[]}}"#,
            user_id()
        );
        let h = headers(&[
            ("user-agent", "claude-cli/2.1.330 (external, cli)"),
            ("x-app", "cli"),
            ("anthropic-beta", "claude-code-20250219"),
        ]);
        assert!(detect(&h, &body, false, &s).confirmed);
        assert!(detect(&h, &body, true, &s).confirmed, "count_tokens");
        let (h, body) = helper("claude-cli/2.1.330 (external, cli)", "0.120.4", "v27.0.1");
        assert!(!detect(&h, &body, false, &s).confirmed);
    }

    /// Headers of a standard request from a release newer than the baseline.
    fn standard(user_agent: &str, x_app: &str) -> HeaderMap {
        headers(&[
            ("user-agent", user_agent),
            ("x-app", x_app),
            ("anthropic-beta", "claude-code-20250219"),
            ("x-stainless-package-version", "0.130.2"),
            ("x-stainless-runtime-version", "v26.14.0"),
        ])
    }

    fn standard_body() -> String {
        format!(
            r#"{{"model":"claude-sonnet-4-6","metadata":{{"user_id":{}}},"messages":[]}}"#,
            user_id()
        )
    }

    /// Claude Code background sessions send `X-App: cli-bg`; they are as native as `cli`
    /// on Messages, count_tokens and the measured helpers. Other values are not.
    #[test]
    fn background_sessions_are_native_claude_code() {
        let s = Settings::default();
        let ua = "claude-cli/2.1.330 (external, cli)";
        let body = standard_body();
        assert!(detect(&standard(ua, "cli-bg"), &body, false, &s).confirmed, "messages");
        assert!(
            detect(&standard(ua, "cli-bg"), r#"{"messages":[]}"#, true, &s).confirmed,
            "count_tokens"
        );
        let (mut h, helper_body) = helper(ua, "0.130.2", "v26.14.0");
        h.insert("x-app", "cli-bg".parse().unwrap());
        let d = detect(&h, &helper_body, false, &s);
        assert!(d.confirmed && d.helper_profile, "helper");
        for other in ["cli-other", "web", "sdk", "CLI-BG", ""] {
            assert!(!detect(&standard(ua, other), &body, false, &s).confirmed, "{other}");
        }
    }

    /// Request fields after the entrypoint (`workload/cron`) keep a request native;
    /// every other native guard still applies.
    #[test]
    fn user_agent_request_fields_keep_native_detection() {
        let s = Settings::default();
        let body = standard_body();
        for ua in [
            "claude-cli/2.1.330 (external, cli, workload/cron)",
            "claude-cli/2.1.330 (external, cli, workload/cron, agent-sdk/0.2.133)",
            "claude-cli/2.1.330 (external, cli, agent-sdk/0.2.133, workload/cron)",
            "claude-cli/2.1.330 (external, cli, future-feature/value, workload/background)",
        ] {
            let d = detect(&standard(ua, "cli"), &body, false, &s);
            assert!(d.confirmed && d.entrypoint == "cli", "{ua}");
            assert!(
                detect(&standard(ua, "cli"), r#"{"messages":[]}"#, true, &s).confirmed,
                "{ua} count_tokens"
            );
        }
        for refused in [
            "claude-cli/2.1.279 (external, cli, workload/cron)",
            "claude-cli/3.0.0 (external, cli, workload/cron)",
            "claude-cli/2.1.330 (external, other-client, workload/cron)",
            "claude-cli/2.1.330 (external, cli, foo)",
        ] {
            assert!(
                !detect(&standard(refused, "cli"), &body, false, &s).confirmed,
                "{refused}"
            );
        }
        let ua = "claude-cli/2.1.330 (external, cli, workload/cron)";
        let mut h = standard(ua, "cli");
        h.remove("anthropic-beta");
        assert!(!detect(&h, &body, false, &s).confirmed, "missing beta");
        let mut h = standard(ua, "cli");
        h.remove("x-app");
        assert!(!detect(&h, &body, false, &s).confirmed, "missing x-app");
        assert!(
            !detect(&standard(ua, "cli"), "{}", false, &s).confirmed,
            "missing metadata"
        );
    }

    #[test]
    fn user_agent_parsing_separates_request_fields() {
        for (ua, entrypoint, profile) in [
            ("claude-cli/2.1.312 (external, cli)", "cli", None),
            (
                "claude-cli/2.1.312 (external, sdk-ts, agent-sdk/0.2.133)",
                "sdk-ts",
                None,
            ),
            (
                "claude-cli/2.1.312 (external, cli, workload/cron)",
                "cli",
                Some("claude-cli/2.1.312 (external, cli)"),
            ),
            (
                "claude-cli/2.1.312 (external, sdk-ts, workload/cron, agent-sdk/0.2.133, future-feature/value)",
                "sdk-ts",
                Some("claude-cli/2.1.312 (external, sdk-ts, agent-sdk/0.2.133)"),
            ),
            (
                "claude-cli/2.1.312 (external, cli, future_field.v2/value/path)",
                "cli",
                Some("claude-cli/2.1.312 (external, cli)"),
            ),
        ] {
            let parsed = parse_user_agent(ua).unwrap_or_else(|| panic!("{ua}"));
            assert_eq!(parsed.version, (2, 1, 312), "{ua}");
            assert_eq!(parsed.entrypoint, entrypoint, "{ua}");
            assert_eq!(parsed.profile_user_agent, profile.unwrap_or(ua), "{ua}");
            assert_eq!(user_agent_entrypoint(ua), entrypoint, "{ua}");
        }
    }

    #[test]
    fn malformed_user_agents_are_refused() {
        for ua in [
            "claude-cli/2.1.312(external, cli)",
            "claude-cli/2.1.312-extra (external, cli)",
            "claude-cli/999999999999999999999999.1.0 (external, cli)",
            "claude-cli/2.1.312 (external, )",
            "claude-cli/2.1.312 (external, cli, foo)",
            "claude-cli/2.1.312 (external, cli, /cron)",
            "claude-cli/2.1.312 (external, cli, workload/)",
            "claude-cli/2.1.312 (external, cli, bad name/value)",
            "claude-cli/2.1.312 (external, cli, workload/(cron))",
            "claude-cli/2.1.312 (external, cli, workload/cron, WORKLOAD/background)",
            "claude-cli/2.1.312 (external, cli, agent-sdk/dev)",
            "claude-cli/2.1.312 (external, cli, agent-sdk/0.2.133, agent-sdk/0.2.134)",
            "claude-cli/2.1.312 (external, cli, workload/cron) trailing",
            "claude-cli/2.1.312 (external, cli, workload/cr\non)",
            "claude-cli/2.1.312 (external, cli)\r\n",
        ] {
            assert!(parse_user_agent(ua).is_none(), "{ua:?}");
        }
    }
}
