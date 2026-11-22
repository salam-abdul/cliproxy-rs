//! Quota fallback: a request that native routing refuses because no account has quota is
//! served by the translating proxy on the model mapped to the requested Claude model.

use super::*;

/// `quotaFallback`: Claude model prefixes, each with the translating-proxy model that
/// serves a matching request when no account has quota. Empty means no fallback.
#[derive(Clone, Default)]
pub(super) struct Mapping(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Mapping {
    /// Rejects an empty prefix, which would match every model, a prefix outside the
    /// `claude-` models this transport serves, and an empty target.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let entries = std::collections::BTreeMap::<String, String>::deserialize(deserializer)?;
        for (prefix, target) in &entries {
            if !prefix.starts_with("claude-") {
                return Err(D::Error::custom(format!(
                    "quotaFallback key {prefix:?} must start with \"claude-\""
                )));
            }
            if target.trim().is_empty() {
                return Err(D::Error::custom(format!("quotaFallback {prefix:?} names no model")));
            }
        }
        Ok(Self(entries.into_iter().collect()))
    }
}

impl Mapping {
    /// The fallback model for `model`: the target of its longest matching prefix.
    pub(super) fn target(&self, model: &str) -> Option<&str> {
        self.0
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, target)| target.as_str())
    }
}

/// Client headers that carry a credential. None reaches a fallback provider: credential
/// header templates, payload rules and Home dispatch can all read client headers.
const CREDENTIAL_HEADERS: [&str; 4] = ["authorization", "x-api-key", "cookie", "proxy-authorization"];

/// A request native routing is about to refuse with the quota 429.
pub(super) struct Refused<'a> {
    pub(super) request_id: &'a str,
    pub(super) sid: &'a str,
    pub(super) model: &'a str,
    pub(super) raw: &'a axum::body::Bytes,
    pub(super) headers: &'a HeaderMap,
    /// Claude accounts tried before the refusal.
    pub(super) attempts: usize,
}

pub(super) enum Fallback {
    /// The mapped model answered: the response to return in place of the 429.
    Served(Response),
    /// The mapped model was tried and produced no output; the 429 stands.
    Failed(String),
    /// No mapping for the model, or the refusal is not about quota.
    Unused,
}

impl Fallback {
    /// The request's `rejected` event, naming a fallback that was tried and failed.
    pub(super) fn annotate(&self, mut event: Value) -> Value {
        if let Self::Failed(model) = self {
            event["fallbackModel"] = json!(model);
            event["fallbackOutcome"] = json!("failed");
        }
        event
    }
}

/// `raw` with its `model` replaced by `target` and every other byte kept.
pub(super) fn body(raw: &[u8], target: &str) -> Option<axum::body::Bytes> {
    cpa_common::json::try_set_str(raw, "model", target).ok().map(Into::into)
}

/// The providers a fallback may use. Others are left out because only these translate
/// with [`crate::dispatch::Call::portable_output`], and a `claude` provider would serve
/// the request outside native admission.
fn allowed(provider: &str) -> bool {
    provider == "gemini" || provider == "openai-compatibility" || provider.starts_with("openai-compatible-")
}

impl State {
    /// Serves a quota-refused request on its mapped model, when it has one and
    /// [`State::quota_blocked`] holds. Reads no cooldown and writes no routing state.
    pub(super) async fn fall_back(self: &Arc<Self>, refused: Refused<'_>) -> Fallback {
        let Some(target) = self.config.quota_fallback.target(refused.model) else {
            return Fallback::Unused;
        };
        if !self.quota_blocked(refused.model).await {
            return Fallback::Unused;
        }
        let Some(response) = self.translate(&refused, target).await else {
            return Fallback::Failed(target.to_owned());
        };
        self.event(json!({"type":"fallback","requestId":refused.request_id,"session":refused.sid,"model":refused.model,"fallbackModel":target,"outcome":"fallback_served","status":200,"attempts":refused.attempts}));
        Fallback::Served(response)
    }

    /// The translating proxy's answer to `refused` on `target`, or `None` when it
    /// produced no output, so the caller can still return the 429 with its reset headers.
    async fn translate(&self, refused: &Refused<'_>, target: &str) -> Option<Response> {
        use crate::dispatch::{self, Done};
        let body = body(refused.raw, target)?;
        let mut headers = refused.headers.clone();
        for name in CREDENTIAL_HEADERS {
            headers.remove(name);
        }
        let fields = crate::claude::peek(&body);
        let call = dispatch::Call {
            entry: cpa_core::format::Format::Claude,
            response: cpa_core::format::Format::Claude,
            operation: cpa_core::exec::Operation::Generate,
            model: target.to_owned(),
            body,
            stream: !matches!(fields.get("stream"), None | Some(Value::Bool(false))),
            alt: None,
            headers,
            caller: cpa_core::exec::Caller {
                principal: String::new(),
                source: "native-fallback",
            },
            forced_provider: None,
            selection_model: None,
            execution_session: None,
            request_path: "/v1/messages".to_owned(),
            peer: None,
            turn: None,
            media: None,
            portable_output: true,
        };
        if !dispatch::providers(&self.rt, &call).ok()?.iter().all(|p| allowed(p)) {
            return None;
        }
        let trace = dispatch::Trace::with_request_id(Some(refused.request_id.to_owned()));
        // Not `dispatch::serve`: its keep-alive commits a 200 before the result exists and
        // its header passthrough would replace the 429's reset headers.
        match dispatch::run_with_bootstrap_retries(&self.rt, call, &trace)
            .await
            .ok()?
        {
            Done::Buffered { body, .. } => Some(crate::respond::json(200, "application/json", body)),
            Done::Stream { first, rest, .. } => Some(crate::respond::sse(crate::respond::stream(
                first,
                rest,
                crate::claude::ClaudeSse,
                crate::respond::keepalive(&self.rt.config()),
            ))),
        }
    }

    /// Whether Claude is unusable for `model` because of quota alone: at least one account
    /// is enabled, and none has included quota or allowed paid overage by its reading. An
    /// unknown reading counts as no quota, as admission refuses it. Cooldowns are not
    /// consulted, so an account that has quota but is resting after a sign-in, overload
    /// or transport failure makes this false.
    pub(super) async fn quota_blocked(self: &Arc<Self>, model: &str) -> bool {
        let Ok(accounts) = self.accounts().await else {
            return false;
        };
        let enabled: Vec<Account> = accounts.into_iter().filter(|a| a.enabled).collect();
        if enabled.is_empty() {
            return false;
        }
        let overage = self.allow_extra_usage();
        self.quota_accounts(enabled)
            .await
            .iter()
            .all(|a| !self.included_available(a, &a.quota, model) && !(overage && quota::extra_available(&a.quota)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(value: Value) -> Result<Mapping, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn longest_prefix_wins() {
        let map = mapping(json!({"claude-":"a","claude-opus-":"b"})).unwrap();
        assert_eq!(map.target("claude-opus-test"), Some("b"));
        assert_eq!(map.target("claude-haiku-x"), Some("a"));
        assert_eq!(mapping(json!({})).unwrap().target("claude-opus-test"), None);
        assert_eq!(Mapping::default().target("claude-opus-test"), None);
        let opus = mapping(json!({"claude-opus-":"b"})).unwrap();
        assert_eq!(opus.target("claude-haiku-x"), None);
    }

    #[test]
    fn empty_or_foreign_keys_are_rejected() {
        assert!(mapping(json!({"":"a"})).is_err());
        assert!(mapping(json!({"claude-opus-":""})).is_err());
        assert!(mapping(json!({"claude-opus-":"  "})).is_err());
        assert!(mapping(json!({"gpt-":"a"})).is_err());
        assert!(mapping(json!({"claude-opus-":7})).is_err());
    }
}
