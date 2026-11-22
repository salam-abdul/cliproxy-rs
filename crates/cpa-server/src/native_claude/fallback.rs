//! Quota fallback: a request that native routing refuses because every account is out of
//! quota is served by the translating proxy on the model mapped to the requested Claude
//! model.

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
        let entries = Option::<std::collections::BTreeMap<String, String>>::deserialize(deserializer)?;
        let mut mapping = Vec::new();
        for (prefix, target) in entries.unwrap_or_default() {
            if !prefix.starts_with("claude-") {
                return Err(D::Error::custom(format!(
                    "quotaFallback key {prefix:?} must start with \"claude-\""
                )));
            }
            let target = target.trim();
            if target.is_empty() {
                return Err(D::Error::custom(format!("quotaFallback {prefix:?} names no model")));
            }
            mapping.push((prefix, target.to_owned()));
        }
        Ok(Self(mapping))
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

/// Client headers that carry a credential. A provider entry's header templates and
/// payload rules can copy client headers, so none of these is handed to dispatch.
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

/// Why a mapped model did not answer. The 429 stands in every case.
#[derive(Clone, Copy)]
enum Failure {
    /// The translating proxy serves no such model.
    UnknownModel,
    /// A provider that would serve the model is not one a fallback may use.
    ProviderNotAllowed,
    /// The request reached dispatch and ended without output.
    NoOutput,
}

impl Failure {
    fn as_str(self) -> &'static str {
        match self {
            Self::UnknownModel => "unknown_model",
            Self::ProviderNotAllowed => "provider_not_allowed",
            Self::NoOutput => "no_output",
        }
    }
}

/// `raw` with its `model` replaced by `target` and every other byte kept.
pub(super) fn body(raw: &[u8], target: &str) -> Option<axum::body::Bytes> {
    cpa_common::json::try_set_str(raw, "model", target).ok().map(Into::into)
}

impl State {
    /// Serves a quota-refused request on its mapped model and returns the response.
    /// Otherwise records `rejected`, the request's rejection event, and returns `None`:
    /// the caller answers as it would without a fallback. Writes no routing state.
    pub(super) async fn fall_back(self: &Arc<Self>, refused: Refused<'_>, mut rejected: Value) -> Option<Response> {
        let attempt = match self.config.quota_fallback.target(refused.model) {
            Some(target) if self.out_of_quota(refused.model).await => {
                Some((target, self.dispatch(&refused, target).await))
            }
            _ => None,
        };
        match attempt {
            Some((target, Ok(response))) => {
                self.event(json!({"type":"fallback","requestId":refused.request_id,"session":refused.sid,"model":refused.model,"fallbackModel":target,"outcome":"fallback_served","status":200,"attempts":refused.attempts}));
                return Some(response);
            }
            Some((target, Err(failure))) => {
                rejected["fallbackModel"] = json!(target);
                rejected["fallbackFailure"] = json!(failure.as_str());
            }
            None => {}
        }
        self.event(rejected);
        None
    }

    /// The translating proxy's answer to `refused` on `target`. An error means no output
    /// was produced, so the caller can still return the 429 with its reset headers.
    async fn dispatch(&self, refused: &Refused<'_>, target: &str) -> Result<Response, Failure> {
        use crate::dispatch::{self, Done};
        let body = body(refused.raw, target).ok_or(Failure::NoOutput)?;
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
        // A provider outside this set would answer with thinking blocks Anthropic rejects
        // later, and a `claude` provider would serve the request outside native admission.
        let providers = dispatch::providers(&self.rt, &call).map_err(|_| Failure::UnknownModel)?;
        if !providers.iter().all(|p| cpa_exec::translates_portable_output(p)) {
            return Err(Failure::ProviderNotAllowed);
        }
        // Its own trace ID: the Claude attempts are already recorded under the request's.
        let trace = dispatch::Trace::with_request_id(None);
        // Not `dispatch::serve`: its keep-alive commits a 200 before the result exists and
        // its header passthrough would replace the 429's reset headers.
        match dispatch::run_with_bootstrap_retries(&self.rt, call, &trace).await {
            Ok(Done::Buffered { body, .. }) => Ok(crate::respond::json(200, "application/json", body)),
            Ok(Done::Stream { first, rest, .. }) => Ok(crate::respond::sse(crate::respond::stream(
                first,
                rest,
                crate::claude::ClaudeSse,
                crate::respond::keepalive(&self.rt.config()),
            ))),
            Err(_) => Err(Failure::NoOutput),
        }
    }

    /// Whether every enabled account is confirmed out of quota for `model`, and at least
    /// one is enabled. An account counts only when its usage reading or its last response
    /// headers name a used-up window with a reset still ahead, and allowed paid overage
    /// does not read as available. An unreadable reading confirms nothing: a usage read
    /// fails for a revoked sign-in as it does for an outage. Cooldowns are not consulted,
    /// so an account that has quota but is resting keeps this false.
    pub(super) async fn out_of_quota(self: &Arc<Self>, model: &str) -> bool {
        let Ok(accounts) = self.accounts().await else {
            return false;
        };
        let enabled: Vec<Account> = accounts.into_iter().filter(|a| a.enabled).collect();
        if enabled.is_empty() {
            return false;
        }
        let overage = self.allow_extra_usage();
        let at = now();
        self.quota_accounts(enabled).await.iter().all(|a| {
            let limited = quota::recovery(&a.quota, model, at).reset().is_some()
                || self
                    .rt
                    .executors
                    .claude
                    .quota()
                    .snapshot(a.quota_id())
                    .is_some_and(|snapshot| quota::header_recovery(&snapshot.signals, at).reset().is_some());
            limited && !(overage && quota::extra_available(&a.quota))
        })
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

    #[test]
    fn null_is_no_mapping_and_targets_are_trimmed() {
        assert_eq!(mapping(Value::Null).unwrap().target("claude-opus-test"), None);
        let padded = mapping(json!({"claude-opus-":" local-test "})).unwrap();
        assert_eq!(padded.target("claude-opus-test"), Some("local-test"));
    }
}
