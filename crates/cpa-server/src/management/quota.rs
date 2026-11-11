use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_common::wirejson::{self as pjson, Node};
use cpa_core::config::credentials;
use cpa_core::credential::Credential;
use serde_json::{Value, json};

use super::Management;
use crate::quota_resets::{Account, Provider, Refusal};

fn map_json(status: StatusCode, value: &Value) -> Response {
    go_json(status, crate::gojson::sorted(value).into_bytes())
}

fn go_json(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

fn fail(status: StatusCode, message: &str) -> Response {
    map_json(status, &json!({"error": message}))
}

/// Go `credentialQuotaRequest` as gin's `ShouldBindJSON` fills it: exact field names
/// first, then case-insensitive in struct order; `None` when binding fails.
#[derive(Default)]
struct QuotaBody {
    auth_index: [Option<String>; 3],
    plugin_id: String,
    provider: String,
}

impl QuotaBody {
    /// gin `ShouldBindJSON`: `None` when binding fails.
    fn decode(body: &[u8]) -> Option<Self> {
        let (out, ok) = Self::bind(body);
        ok.then_some(out)
    }

    /// `json.Decoder.Decode` into `credentialQuotaRequest`: the first JSON value only;
    /// exact field names first, then case-insensitive in struct order. A type error
    /// leaves that field as it was and decoding goes on (Go reports it at the end);
    /// a syntax error yields nothing.
    fn bind(body: &[u8]) -> (Self, bool) {
        const FIELDS: [&str; 5] = ["auth_index", "authIndex", "AuthIndex", "plugin_id", "provider"];
        let mut out = Self::default();
        let fields = match pjson::parse(first_value(body)) {
            Ok(mut node) => match &mut node {
                Node::Object(fields) => std::mem::take(fields),
                Node::Null => return (out, true),
                _ => return (out, false),
            },
            Err(_) => return (out, false),
        };
        let mut ok = true;
        for (key, mut v) in fields {
            let field = FIELDS
                .iter()
                .position(|f| *f == key)
                .or_else(|| FIELDS.iter().position(|f| f.eq_ignore_ascii_case(&key)));
            match (field, &mut v) {
                (Some(i @ 0..=2), Node::String(s)) => out.auth_index[i] = Some(std::mem::take(s)),
                (Some(i @ 0..=2), Node::Null) => out.auth_index[i] = None,
                (Some(3), Node::String(s)) => out.plugin_id = std::mem::take(s),
                (Some(4), Node::String(s)) => out.provider = std::mem::take(s),
                (Some(_), Node::Null) | (None, _) => {}
                (Some(_), _) => ok = false,
            }
        }
        (out, ok)
    }

    /// Go `resolveAuthIndex`.
    fn auth_index(&self) -> String {
        self.auth_index
            .iter()
            .flatten()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_owned()
    }
}

/// The bytes of the first JSON value (`json.Decoder` ignores what follows it).
fn first_value(body: &[u8]) -> &[u8] {
    let start = body.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(body.len());
    let rest = &body[start..];
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in rest.iter().enumerate() {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    if depth == 0 {
                        return &rest[..=i];
                    }
                }
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &rest[..=i];
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' | b',' if depth == 0 => return &rest[..i],
            _ => {}
        }
    }
    rest
}

fn credential(state: &Management, index: &str) -> Option<Arc<Credential>> {
    state
        .rt
        .store()
        .snapshot()
        .into_iter()
        .find(|c| credentials::auth_index(c) == index)
}

pub(crate) async fn claude_usage(
    State(state): State<Arc<Management>>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(index) = query.get("auth_index").filter(|s| !s.is_empty()) else {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    };
    let Some(auth) = credential(&state, index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    if auth.provider != "claude" || auth.disabled {
        return fail(StatusCode::BAD_REQUEST, "An enabled Claude credential is required");
    }
    let token = super::api_call::token_for(&auth);
    let Some(client) = super::api_call::client_for(&state, Some(&auth), "") else {
        return fail(StatusCode::BAD_GATEWAY, "Usage transport unavailable");
    };
    match state
        .rt
        .claude_usage
        .get(
            &token,
            &client,
            state.rt.deny_external.load(std::sync::atomic::Ordering::Relaxed),
        )
        .await
    {
        Ok(usage) => map_json(StatusCode::OK, &usage),
        Err(error) => fail(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

pub(crate) async fn providers() -> Response {
    map_json(StatusCode::OK, &json!({"providers": []}))
}

pub(crate) async fn fetch(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Some(body) = QuotaBody::decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = body.auth_index();
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(auth) = credential(&state, &index) else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    if !body.plugin_id.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "plugin_id is not supported");
    }
    if let Some(probe) = auth.metadata.get("quota_probe").and_then(Value::as_object) {
        match super::quota_probe::execute(&state, &auth, probe).await {
            Some(Ok(resp)) => return go_json(StatusCode::OK, cpa_common::wirejson::to_vec(&resp)),
            Some(Err(e)) => return fail(StatusCode::BAD_GATEWAY, &format!("quota probe failed: {e}")),
            None => {}
        }
    }
    fail(
        StatusCode::UNPROCESSABLE_ENTITY,
        "This credential has no usable quota probe",
    )
}

fn refuse(status: StatusCode, message: &str) -> Box<Response> {
    Box::new(fail(status, message))
}

/// The account behind `index` with a usable token and its strict transport, or the
/// refusal to send. Only enabled Claude and Codex sign-ins have resets.
async fn reset_account(state: &Management, index: &str) -> Result<Account, Box<Response>> {
    if index.is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, "auth_index is required"));
    }
    let Some(auth) = credential(state, index) else {
        return Err(refuse(StatusCode::NOT_FOUND, "auth not found"));
    };
    let Some(provider) = Provider::of(&auth.provider).filter(|_| !auth.disabled) else {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "An enabled Claude or Codex account is required",
        ));
    };
    if cpa_core::registry::dynamic::auth_kind(&auth) == Some("apikey") {
        return Err(refuse(StatusCode::BAD_REQUEST, "API-key accounts have no resets"));
    }
    if provider == Provider::Claude && cpa_exec::oauth::setup_token(&auth) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "Setup tokens cannot use resets. Connect Claude through the dashboard sign-in.",
        ));
    }
    let auth = state.rt.ready_credential(&auth.id).await.map_err(|_| {
        refuse(
            StatusCode::BAD_GATEWAY,
            "The account's sign-in could not be refreshed. Sign in again.",
        )
    })?;
    let token = super::api_call::token_for(&auth);
    if token.is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, "The account has no access token"));
    }
    let Some(client) = super::api_call::strict_client_for(state, &auth) else {
        return Err(refuse(StatusCode::BAD_GATEWAY, "The account's proxy cannot be used"));
    };
    let text = |key: &str| {
        auth.str(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let account = Account {
        provider,
        id: auth.id.clone(),
        token,
        client,
        organization: text("organization_uuid"),
        chatgpt_account: text("account_id"),
    };
    Ok(account)
}

fn deny_external(state: &Management) -> bool {
    state.rt.deny_external.load(std::sync::atomic::Ordering::Relaxed)
}

fn json_reply<T: serde::Serialize>(value: &T) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    go_json(StatusCode::OK, body)
}

/// `GET /quota/resets?auth_index=[&fresh=1]`: the account's available resets, from a
/// read at most an hour old, or a new read with `fresh=1`.
pub(crate) async fn resets(
    State(state): State<Arc<Management>>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let index = query.get("auth_index").map(|s| s.trim()).unwrap_or_default();
    let account = match reset_account(&state, index).await {
        Ok(account) => account,
        Err(response) => return *response,
    };
    let fresh = query.get("fresh").is_some_and(|v| v == "1");
    match state
        .rt
        .quota_resets
        .status(&account, fresh, deny_external(&state))
        .await
    {
        Ok(status) => json_reply(&status),
        Err(message) => fail(StatusCode::BAD_GATEWAY, &message),
    }
}

#[derive(serde::Deserialize)]
struct ClaimBody {
    auth_index: String,
    reset_id: String,
    request_id: String,
}

/// `POST /quota/resets`: spends one reset. Any answer from the provider is a 200 whose
/// `outcome` says what happened; an error status means nothing was sent. The claim and
/// its cleanup run to the end even when the dashboard stops waiting.
pub(crate) async fn claim_reset(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<ClaimBody>(&body) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "auth_index, reset_id and request_id are required",
        );
    };
    let account = match reset_account(&state, body.auth_index.trim()).await {
        Ok(account) => account,
        Err(response) => return *response,
    };
    let task = tokio::spawn(async move {
        let claim = state
            .rt
            .quota_resets
            .claim(&account, &body.reset_id, &body.request_id, deny_external(&state))
            .await;
        if let Ok(claim) = &claim {
            settle(&state, &account, claim).await;
        }
        claim
    });
    match task.await {
        Ok(Ok(claim)) => json_reply(&claim),
        Ok(Err(Refusal::Busy)) => fail(StatusCode::CONFLICT, "A reset is already being applied to this account"),
        Ok(Err(Refusal::Invalid(message))) => fail(StatusCode::BAD_REQUEST, message),
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The reset claim stopped unexpectedly",
        ),
    }
}

/// After a claim: refreshes what admission reads. An applied reset refilled the
/// account's limits, so its stored quota readings and cooldowns no longer hold.
async fn settle(state: &Management, account: &Account, claim: &crate::quota_resets::Claim) {
    if account.provider == Provider::Claude {
        state.rt.claude_usage.invalidate(&account.token).await;
    }
    if claim.outcome != crate::quota_resets::Outcome::Applied {
        return;
    }
    match account.provider {
        Provider::Claude => state.rt.executors.claude.quota().forget(&account.id),
        Provider::Codex => state.rt.executors.codex.quota().forget(&account.id),
    }
    let rt = state.rt.clone();
    let id = account.id.clone();
    // Clearing cooldowns may persist them to disk.
    let _ = tokio::task::spawn_blocking(move || rt.store().reset_cooldowns(&id)).await;
    let native = state
        .rt
        .native_state
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .upgrade();
    if let Some(native) = native
        && let Err(error) = native.clear_cooldowns(&account.id).await
    {
        tracing::warn!(%error, "native cooldowns were not cleared after a quota reset");
    }
}
