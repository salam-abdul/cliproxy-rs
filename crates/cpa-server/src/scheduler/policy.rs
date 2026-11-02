use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    #[default]
    RoundRobin,
    FillFirst,
    WeightedRoundRobin,
    /// cliproxy-rs addition (`soonest-reset`, alias `reset-first`): spend the account
    /// whose weekly window resets soonest first, until it cools or a usage window runs
    /// out, then the next soonest. Go has no such strategy and reads it as round-robin.
    SoonestReset,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub strategy: Strategy,
    /// routing.retry.request-retry: int, default 0 (additional rounds).
    pub request_retry: usize,
    /// routing.retry.max-retry-credentials: int, default 0 (unlimited per round).
    pub max_retry_credentials: usize,
    /// routing.retry.max-retry-interval: int seconds, default 0 (no positive waits).
    pub max_retry_interval: Duration,
    /// routing.cooldown.disable-cooling: bool, default false.
    pub disable_cooling: bool,
    /// routing.cooldown.transient-error-cooldown-seconds: int, 0 = 60s, negative disables.
    pub transient_error_cooldown_seconds: i64,
    /// routing.session-affinity: bool, default false.
    pub session_affinity: bool,
    /// routing.session-affinity-ttl: duration string, default 1h.
    pub session_affinity_ttl: Duration,
    pub session_affinity_subagents: bool,
    /// routing.cooldown.save-cooldown-status: persist cooldowns as `.cds` files in
    /// `auth-dir` (cooldown_store.rs), never in credential files.
    pub save_cooldown_status: bool,
    /// routing.cooldown.max-trusted-cooldown (cliproxy-rs only): the first bound on a
    /// quota reset an upstream states ([`trusted`]); zero trusts every reset, as Go does.
    pub max_trusted_cooldown: Duration,
    /// routing.force-model-prefix: bool, default false.
    pub force_model_prefix: bool,
    /// Enabled `openai-compatibility` entries in config order with their
    /// `disable-cooling` (Go `providerCoolingOverrideForAuth`). Filled from the config on
    /// every publish ([`compat_cooling`]).
    pub compat_disable_cooling: Vec<(String, Option<bool>)>,
    /// OAuth-only `oauth.request-scoped-errors` channels; API keys must not inherit these.
    pub oauth_request_scoped_errors: HashMap<String, Vec<ErrorRule>>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            strategy: Strategy::RoundRobin,
            request_retry: 0,
            max_retry_credentials: 0,
            max_retry_interval: Duration::ZERO,
            disable_cooling: false,
            transient_error_cooldown_seconds: 0,
            session_affinity: false,
            session_affinity_ttl: Duration::from_secs(3600),
            session_affinity_subagents: true,
            save_cooldown_status: false,
            max_trusted_cooldown: DEFAULT_TRUSTED_COOLDOWN,
            force_model_prefix: false,
            compat_disable_cooling: Vec::new(),
            oauth_request_scoped_errors: HashMap::new(),
        }
    }
}

impl From<&cpa_core::config::RoutingConfig> for Policy {
    fn from(r: &cpa_core::config::RoutingConfig) -> Self {
        let ttl = cpa_core::config::parse_duration(r.session_affinity_ttl.trim())
            .filter(|d| *d > 0)
            .map(|n| Duration::from_nanos(n as u64).max(Duration::from_secs(1)))
            .unwrap_or(Duration::from_secs(3600));
        Policy {
            strategy: match r.strategy.trim().to_ascii_lowercase().as_str() {
                "weighted-round-robin" | "weightedroundrobin" | "wrr" => Strategy::WeightedRoundRobin,
                "fill-first" | "fillfirst" | "ff" => Strategy::FillFirst,
                "soonest-reset" | "soonestreset" | "reset-first" | "resetfirst" => Strategy::SoonestReset,
                _ => Strategy::RoundRobin,
            },
            force_model_prefix: r.force_model_prefix,
            session_affinity: r.session_affinity,
            session_affinity_ttl: ttl,
            session_affinity_subagents: !r.session_affinity || r.session_affinity_subagents.unwrap_or(true),
            request_retry: r.retry.request_retry.max(0) as usize,
            max_retry_credentials: r.retry.max_retry_credentials.max(0) as usize,
            max_retry_interval: Duration::from_secs(r.retry.max_retry_interval.max(0) as u64),
            disable_cooling: r.cooldown.disable_cooling,
            save_cooldown_status: r.cooldown.save_cooldown_status,
            transient_error_cooldown_seconds: r.cooldown.transient_error_cooldown_seconds,
            max_trusted_cooldown: trusted_cooldown(&r.cooldown.max_trusted_cooldown),
            ..Policy::default()
        }
    }
}

impl Policy {
    /// The policy a config publishes: `routing`, the `disable-cooling` of enabled
    /// OpenAI-compatibility entries, and `oauth.request-scoped-errors`.
    pub fn from_config(cfg: &cpa_core::config::Config) -> Self {
        Self {
            compat_disable_cooling: compat_cooling(cfg),
            oauth_request_scoped_errors: oauth_rules(cfg),
            ..Self::from(&cfg.routing)
        }
    }
}

pub(super) const DEFAULT_TRUSTED_COOLDOWN: Duration = Duration::from_secs(3600);

/// `max-trusted-cooldown`: a Go duration (`"90m"`) or a bare number of seconds. Empty
/// means the one-hour default and zero or negative turns the bound off; a positive value
/// is at least Go's 10 s quota floor. An unreadable value (`"1d"`, `false`, `"1.5"`)
/// keeps the default and is logged once per value.
pub(super) fn trusted_cooldown(raw: &str) -> Duration {
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_TRUSTED_COOLDOWN;
    }
    let nanos = match raw.parse::<i64>() {
        Ok(seconds) => seconds.saturating_mul(1_000_000_000),
        Err(_) => match cpa_core::config::parse_duration(raw) {
            Some(nanos) => nanos,
            None => {
                static WARNED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
                let mut last = WARNED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if *last != raw {
                    raw.clone_into(&mut last);
                    tracing::warn!(
                        "routing.cooldown.max-trusted-cooldown {raw:?} is not a duration (\"90m\") or a \
                         number of seconds; using 1h"
                    );
                }
                return DEFAULT_TRUSTED_COOLDOWN;
            }
        },
    };
    match nanos {
        ..=0 => Duration::ZERO,
        n => Duration::from_nanos(n as u64).max(Duration::from_secs(10)),
    }
}

/// What a matched request-scoped error rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Stop,
    StopAndCooldown,
    Continue,
    ContinueAndCooldown,
}

impl RuleAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "stop" => Some(Self::Stop),
            "stop-and-cooldown" => Some(Self::StopAndCooldown),
            "continue" => Some(Self::Continue),
            "continue-and-cooldown" => Some(Self::ContinueAndCooldown),
            _ => None,
        }
    }
}

/// A request-scoped error rule, parsed once: from `oauth.request-scoped-errors` when the
/// policy is built, from a credential's `request_scoped_errors` once per revision.
#[derive(Debug, Clone)]
pub struct ErrorRule {
    pub status: i64,
    pub r#match: Vec<String>,
    pub regex: Vec<regex::Regex>,
    pub action: RuleAction,
}

impl ErrorRule {
    /// A rule that can match, or `None` with the reason in `problems`. Empty match
    /// strings and patterns match nothing and are dropped; an invalid pattern is dropped
    /// and reported.
    fn parse(
        status: i64,
        words: Vec<String>,
        patterns: &[String],
        action: &str,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let r#match: Vec<String> = words.into_iter().filter(|w| !w.is_empty()).collect();
        let regex: Vec<regex::Regex> = patterns
            .iter()
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                regex::Regex::new(p)
                    .map_err(|e| problems.push(format!("status {status}: match-regexr {p:?} is invalid: {e}")))
                    .ok()
            })
            .collect();
        let Some(action) = RuleAction::parse(action) else {
            problems.push(format!("status {status}: unknown action {action:?}"));
            return None;
        };
        (status > 0 && !(r#match.is_empty() && regex.is_empty())).then_some(Self {
            status,
            r#match,
            regex,
            action,
        })
    }

    fn matches(&self, status: i64, body: &str) -> bool {
        self.status == status
            && (self.r#match.iter().any(|s| body.contains(s.as_str())) || self.regex.iter().any(|re| re.is_match(body)))
    }
}

/// A credential's own `request_scoped_errors`, as it is stored in its file.
#[derive(Deserialize)]
pub(super) struct FileRule {
    #[serde(default)]
    status: i64,
    #[serde(default)]
    r#match: Vec<String>,
    #[serde(default, rename = "match-regexr")]
    match_regex: Vec<String>,
    #[serde(default)]
    action: String,
}

/// A credential's own rules. `Some` whenever it lists any, even when none of them can
/// match: a listed set replaces the OAuth provider rules.
pub(super) fn file_rules(c: &Credential) -> Option<Vec<ErrorRule>> {
    let raw: Vec<FileRule> = serde_json::from_value(metadata(c, "request_scoped_errors")?.clone()).ok()?;
    if raw.is_empty() {
        return None;
    }
    let mut problems = Vec::new();
    let rules = raw
        .into_iter()
        .filter_map(|r| ErrorRule::parse(r.status, r.r#match, &r.match_regex, &r.action, &mut problems))
        .collect();
    for problem in problems {
        tracing::warn!(id = %c.id, "request_scoped_errors rule ignored: {problem}");
    }
    Some(rules)
}

/// `oauth.request-scoped-errors` by lowercase channel (Go
/// `SanitizeOAuthRequestScopedErrors`), with what was dropped and why.
pub(super) fn parse_oauth_rules(cfg: &cpa_core::config::Config) -> (HashMap<String, Vec<ErrorRule>>, Vec<String>) {
    let mut out = HashMap::new();
    let mut problems = Vec::new();
    let channels = cfg
        .document
        .get("oauth")
        .and_then(|o| o.get("request-scoped-errors"))
        .and_then(serde_yaml_ng::Value::as_mapping);
    for (channel, list) in channels.into_iter().flatten() {
        let channel = channel.as_str().unwrap_or_default().trim().to_lowercase();
        let mut issues = Vec::new();
        let rules: Vec<ErrorRule> = list
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let words = |k: &str| -> Vec<String> {
                    r.get(k)
                        .and_then(serde_yaml_ng::Value::as_sequence)
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                        .collect()
                };
                let status = r.get("status").and_then(serde_yaml_ng::Value::as_i64).unwrap_or(0);
                let action = r
                    .get("action")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase();
                ErrorRule::parse(status, words("match"), &words("match-regexr"), &action, &mut issues)
            })
            .collect();
        problems.extend(issues.into_iter().map(|p| format!("{channel}: {p}")));
        if !channel.is_empty() && !rules.is_empty() {
            out.insert(channel, rules);
        }
    }
    (out, problems)
}

pub(super) fn oauth_rules(cfg: &cpa_core::config::Config) -> HashMap<String, Vec<ErrorRule>> {
    let (rules, problems) = parse_oauth_rules(cfg);
    for problem in problems {
        tracing::warn!("oauth.request-scoped-errors rule ignored: {problem}");
    }
    rules
}

#[derive(Debug, Clone, Copy)]
pub struct ErrorAction {
    /// A configured request-scoped rule decided this outcome.
    pub matched: bool,
    pub stop: bool,
    pub cooldown: bool,
    pub force_cooldown: bool,
}

pub(super) fn metadata<'a>(c: &'a Credential, key: &str) -> Option<&'a Value> {
    // Canonical presence wins even for false, zero and null.
    c.metadata
        .get(key)
        .or_else(|| c.metadata.get(key.replace('_', "-").as_str()))
}

pub(super) fn integer(c: &Credential, key: &str) -> Option<i64> {
    c.attributes
        .get(key)
        .filter(|_| key == "priority")
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            [key.to_owned(), key.replace('_', "-")].iter().find_map(|key| {
                let v = c.metadata.get(key)?;
                v.as_i64()
                    .or_else(|| v.as_str()?.trim().parse().ok())
                    .or_else(|| v.as_f64().map(|n| n as i64))
            })
        })
}

pub(super) fn boolean(c: &Credential, key: &str) -> Option<bool> {
    let parse = |s: &str| match s.trim() {
        "true" | "True" | "TRUE" | "1" | "t" | "T" => Some(true),
        "false" | "False" | "FALSE" | "0" | "f" | "F" => Some(false),
        _ => None,
    };
    [key.to_owned(), key.replace('_', "-")].iter().find_map(|key| {
        let v = c.metadata.get(key)?;
        v.as_bool()
            .or_else(|| parse(v.as_str()?))
            .or_else(|| v.as_f64().map(|n| n != 0.0))
    })
}

pub(super) fn oauth(c: &Credential) -> bool {
    c.str("api_key").is_none() && !c.attributes.contains_key("api_key")
}

impl Policy {
    pub fn retry_limit(&self, c: &Credential) -> usize {
        integer(c, "request_retry")
            .filter(|n| *n >= 0)
            .map(|n| n as usize)
            .unwrap_or(self.request_retry)
    }

    /// [`Self::retry_limit`] of a parsed credential.
    pub(crate) fn retry_limit_of(&self, profile: &Profile) -> usize {
        profile.request_retry.unwrap_or(self.request_retry)
    }

    /// Whether the strategy can pick a credential of this weight.
    pub(crate) fn admits_weight(&self, weight: i64) -> bool {
        self.strategy != Strategy::WeightedRoundRobin || weight > 0
    }

    /// Go `quotaCooldownDisabledForAuthWithConfig`: the credential's `disable_cooling`
    /// metadata, then its OpenAI-compatibility entry's `disable-cooling`, then global.
    pub(crate) fn cooling_disabled(&self, c: &Credential) -> bool {
        boolean(c, "disable_cooling")
            .or_else(|| self.provider_cooling(c))
            .unwrap_or(self.disable_cooling)
    }

    /// Go `providerCoolingOverrideForAuth` with `resolveOpenAICompatConfig`: the first
    /// enabled entry (config order) named by the credential's `compat_name`,
    /// `provider_key` or provider.
    fn provider_cooling(&self, c: &Credential) -> Option<bool> {
        let provider = c.provider.trim().to_lowercase();
        let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).unwrap_or_default();
        let (compat_name, mut provider_key) = (attr("compat_name"), attr("provider_key"));
        if provider.is_empty()
            || (provider_key.is_empty() && compat_name.is_empty() && provider != "openai-compatibility")
        {
            return None;
        }
        if provider_key.is_empty() {
            provider_key = &provider;
        }
        let candidates = [compat_name, provider_key, provider.as_str()];
        // ponytail: ASCII case folding; Go's EqualFold also folds non-ASCII names. Use Unicode
        // folding if names stop being ASCII.
        self.compat_disable_cooling
            .iter()
            .find(|(name, _)| candidates.iter().any(|c| !c.is_empty() && c.eq_ignore_ascii_case(name)))
            .and_then(|(_, disable)| *disable)
    }

    pub fn error_action(&self, c: &Credential, error: &ExecError) -> ErrorAction {
        self.error_action_with(file_rules(c).as_deref(), c, error)
    }

    /// [`Self::error_action`] with the credential's own rules already parsed.
    pub(crate) fn error_action_with(
        &self,
        own: Option<&[ErrorRule]>,
        c: &Credential,
        error: &ExecError,
    ) -> ErrorAction {
        let rules = own.or_else(|| {
            oauth(c)
                .then(|| self.oauth_request_scoped_errors.get(&c.provider))
                .flatten()
                .map(Vec::as_slice)
        });
        if let Some(rules) = rules {
            let body = crate::classify::rule_text(error);
            let status = i64::from(error.go_status());
            if let Some(rule) = rules.iter().find(|r| r.matches(status, &body)) {
                let (stop, cooldown) = match rule.action {
                    RuleAction::Stop => (true, false),
                    RuleAction::StopAndCooldown => (true, true),
                    RuleAction::Continue => (false, false),
                    RuleAction::ContinueAndCooldown => (false, true),
                };
                return ErrorAction {
                    matched: true,
                    stop,
                    cooldown,
                    force_cooldown: cooldown,
                };
            }
        }
        // Go `shouldSkipCredentialCooldown`: request faults and transport/lifecycle
        // faults never cool a credential.
        let request = crate::classify::is_request_invalid(error);
        ErrorAction {
            matched: false,
            stop: request,
            cooldown: !request && !crate::classify::is_transport(error),
            force_cooldown: false,
        }
    }
}

/// Go `isCredentialRetryRoundStatus`.
pub fn retry_status(status: u16) -> bool {
    matches!(status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

pub(super) fn weight(c: &Credential) -> i64 {
    let value = if let Some(s) = c.attributes.get("weight").filter(|s| !s.trim().is_empty()) {
        s.trim().parse::<i64>().ok()
    } else if let Some(v) = metadata(c, "weight") {
        v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()).or_else(|| {
            let n = v.as_f64()?;
            (n.fract() == 0.0 && n <= 1_000_000.0).then_some(n as i64)
        })
    } else {
        Some(1)
    };
    value.filter(|n| *n <= 1_000_000).unwrap_or(0).max(0)
}

/// What selection and outcome recording read from one credential revision, parsed when
/// the revision is stored rather than on every pick.
#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub revision: u64,
    /// The provider key it serves (`registry::provider_key`).
    pub provider: String,
    pub priority: i64,
    pub weight: i64,
    /// Its own `request_retry`, when set and not negative.
    pub request_retry: Option<usize>,
    /// Its own request-scoped error rules ([`file_rules`]).
    pub rules: Option<Vec<ErrorRule>>,
}

impl Profile {
    pub fn of(c: &Credential) -> Self {
        Self {
            revision: c.revision,
            provider: crate::registry::provider_key(c),
            priority: integer(c, "priority").unwrap_or(0),
            weight: weight(c),
            request_retry: integer(c, "request_retry").filter(|n| *n >= 0).map(|n| n as usize),
            rules: file_rules(c),
        }
    }
}

/// Go `cfg.OpenAICompatibility` for cooling: enabled entries with a base URL (Go
/// `SanitizeOpenAICompatibility`), in config order, with their `disable-cooling`.
pub(super) fn compat_cooling(cfg: &cpa_core::config::Config) -> Vec<(String, Option<bool>)> {
    cfg.document
        .get("api-keys")
        .and_then(|k| k.get("openai-compatibility"))
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|g| {
            g.get("base-url")
                .and_then(serde_yaml_ng::Value::as_str)
                .is_some_and(|b| !b.trim().is_empty())
                && g.get("disabled").and_then(serde_yaml_ng::Value::as_bool) != Some(true)
        })
        .map(|g| {
            (
                g.get("name")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                g.get("disable-cooling").and_then(serde_yaml_ng::Value::as_bool),
            )
        })
        .collect()
}
