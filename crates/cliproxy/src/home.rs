//! Home mode (`-home-jwt`): config from Home, credentials dispatched per request,
//! concurrency leases released back (Go cmd/server/main.go's Home branch,
//! sdk/cliproxy/service_home.go and auth/conductor_home.go). The protocol lives in
//! `cpa-home`; this module wires it to the runtime through
//! [`cpa_server::remote::RemoteDispatch`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{ExecError, FailureScope};
use cpa_home::dispatch::{DispatchResponse, decode_concurrency, decode_error, verify_identity};
use cpa_home::registry::{Registry, ScopeSpec};
use cpa_home::release::ReleaseFlusher;
use cpa_home::{Client, CredentialConcurrency, DispatchRequest, HomeConfig};
use cpa_server::Runtime;
use cpa_server::management::Management;
use cpa_server::remote::{
    EndLease, ModelsError, RemoteDispatch, RemoteError, RemoteErrorKind, RemoteGrant, RemoteRequest,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};
use tokio_util::sync::CancellationToken;

/// Go main's 30-second contexts around the Home bootstrap.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
/// Go `homeSubscriberPreAckRetryBackoff`.
const PRE_ACK_RETRY: Duration = Duration::from_millis(100);

fn set(doc: &mut Yaml, path: &[&str], value: Option<Yaml>) {
    let mut node = doc;
    for (i, key) in path.iter().enumerate() {
        if !node.is_mapping() {
            *node = Yaml::Mapping(Mapping::new());
        }
        let Yaml::Mapping(map) = node else { unreachable!() };
        let key = Yaml::String((*key).to_owned());
        if i + 1 == path.len() {
            match value {
                Some(value) => {
                    map.insert(key, value);
                }
                None => {
                    map.remove(&key);
                }
            }
            return;
        }
        node = map.entry(key).or_insert(Yaml::Null);
    }
}

fn get<'a>(doc: &'a Yaml, path: &[&str]) -> Option<&'a Yaml> {
    path.iter().try_fold(doc, |node, key| node.get(*key))
}

/// A Home config payload as this node runs it: Go `ParseConfigBytes`, the listener
/// kept from `base` (or `NormalizeHomePort` at startup), then `forceHomeRuntimeConfig`.
pub fn overlay(raw: &[u8], base: Option<&Config>) -> anyhow::Result<Config> {
    let text = std::str::from_utf8(raw).map_err(|e| anyhow::anyhow!("config payload is not UTF-8: {e}"))?;
    let parsed = Config::parse(text)?;
    let mut doc = parsed.document.clone();
    match base {
        Some(base) => {
            for key in ["host", "port", "tls"] {
                set(
                    &mut doc,
                    &["server", key],
                    get(&base.document, &["server", key]).cloned(),
                );
            }
        }
        None => set(
            &mut doc,
            &["server", "port"],
            Some(Yaml::Number(cpa_home::normalize_home_port(parsed.port).into())),
        ),
    }
    set(&mut doc, &["access", "api-keys"], None);
    set(
        &mut doc,
        &["observability", "usage", "usage-statistics-enabled"],
        Some(Yaml::Bool(true)),
    );
    set(
        &mut doc,
        &["routing", "cooldown", "disable-cooling"],
        Some(Yaml::Bool(true)),
    );
    set(
        &mut doc,
        &["routing", "cooldown", "save-cooldown-status"],
        Some(Yaml::Bool(false)),
    );
    set(
        &mut doc,
        &["oauth", "providers", "aistudio", "ws-auth"],
        Some(Yaml::Bool(false)),
    );
    set(&mut doc, &["management", "allow-remote"], Some(Yaml::Bool(false)));
    set(
        &mut doc,
        &["management", "disable-control-panel"],
        Some(Yaml::Bool(true)),
    );
    set(&mut doc, &["plugins", "store-auth"], None);
    Config::parse(&serde_yaml_ng::to_string(&doc)?)
}

/// Go main's Home bootstrap. The error is the line Go logs before exiting.
pub async fn bootstrap(jwt: &str, disable_discovery: bool) -> Result<(HomeConfig, Config), String> {
    let mut home = tokio::time::timeout(BOOTSTRAP_TIMEOUT, cpa_home::cert::config_from_jwt(jwt))
        .await
        .unwrap_or(Err(cpa_home::Error::Timeout))
        .map_err(|e| format!("invalid -home-jwt: {e}"))?;
    if disable_discovery {
        home.disable_cluster_discovery = true;
    }
    let client = Client::new(home.clone());
    let raw = tokio::time::timeout(BOOTSTRAP_TIMEOUT, client.get_config())
        .await
        .unwrap_or(Err(cpa_home::Error::Timeout));
    // Go closes the bootstrap client once startup is done; the service opens its own.
    client.close();
    let raw = raw.map_err(|e| format!("failed to fetch config from home: {e}"))?;
    let config = overlay(&raw, None).map_err(|e| format!("failed to parse config payload from home: {e:#}"))?;
    Ok((home, config))
}

/// The client and registry of the live Home lifetime (Go `HomeDispatchBundle`).
#[derive(Clone)]
struct Bundle {
    client: Client,
    registry: Registry,
}

/// Go `pickHomeDispatchSelection` behind [`RemoteDispatch`].
pub struct Dispatcher {
    bundle: RwLock<Option<Bundle>>,
    /// For `Executors::supports` (Go `m.Executor(key)`); weak, the runtime owns this.
    rt: std::sync::Weak<Runtime>,
    /// Go `Manager.homeSessionAliases`.
    sessions: cpa_home::session_alias::SessionAliases,
}

fn fail(status: u16, code: &str, message: impl std::fmt::Display) -> ExecError {
    // Go's handlers render `*auth.Error` text, `code: message`. Client errors stop the
    // request; the rest may move to another credential or round.
    let scope = if (400..500).contains(&status) && status != 429 && status != 408 {
        FailureScope::Request
    } else {
        FailureScope::Credential
    };
    ExecError::local(status, scope, format!("{code}: {message}"))
}

/// A plain Home failure (Go `*auth.Error`).
fn reject(status: u16, code: &str, message: impl std::fmt::Display) -> RemoteError {
    RemoteError::plain(fail(status, code, message), code)
}

/// Go `invalidHomeConcurrencyResponse`.
fn invalid_concurrency(message: &str) -> RemoteError {
    reject(502, "invalid_home_concurrency", message)
}

impl Dispatcher {
    pub fn new(rt: &Arc<Runtime>) -> Self {
        Self {
            bundle: RwLock::default(),
            rt: Arc::downgrade(rt),
            sessions: Default::default(),
        }
    }

    /// Go `homeDispatchSessionIDs` once the session is extracted: a fallback that is the
    /// session's parent is sent as the parent; any other fallback names the same
    /// conversation, so both join one alias group whose canonical ID is sent.
    fn dispatch_sessions(&self, primary: &str, fallback: &str, ttl: Duration) -> (String, String) {
        use cpa_common::session::bound_session_identity as bound;
        let (primary, fallback) = (primary.trim(), fallback.trim());
        if primary.is_empty() {
            return (String::new(), String::new());
        }
        let (mut parent, mut alias) = ("", "");
        if !fallback.is_empty() && fallback != primary {
            if cpa_home::session_alias::is_hierarchy_parent(primary, fallback) {
                parent = fallback;
            } else {
                alias = fallback;
            }
        }
        let now = std::time::Instant::now();
        let canonical = self.sessions.canonical(primary, alias, ttl, now);
        let mut parent = if parent.is_empty() || parent == canonical || parent == primary || parent == alias {
            String::new()
        } else {
            let parent = self.sessions.canonical(parent, "", ttl, now);
            if parent == canonical { String::new() } else { parent }
        };
        let canonical = bound(&canonical);
        if !parent.is_empty() {
            parent = bound(&parent);
        }
        if canonical == parent {
            parent.clear();
        }
        (canonical, parent)
    }

    fn install(&self, client: Client, registry: Registry) {
        *self.bundle.write().unwrap_or_else(PoisonError::into_inner) = Some(Bundle { client, registry });
    }

    /// Go `ClearHomeDispatchBundle`: only the lifetime that installed it.
    fn clear(&self, client: &Client) {
        let mut bundle = self.bundle.write().unwrap_or_else(PoisonError::into_inner);
        if bundle.as_ref().is_some_and(|b| b.client.ptr_eq(client)) {
            *bundle = None;
        }
    }

    fn current(&self) -> Option<Bundle> {
        self.bundle.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn pick(&self, request: RemoteRequest) -> Result<RemoteGrant, RemoteError> {
        let Some(Bundle { client, registry }) = self.current() else {
            return Err(reject(503, "home_unavailable", "home dispatch bundle unavailable"));
        };
        if !client.heartbeat_ok() {
            return Err(reject(503, "home_unavailable", "home control center unavailable"));
        }
        let pinned = request.pinned.trim().to_owned();
        if !pinned.is_empty() && request.excluded.contains(&pinned) {
            return Err(reject(
                503,
                "auth_not_found",
                "pinned auth is unavailable in the current retry round",
            ));
        }
        let pending = registry
            .begin_dispatch()
            .map_err(|_| reject(503, "home_unavailable", "home execution registry unavailable"))?;
        let model = request.model.trim().to_owned();
        let mut excluded: Vec<String> = request
            .excluded
            .iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        excluded.sort();
        excluded.dedup();
        // ponytail: no Merkle LCP session matching; Go consults it when no session
        // identifier was sent.
        let ttl = self.rt.upgrade().map_or(cpa_home::session_alias::DEFAULT_TTL, |rt| {
            rt.policy().session_affinity_ttl
        });
        let (session_id, parent_session_id) =
            self.dispatch_sessions(&request.session_id, &request.parent_session_id, ttl);
        let dispatch = DispatchRequest {
            model: model.clone(),
            session_id,
            parent_session_id,
            headers: request.headers.clone(),
            count: request.count,
            credential_policy: request.credential_policy.trim().to_owned(),
            retry_round: Some(request.retry_round.max(0)),
            // Go sends the list only once something was tried.
            excluded_auth_ids: (!excluded.is_empty()).then_some(excluded),
            pinned_auth_id: pinned.clone(),
        };
        let raw = match client.rpop_auth(&dispatch).await {
            Ok(raw) => raw,
            Err(error) => {
                drop(pending);
                return Err(match error {
                    cpa_home::Error::AuthNotFound => reject(503, "auth_not_found", &error),
                    _ => reject(503, "home_unavailable", &error),
                });
            }
        };
        let tuple = match decode_concurrency(&raw) {
            Ok(tuple) => tuple,
            Err((true, _)) => {
                client.abort_ambiguous_dispatch();
                return Err(invalid_concurrency("Home returned malformed concurrency tuple"));
            }
            Err((false, _)) => return Err(reject(502, "invalid_auth", "home returned invalid auth payload")),
        };
        let kind = request.kind.to_owned();
        let spec = |credential_id: String, model: String, accounted: bool| ScopeSpec {
            request_id: request.request_id.clone(),
            credential_id,
            model,
            kind: kind.clone(),
            started_at: SystemTime::now(),
            accounted,
        };
        // Go `homeConcurrencyInstallError` for a registry that refuses the scope.
        let install_failed = |error: cpa_home::registry::RegistryError| {
            client.abort_ambiguous_dispatch();
            reject(
                503,
                "home_unavailable",
                format!("home execution registry unavailable: {error}"),
            )
        };
        // An accounted lease exists in Home from here on; every failure below ends its
        // scope, which releases it.
        let mut pending = Some(pending);
        let scope = match &tuple {
            Some(tuple) => Some(
                registry
                    .install(
                        pending.take().expect("pending"),
                        spec(tuple.credential_id.clone(), tuple.model.clone(), true),
                    )
                    .map_err(install_failed)?,
            ),
            None => None,
        };
        let end_scope = |scope: &Option<cpa_home::registry::Scope>| {
            if let Some(scope) = scope {
                scope.end();
            }
        };
        if let Some(error) = decode_error(&raw) {
            if tuple.is_some() {
                client.abort_ambiguous_dispatch();
                end_scope(&scope);
                return Err(invalid_concurrency(
                    "Home returned both accounted concurrency and an error",
                ));
            }
            // Go's typed Home errors: cooldowns join retry rounds; busy is never
            // retried and exposes its safe `Retry-After`.
            let header = error.retry_after_header();
            let kind = match error.kind {
                cpa_home::dispatch::HomeErrorKind::Plain => RemoteErrorKind::Plain,
                cpa_home::dispatch::HomeErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                } => RemoteErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                },
                cpa_home::dispatch::HomeErrorKind::Busy { .. } => RemoteErrorKind::Busy { header },
            };
            let mut failure = fail(error.status, &error.code, &error.message);
            if let RemoteErrorKind::Cooldown { retry_after, .. } = &kind {
                failure.retry_after = *retry_after;
            }
            return Err(RemoteError {
                error: failure,
                code: error.code.as_str().into(),
                kind,
            });
        }
        let response = match DispatchResponse::parse(&raw) {
            Ok(response) => response,
            Err(message) => {
                end_scope(&scope);
                return Err(reject(502, "invalid_auth", message));
            }
        };
        let observed = response.observed_model(&model).to_owned();
        if let Some(tuple) = &tuple
            && cpa_home::dispatch::valid_concurrency_model_key(&observed).as_deref() != Some(tuple.model.as_str())
        {
            client.abort_ambiguous_dispatch();
            end_scope(&scope);
            return Err(invalid_concurrency(
                "Home concurrency model does not match dispatched model",
            ));
        }
        let auth_id = response.auth_id().trim().to_owned();
        if auth_id.is_empty() {
            end_scope(&scope);
            return Err(reject(502, "invalid_auth", "home returned auth without id"));
        }
        if !pinned.is_empty() && auth_id != pinned {
            end_scope(&scope);
            return Err(reject(
                503,
                "auth_not_found",
                "home returned an auth that does not match the pinned credential",
            ));
        }
        if let Some(tuple) = &tuple
            && let Err(message) = verify_identity(tuple, response.auth_id(), response.auth_index.trim())
        {
            end_scope(&scope);
            return Err(invalid_concurrency(&message));
        }
        let mut credential = match credential(&response, &model) {
            Ok(credential) => credential,
            Err(error) => {
                end_scope(&scope);
                return Err(error);
            }
        };
        // Go `selection.CanonicalSessionID` / `ParentSessionID`: what this pick sent.
        if !dispatch.session_id.is_empty() {
            credential
                .attributes
                .insert(cpa_server::remote::SESSION.into(), dispatch.session_id.clone());
            credential.attributes.insert(
                cpa_server::remote::PARENT_SESSION.into(),
                dispatch.parent_session_id.clone(),
            );
        }
        let supports = |provider: &str| self.rt.upgrade().is_some_and(|rt| rt.executors.supports(provider));
        // Go: an unregistered provider with a base URL runs on the generic
        // OpenAI-compatible executor.
        if !supports(&credential.provider)
            && credential
                .attributes
                .get("base_url")
                .is_some_and(|u| !u.trim().is_empty())
        {
            credential.provider = "openai-compatibility".into();
        }
        if !supports(&credential.provider) {
            end_scope(&scope);
            return Err(reject(502, "executor_not_found", "executor not registered"));
        }
        let scope = match scope {
            Some(scope) => scope,
            None => registry
                .install(
                    pending.take().expect("pending"),
                    spec(auth_id.clone(), observed.clone(), false),
                )
                .map_err(install_failed)?,
        };
        // Go `newHomeDispatchSelection`: the scope's bound resource cancels the
        // execution, so a draining registry stops in-flight work instead of waiting.
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        if scope
            .bind(move || {
                let _ = cancel_tx.send(true);
            })
            .is_err()
        {
            scope.end();
            return Err(reject(503, "home_unavailable", "home execution registry unavailable"));
        }
        let bound = client.limiter_config().cancel_bound();
        let end: EndLease = Box::new(move || {
            let ticket = scope.end_with_release()?;
            Some(Box::pin(
                async move { ticket.wait(bound).await.map_err(|e| e.to_string()) },
            ))
        });
        // Go: Home's request-retry limit applies unless the request is pinned.
        let request_retry = response.request_retry.filter(|r| *r >= 0 && pinned.is_empty());
        Ok(RemoteGrant {
            credential,
            end,
            cancel: Some(cancel),
            request_retry,
            user_api_key: response.user_api_key.trim().to_owned(),
        })
    }
}

impl RemoteDispatch for Dispatcher {
    fn available(&self) -> bool {
        self.current().is_some_and(|b| b.client.heartbeat_ok())
    }

    fn request_log(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let Some(bundle) = self.current().filter(|b| b.client.heartbeat_ok()) else {
                return Ok(());
            };
            bundle
                .client
                .rpush_request_log(&payload)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>> {
        Box::pin(self.pick(request))
    }

    /// Go `loadHomeModelEntries` with `GetModels` (headers and query lower-cased and
    /// joined as Go's `headersToLowerMap` / `queryToLowerMap`).
    fn models(
        &self,
        headers: Vec<(String, String)>,
        query: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>> {
        Box::pin(async move {
            let Some(bundle) = self.current() else {
                return Err(ModelsError::Unavailable);
            };
            let lower = |pairs: &[(String, String)]| {
                cpa_home::client::lower_map(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            };
            bundle
                .client
                .get_models(&lower(&headers), &lower(&query))
                .await
                .map_err(|e| ModelsError::Failed(e.to_string()))
        })
    }
}

/// Go `executorKeyFromAuth`: the provider key executors are registered under.
fn executor_key(provider: &str, label: &str, attributes: &BTreeMap<String, String>) -> String {
    let attr = |key: &str| attributes.get(key).map(|v| v.trim()).unwrap_or_default();
    if !attr("compat_name").is_empty() {
        let key = if attr("provider_key").is_empty() {
            attr("compat_name")
        } else {
            attr("provider_key")
        };
        return cpa_core::config::credentials::openai_compat_provider(key);
    }
    if provider.trim().eq_ignore_ascii_case("openai-compatibility") {
        return cpa_core::config::credentials::openai_compat_provider(label.trim());
    }
    match provider.trim().to_lowercase().as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        other => other.to_owned(),
    }
}

/// The dispatched Go `coreauth.Auth` as a runtime credential. It lives for one lease:
/// nothing persists it and the local scheduler never sees it.
fn credential(response: &DispatchResponse, requested: &str) -> Result<Credential, RemoteError> {
    let auth = &response.auth;
    let text = |key: &str| {
        auth.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut attributes: BTreeMap<String, String> = auth
        .get("attributes")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let metadata: Map<String, Value> = auth
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let provider = text("provider").to_lowercase();
    let label = text("label");
    let key = executor_key(&provider, &label, &attributes);
    if provider.is_empty() || key.is_empty() {
        return Err(reject(502, "invalid_auth", "home returned auth without provider"));
    }
    attributes.insert(cpa_server::remote::PROVIDER.into(), text("provider"));
    let prefix = cpa_core::config::credentials::normalize_prefix(&text("prefix"));
    if !prefix.is_empty() {
        attributes.insert("prefix".into(), prefix);
    }
    if !text("proxy_url").is_empty() {
        attributes.insert("proxy_url".into(), text("proxy_url"));
    }
    // Go `auth.Index = homeAuthIndex`.
    if !response.auth_index.trim().is_empty() {
        attributes.insert(
            cpa_core::config::credentials::HOME_AUTH_INDEX.into(),
            response.auth_index.trim().to_owned(),
        );
    }
    if !response.model.trim().is_empty() {
        attributes.insert(
            cpa_server::remote::UPSTREAM_MODEL.into(),
            response.model.trim().to_owned(),
        );
    }
    // Go `homeForceMappingAliasResult`: the mapping applies when the alias Home mapped
    // from is the requested model (prefix removed, recognized suffixes ignored).
    let canonical = cpa_home::dispatch::canonical_concurrency_model_key;
    let raw_prefix = text("prefix");
    let unprefixed = match requested.strip_prefix(&format!("{raw_prefix}/")) {
        Some(rest) if !raw_prefix.is_empty() => rest,
        _ => requested,
    };
    let original = canonical(&response.original_alias);
    if response.force_mapping && !original.is_empty() && original == canonical(unprefixed) {
        attributes.insert(cpa_server::remote::FORCE_MAPPING.into(), "true".into());
        attributes.insert(
            cpa_server::remote::ORIGINAL_ALIAS.into(),
            response.original_alias.trim().to_owned(),
        );
    }
    let api_key = attributes.get("api_key").is_some_and(|k| !k.trim().is_empty());
    let id = text("id");
    let source = if api_key {
        Source::Config {
            section: "home".into(),
            index: 0,
        }
    } else {
        Source::File(PathBuf::from(
            attributes.get("path").cloned().unwrap_or_else(|| id.clone()),
        ))
    };
    let label = if label.is_empty() {
        metadata
            .get("email")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .unwrap_or(&provider)
            .to_owned()
    } else {
        label
    };
    let mut credential = Credential {
        id,
        provider: key,
        source,
        disabled: auth.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        label,
        attributes,
        metadata,
        revision: 0,
    };
    // Go `attachResolvedHomeModelInfo` inputs, fixed for the lease: Home's definition of
    // the model (`registryModelInfo` is nil without an ID) and the `credential_options`
    // entry for the dispatched upstream model reached through the route model. Only the
    // dispatcher sets these attributes.
    for key in [
        cpa_server::remote::MODEL_INFO,
        cpa_core::config::credentials::HOME_MODEL_OPTIONS,
    ] {
        credential.attributes.remove(key);
    }
    let info = response.model_info.as_ref().filter(|info| {
        info.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
    });
    if let Some(info) = info {
        credential
            .attributes
            .insert(cpa_server::remote::MODEL_INFO.into(), info.to_string());
    }
    // ponytail: without a dispatched model or model info Go uses the attempt's model;
    // a Home pick without either is the route model, which this uses.
    let upstream = match (
        response.model.trim(),
        info.and_then(|i| i.get("id")).and_then(Value::as_str),
    ) {
        ("", Some(id)) => id.trim(),
        ("", None) => requested,
        (model, _) => model,
    };
    if let Some(options) = cpa_home::dispatch::credential_model_options(&credential, upstream, requested) {
        credential.attributes.insert(
            cpa_core::config::credentials::HOME_MODEL_OPTIONS.into(),
            Value::Object(options).to_string(),
        );
    }
    Ok(credential)
}

/// Go `startHomeUsageForwarder`: queued usage records go to Home while the heartbeat
/// holds; a failed push puts the rest back and waits a second.
async fn forward_usage(rt: Arc<Runtime>, client: Client, stop: CancellationToken) {
    let sleep = |wait: Duration| {
        let stop = stop.clone();
        async move {
            tokio::select! {
                _ = stop.cancelled() => false,
                _ = tokio::time::sleep(wait) => true,
            }
        }
    };
    while !stop.is_cancelled() {
        if !client.heartbeat_ok() {
            if !sleep(Duration::from_secs(1)).await {
                return;
            }
            continue;
        }
        let items = rt.usage_queue().pop_oldest(64);
        if items.is_empty() {
            if !sleep(Duration::from_millis(500)).await {
                return;
            }
            continue;
        }
        for (i, item) in items.iter().enumerate() {
            // Go passes the lifetime context to every push: once it is cancelled the
            // push fails, and the rest goes back to the queue.
            if stop.is_cancelled() || client.lpush_usage(item).await.is_err() {
                for rest in &items[i..] {
                    rt.usage_queue().enqueue(rest.clone());
                }
                if !sleep(Duration::from_secs(1)).await {
                    return;
                }
                break;
            }
        }
    }
}

/// Go `startHomeSubscriber` / `runHomeSubscriber`: one Home lifetime after another until
/// `shutdown`, each subscribed to config, publishing dispatch once its first config
/// applied, and releasing leases through one flusher.
pub fn spawn_subscriber(
    home: HomeConfig,
    rt: Arc<Runtime>,
    management: Arc<Management>,
    dispatcher: Arc<Dispatcher>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Go `StartHomeAppLogForwarder` (process-wide logrus hook): bound to each
        // published lifetime, stopped with the subscriber.
        let app_log = cpa_home::applog::Forwarder::start(0);
        let hook = cpa_server::logging::add_hook({
            let app_log = app_log.clone();
            Arc::new(move |e: &cpa_server::logging::Entry<'_>| app_log.fire(e.line, e.level, e.time, e.request_id))
        });
        run_lifetimes(home, rt, management, dispatcher, shutdown, app_log.clone()).await;
        cpa_server::logging::remove_hook(hook);
        app_log.stop().await;
    })
}

async fn run_lifetimes(
    home: HomeConfig,
    rt: Arc<Runtime>,
    management: Arc<Management>,
    dispatcher: Arc<Dispatcher>,
    shutdown: CancellationToken,
    app_log: Arc<cpa_home::applog::Forwarder>,
) {
    let mut registry = Registry::new();
    let mut flusher = ReleaseFlusher::new();
    registry.set_release_sink(Some(flusher.sink()));
    let mut previous: Option<Client> = None;
    let bound = || CredentialConcurrency::default().with_defaults().cancel_bound();
    // Go keeps the publisher settings on the auth manager, across lifetimes.
    let publisher = cpa_home::inflight::PublisherSettings::default();
    while !shutdown.is_cancelled() {
        let client = match &previous {
            Some(client) => client.new_lifetime(),
            None => Client::new(home.clone()),
        };
        client.set_managed_lifetime(true);
        flusher.set_sender(Some(Arc::new(client.clone())));
        let release_stop = CancellationToken::new();
        let release_task = tokio::spawn({
            let flusher = flusher.clone();
            let stop = release_stop.clone();
            async move { flusher.run(stop).await }
        });
        let lifetime = shutdown.child_token();
        let (configs, mut latest) = tokio::sync::watch::channel::<Option<Vec<u8>>>(None);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = tokio::spawn({
            let (rt, management, dispatcher, client, registry, app_log) = (
                rt.clone(),
                management.clone(),
                dispatcher.clone(),
                client.clone(),
                registry.clone(),
                app_log.clone(),
            );
            let (lifetime, published, publisher) = (lifetime.clone(), published.clone(), publisher.clone());
            async move {
                // Go `runHomeConfigWorkerWithSupervisor`: the lifetime's in-flight
                // publisher and usage forwarder, awaited before it is torn down.
                let mut helpers = Vec::new();
                tokio::select! {
                    _ = lifetime.cancelled() => return helpers,
                    ready = ready_rx => if ready.is_err() { return helpers },
                }
                loop {
                    let raw = latest.borrow_and_update().clone();
                    if let Some(raw) = raw {
                        let base = rt.config();
                        match overlay(&raw, Some(base.as_ref())) {
                            Ok(cfg) => {
                                management.publish_home(cfg);
                                if !published.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                    // The KV client goes first: an attempt that
                                    // receives a Home credential must also see
                                    // Home KV for its identities and caches.
                                    cpa_home::set_current(Some(client.clone()));
                                    dispatcher.install(client.clone(), registry.clone());
                                    app_log.bind(&client);
                                    helpers.push(tokio::spawn(cpa_home::inflight::run_publisher(
                                        client.clone(),
                                        registry.clone(),
                                        publisher.clone(),
                                        lifetime.clone(),
                                    )));
                                    helpers.push(tokio::spawn(forward_usage(
                                        rt.clone(),
                                        client.clone(),
                                        lifetime.clone(),
                                    )));
                                }
                            }
                            Err(error) => tracing::warn!("failed to stage home config; retrying: {error:#}"),
                        }
                    }
                    tokio::select! {
                        _ = lifetime.cancelled() => return helpers,
                        changed = latest.changed() => if changed.is_err() { return helpers },
                    }
                }
            }
        });
        let run = client
            .run_config_subscriber_lifetime(
                &lifetime,
                |raw| {
                    let parsed = Config::parse(&String::from_utf8_lossy(raw)).map_err(|e| {
                        tracing::warn!("failed to parse home config payload: {e:#}");
                        e.to_string()
                    })?;
                    CredentialConcurrency::from_document(&parsed.document)
                        .map_err(|e| e.to_string())
                        .and_then(|c| client.set_lifecycle_config(c).map_err(|e| e.to_string()))
                        .inspect_err(|e| tracing::warn!("failed to apply Home lifecycle config: {e}"))?;
                    let in_flight = cpa_home::CredentialInFlight::from_document(&parsed.document);
                    let settings = cpa_home::inflight::PublisherConfig::from_config(&in_flight)
                        .map_err(|e| e.to_string())
                        .inspect_err(|e| tracing::warn!("failed to apply Home in-flight publisher config: {e}"))?;
                    publisher.apply(settings);
                    registry.observe_barrier(client.limiter_config().observation_barrier_revision);
                    configs.send_replace(Some(raw.to_vec()));
                    Ok(())
                },
                move || {
                    let _ = ready_tx.send(());
                },
            )
            .await;
        lifetime.cancel();
        if let Ok(helpers) = worker.await {
            for helper in helpers {
                let _ = helper.await;
            }
        }
        dispatcher.clear(&client);
        cpa_home::clear_current_if(&client);
        app_log.deactivate(&client);
        let bound = client.limiter_config().cancel_bound().max(bound());
        let retry = run.is_err() && !shutdown.is_cancelled();
        if retry {
            release_stop.cancel();
            let _ = release_task.await;
            client.close();
            if let Err(error) = registry.wait_pending(bound).await {
                tracing::error!("failed to settle pending Home dispatches before subscriber replacement: {error}");
                return;
            }
            let error = run.as_ref().err();
            let legacy = error.is_some_and(|e| e.is_legacy_membership_protocol());
            if legacy {
                client.enable_legacy_membership();
            }
            if client.ambiguous_dispatch()
                || error.is_some_and(|e| e.is_membership_takeover_unavailable())
                || legacy
                || client.legacy_membership()
            {
                registry.set_release_sink(None);
                if let Err(error) = registry.drain(bound).await {
                    tracing::error!("failed to drain Home executions after unsafe subscriber replacement: {error}");
                    return;
                }
                client.suppress_takeover();
                registry = Registry::new();
                flusher = ReleaseFlusher::new();
                registry.set_release_sink(Some(flusher.sink()));
            }
            if let Some(error) = error {
                tracing::warn!("home config subscription lifetime ended: {error}");
            }
            if !published.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(PRE_ACK_RETRY) => {}
                }
            }
            previous = Some(client);
            continue;
        }
        let drained = registry.drain(bound).await;
        let flushed = match &drained {
            Ok(()) => flusher.flush_all(bound).await,
            Err(_) => Ok(()),
        };
        release_stop.cancel();
        let _ = release_task.await;
        client.close();
        if let Err(error) = drained {
            tracing::error!("failed to drain Home execution registry: {error}");
        } else if let Err(error) = flushed {
            tracing::error!("failed to flush Home concurrency releases: {error}");
        }
        return;
    }
}

#[cfg(test)]
#[path = "home_tests.rs"]
mod tests;
