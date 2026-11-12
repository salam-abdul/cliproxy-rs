use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Go `nextRefreshCheckAt` skips `AuthKind() == apikey` for every provider. The
/// kinds are Go's `AuthKind` goldens (tests/fixtures/server_go.json `auth_kind`).
#[test]
fn refresh_skips_api_key_kinds_like_go() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
    let mut kinds = std::collections::BTreeSet::new();
    for case in fixture["auth_kind"].as_array().unwrap() {
        let meta = serde_json::json!({"type": "kimi"}).as_object().unwrap().clone();
        let mut c = Credential::from_file(Path::new("/a"), Path::new("/a/x.json"), meta).unwrap();
        c.metadata = case["metadata"].as_object().cloned().unwrap_or_default();
        c.attributes = case["attributes"]
            .as_object()
            .map(|a| {
                a.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let kind = case["kind"].as_str().unwrap();
        kinds.insert(kind);
        assert_eq!(refresh_candidate(&c), kind != "apikey", "case {case}");
        c.disabled = true;
        assert!(!refresh_candidate(&c));
    }
    assert!(kinds.contains("apikey") && kinds.contains("oauth"), "{kinds:?}");
}

#[test]
fn signature_cache_config_reads_v8_and_legacy_keys() {
    let at = |yaml: &str| signature_cache_config(&Config::parse(yaml).unwrap());
    assert_eq!(at("{}\n"), (true, false));
    assert_eq!(
        at("antigravity-signature-cache-enabled: false\nantigravity-signature-bypass-strict: true\n"),
        (false, true)
    );
    assert_eq!(
        at("oauth: {providers: {antigravity: {signature-cache-enabled: false, signature-bypass-strict: true}}}\n"),
        (false, true)
    );
    assert_eq!(
        at("oauth: {providers: {antigravity: {signature-cache-enabled: null}}}\n"),
        (true, false)
    );
}

#[test]
fn exhausted_quota_overrides_affinity_and_never_falls_back_to_paid_usage() {
    for strategy in [
        crate::scheduler::Strategy::SoonestReset,
        crate::scheduler::Strategy::RoundRobin,
    ] {
        let store = CredentialStore::new(vec![cred("a", "claude", false), cred("b", "claude", false)]);
        let policy = Arc::new(Policy {
            strategy,
            session_affinity: true,
            ..Default::default()
        });
        let mut selection = Selection::new("claude", "model");
        selection.session = Some("fixture-session".into());
        let admit = |_: &Credential| Some("model".into());
        let first = store
            .select_ranked(selection.clone(), policy.clone(), &admit, &|_| Default::default())
            .unwrap();
        assert_eq!(first.credential.id, "a");
        first.complete(Outcome::Success);
        let exhausted = |c: &Credential| crate::scheduler::Windows {
            exhausted: c.id == "a",
            ..Default::default()
        };
        let next = store
            .select_ranked(selection.clone(), policy.clone(), &admit, &exhausted)
            .unwrap();
        assert_eq!(next.credential.id, "b", "an exhausted pinned account must yield");
        next.complete(Outcome::Success);
        let all = |_: &Credential| crate::scheduler::Windows {
            exhausted: true,
            ..Default::default()
        };
        assert!(store.select_ranked(selection, policy, &admit, &all).is_err());
    }
}

fn cred(id: &str, provider: &str, disabled: bool) -> Credential {
    let mut metadata = Map::new();
    metadata.insert("type".into(), provider.into());
    metadata.insert("disabled".into(), disabled.into());
    Credential::from_file(Path::new("/a"), &Path::new("/a").join(id), metadata).unwrap()
}

fn sel(provider: &str) -> Selection {
    Selection {
        provider: provider.into(),
        ..Selection::default()
    }
}

#[test]
fn select_round_robins_enabled_credentials_and_honours_exclusions() {
    let store = CredentialStore::new(vec![
        cred("c1.json", "claude", false),
        cred("off.json", "claude", true),
        cred("x.json", "codex", false),
        cred("c2.json", "claude", false),
    ]);
    let picks: Vec<String> = (0..4)
        .map(|_| store.select(sel("claude")).unwrap().credential.id.clone())
        .collect();
    assert_eq!(picks, ["c1.json", "c2.json", "c1.json", "c2.json"]);
    assert!(store.select(sel("gemini")).is_none());
    let only = Selection {
        exclude: vec!["c1.json".into()],
        ..sel("claude")
    };
    for _ in 0..3 {
        assert_eq!(store.select(only.clone()).unwrap().credential.id, "c2.json");
    }
}

#[test]
fn a_route_credential_rule_narrows_selection() {
    let store = CredentialStore::new(vec![
        cred("c1.json", "claude", false),
        cred("keep.json", "claude", false),
    ]);
    let only_kept = Selection {
        accept: Some(|c| c.id == "keep.json"),
        ..sel("claude")
    };
    for _ in 0..3 {
        assert_eq!(store.select(only_kept.clone()).unwrap().credential.id, "keep.json");
    }
    let none = Selection {
        accept: Some(|_| false),
        ..sel("claude")
    };
    assert!(store.select(none).is_none());
}

#[test]
fn standalone_admission_reads_attributes_first() {
    let mut c = cred("a.json", "claude", false);
    c.metadata.insert("prefix".into(), "meta".into());
    c.metadata
        .insert("excluded_models".into(), serde_json::json!(["claude-sonnet*"]));
    c.attributes.insert("prefix".into(), "team".into());
    c.attributes.insert("excluded_models".into(), "claude-opus*".into());
    let policy = Policy::default();
    let admit = |model: &str| {
        let s = Selection::new("claude", model);
        standalone_admission(&s, &policy)(&c)
    };
    assert_eq!(
        admit("team/claude-sonnet-5(high)").as_deref(),
        Some("claude-sonnet-5(high)")
    );
    assert_eq!(admit("team/claude-opus-5"), None, "attribute exclusions win");
    assert_eq!(
        admit("meta/claude-sonnet-5"),
        None,
        "metadata prefix is not authoritative"
    );
    assert_eq!(admit("claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
    let forced = Policy {
        force_model_prefix: true,
        ..Policy::default()
    };
    let s = Selection::new("claude", "claude-sonnet-5");
    assert_eq!(standalone_admission(&s, &forced)(&c), None);
    assert_eq!(standalone_admission(&Selection::new("codex", "x"), &policy)(&c), None);
}

#[test]
fn patch_persists_atomically_preserves_unknown_fields_and_rejects_stale() {
    let dir = std::env::temp_dir().join(format!("cpa-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude-a.json");
    std::fs::write(
        &path,
        r#"{"type":"claude","access_token":"old","zz_unknown":{"k":[1,2]}}"#,
    )
    .unwrap();
    // A stale temp-file-looking path and a symlink must not be reused or followed.
    let victim = dir.join("victim");
    std::fs::write(&victim, "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        &victim,
        dir.join(format!(".claude-a.json.{}.0.tmp", std::process::id())),
    )
    .unwrap();
    let mut cfg = cpa_core::config::Config::parse("").unwrap();
    cfg.auth_dir = dir.clone();
    let store = CredentialStore::new(cpa_core::config::credentials::from_auth_dir(&cfg));
    let rev = store.get("claude-a.json").unwrap().revision;

    let mut patch = MetadataPatch::default();
    patch.set.insert("access_token".into(), "new".into());
    patch.set.insert("email".into(), "a@x.test".into());
    patch.set.insert("disabled".into(), true.into());
    let updated = store.apply_patch("claude-a.json", rev, &patch).unwrap();
    assert!(updated.revision > rev);
    assert!(updated.disabled);
    assert_eq!(updated.label, "a@x.test", "derived label follows the new email");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"type\":\"claude\",\"access_token\":\"new\",\"zz_unknown\":{\"k\":[1,2]},\"email\":\"a@x.test\",\"disabled\":true}\n"
    );
    #[cfg(unix)]
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "keep",
        "symlink target untouched"
    );
    assert!(
        store.select(sel("claude")).is_none(),
        "patched-disabled credential leaves rotation"
    );

    assert_eq!(
        store.apply_patch("claude-a.json", rev, &patch).unwrap_err(),
        PatchError::Stale {
            current: updated.revision
        }
    );
    assert_eq!(
        store.apply_patch("nope.json", 0, &patch).unwrap_err(),
        PatchError::NotFound
    );
    let mut retype = MetadataPatch::default();
    retype.set.insert("type".into(), "codex".into());
    assert_eq!(
        store
            .apply_patch("claude-a.json", updated.revision, &retype)
            .unwrap_err(),
        PatchError::TypeIsImmutable
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".tmp") && !n.ends_with(".0.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp files left behind: {leftovers:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reconcile_keeps_unchanged_revisions_and_never_reuses_old_ones() {
    let store = CredentialStore::new(vec![cred("a.json", "claude", false), cred("b.json", "claude", false)]);
    let a = store.get("a.json").unwrap();
    let b = store.get("b.json").unwrap();
    store.reconcile(vec![cred("a.json", "claude", false)]);
    assert!(
        Arc::ptr_eq(&store.get("a.json").unwrap(), &a),
        "unchanged credential kept as is"
    );
    assert!(store.get("b.json").is_none());
    store.reconcile(vec![cred("a.json", "claude", false), cred("b.json", "claude", false)]);
    assert!(
        store.get("b.json").unwrap().revision > b.revision,
        "re-created credential gets a new revision"
    );
}

#[test]
fn attribute_only_reload_changes_priority_and_rejects_stale_outcome() {
    let a = cred("a.json", "claude", false);
    let mut b = cred("b.json", "claude", false);
    b.attributes.insert("priority".into(), "1".into());
    let store = CredentialStore::new(vec![a.clone(), b.clone()]);
    let old = store.select(sel("claude")).unwrap();
    assert_eq!(old.credential.id, "b.json");
    let revision = old.credential.revision;
    b.attributes.insert("priority".into(), "2".into());
    store.reconcile(vec![a, b]);
    assert!(store.get("b.json").unwrap().revision > revision);
    old.complete(Outcome::Failure(ExecError::local(
        401,
        FailureScope::Credential,
        "expired",
    )));
    let current = store.select(sel("claude")).unwrap();
    assert_eq!(
        current.credential.id, "b.json",
        "stale outcome must not cool new revision"
    );
    assert_eq!(current.credential.attributes["priority"], "2");
    current.complete(Outcome::Success);
    assert_eq!(store.stats().failure, 1);
    assert_eq!(store.stats().success, 1);
}

#[test]
fn stale_lease_cannot_poison_deleted_and_recreated_credential() {
    let credential = cred("a.json", "claude", false);
    let store = CredentialStore::new(vec![credential.clone()]);
    let old = store.select(sel("claude")).unwrap();
    store.reconcile(Vec::new());
    store.reconcile(vec![credential]);
    old.complete(Outcome::Failure(ExecError::local(
        429,
        FailureScope::Credential,
        "quota",
    )));
    store.select(sel("claude")).unwrap().complete(Outcome::Success);
    assert_eq!(store.stats().failure, 1);
    assert_eq!(store.stats().success, 1);
}

/// Go `TestRetryIntervalFiltersCooldownCredentials` and
/// `TestRetryRoundAvailabilityRejectsStaleQuotaForNonRetryableStatus`: the next round
/// waits for the earliest cooldown within max-retry-interval, skipping longer ones,
/// and a credential cooling after a 402 or 404 never permits a round, whatever its
/// deadline, while one cooling after a 429 does.
#[test]
fn retry_wait_skips_cooldowns_beyond_the_cap_and_non_retryable_statuses() {
    let now = Instant::now();
    let selection = sel("claude");
    let admit_all = |_: &Credential| Some(String::new());
    // One credential per (status, retry-after seconds), each cooling from `now`.
    let wait = |cap: u64, cooldowns: &[(u16, u64)]| {
        let policy = Policy {
            request_retry: 1,
            max_retry_interval: Duration::from_secs(cap),
            ..Policy::default()
        };
        let ids: Vec<String> = (0..cooldowns.len()).map(|i| format!("c{i}.json")).collect();
        let store = CredentialStore::new(ids.iter().map(|id| cred(id, "claude", false)).collect());
        for (id, (status, seconds)) in ids.iter().zip(cooldowns) {
            let mut error = ExecError::local(*status, FailureScope::Model, "cooling");
            error.retry_after = Some(Duration::from_secs(*seconds));
            let credential = store.get(id).unwrap();
            store.scheduler.lock().unwrap().record(
                &credential,
                &selection.model,
                &Outcome::Failure(error),
                &policy,
                now,
            );
        }
        store.retry_wait_at(&selection, &policy, 429, &[], &admit_all, now)
    };
    let short_and_long = wait(30, &[(429, 10), (429, 60)]);
    assert!(
        short_and_long.is_some_and(|w| !w.is_zero() && w <= Duration::from_secs(10)),
        "{short_and_long:?}"
    );
    assert_eq!(wait(30, &[(429, 60)]), None, "only a cooldown beyond the cap");
    assert!(wait(3600, &[(429, 60)]).is_some(), "rate limit");
    assert_eq!(wait(3600, &[(402, 60)]), None, "payment required");
    assert_eq!(wait(3600, &[(404, 60)]), None, "not found");
}

#[test]
fn retry_wait_obeys_exact_cap_and_attempted_quota_floor() {
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let now = Instant::now();
    let mut selection = sel("claude");
    let admit_all = |_: &Credential| Some(String::new());
    let tried = ["a.json".to_owned()];
    let mut policy = Policy {
        request_retry: 1,
        ..Policy::default()
    };
    // A transport fault (status 0) retries immediately.
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 0, &tried, &admit_all, now),
        Some(Duration::ZERO)
    );
    // An attempted credential after a 429 waits at least 10s, which the default
    // max-retry-interval of 0 forbids; an untried one may go immediately.
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, now),
        None
    );
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &[], &admit_all, now),
        Some(Duration::ZERO)
    );
    policy.max_retry_interval = Duration::from_secs(10);
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, now),
        Some(Duration::from_secs(10))
    );
    let quota = ExecError::local(429, FailureScope::Model, "quota");
    let credential = store.get("a.json").unwrap();
    store.scheduler.lock().unwrap().record(
        &credential,
        &selection.model,
        &Outcome::Failure(quota.clone()),
        &policy,
        now,
    );
    let later = now + Duration::from_millis(1);
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
        Some(Duration::from_secs(10))
    );
    policy.max_retry_interval = Duration::from_secs(9);
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
        None
    );
    policy.disable_cooling = true;
    store.scheduler.lock().unwrap().record(
        &credential,
        &selection.model,
        &Outcome::Failure(quota.clone()),
        &policy,
        later,
    );
    policy.max_retry_interval = Duration::ZERO;
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
        Some(Duration::ZERO),
        "disabled cooling has no floor"
    );
    selection.retry_round = 1;
    assert_eq!(
        store.retry_wait_at(&selection, &policy, 0, &tried, &admit_all, later),
        None
    );
}

/// A 429 with a reset six days away (`max-trusted-cooldown` bounds it).
fn weekly_limit(scope: FailureScope) -> Outcome {
    let mut error = ExecError::local(429, scope, "usage_limit_reached");
    error.retry_after = Some(Duration::from_secs(6 * 24 * 3600));
    Outcome::Failure(error)
}

/// A probe whose hold lapsed was taken over by a later pick: the first probe's
/// cancellation leaves the account reserved for the second, whose own does not.
#[test]
fn a_stale_probe_cancellation_keeps_the_newer_reservation() {
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let selection = Selection::new("claude", "m");
    let credential = store.get("a.json").unwrap();
    // A bounded window of the 10 s minimum that ended a second ago.
    let policy = Policy {
        max_trusted_cooldown: Duration::from_secs(10),
        ..Policy::default()
    };
    let opened = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
    store
        .scheduler
        .lock()
        .unwrap()
        .record(&credential, "m", &weekly_limit(FailureScope::Model), &policy, opened);
    let first = store.select(selection.clone()).expect("the probe");
    assert_eq!(first.execution_model, "m");
    assert!(store.select(selection.clone()).is_none(), "reserved");
    // The first probe's hold lapses, and the next pick probes again.
    store
        .scheduler
        .lock()
        .unwrap()
        .cooldown_mut("a.json", "m")
        .unwrap()
        .trust
        .probe = Some(Instant::now());
    let second = store.select(selection.clone()).expect("the second probe");
    first.complete(Outcome::Cancelled);
    assert!(store.select(selection.clone()).is_none(), "still reserved");
    second.complete(Outcome::Cancelled);
    assert!(store.select(selection).is_some(), "its own cancellation frees it");
}

/// A request picked before a bounded window opened succeeds after it (a stream that
/// outlived the 429): its answer is older than the window, which stays, live or
/// ended, until its own probe.
#[test]
fn a_late_success_keeps_a_newer_window() {
    let selection = Selection::new("claude", "m");
    // A model window, opened by a request picked after the stream.
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let stream = store.select(selection.clone()).unwrap();
    let other = store.select(selection.clone()).unwrap();
    other.complete(weekly_limit(FailureScope::Model));
    assert!(store.select(selection.clone()).is_none(), "cooling");
    stream.accepted();
    stream.complete(Outcome::Success);
    assert!(store.select(selection.clone()).is_none(), "the window stands");
    // A credential-wide window that already ended keeps its probe.
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let stream = store.select(selection.clone()).unwrap();
    let policy = Policy {
        max_trusted_cooldown: Duration::from_secs(10),
        ..Policy::default()
    };
    let opened = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
    store.scheduler.lock().unwrap().record(
        &store.get("a.json").unwrap(),
        "m",
        &weekly_limit(FailureScope::Credential),
        &policy,
        opened,
    );
    stream.accepted();
    stream.complete(Outcome::Success);
    let _probe = store.select(selection.clone()).expect("the window's probe");
    assert!(store.select(selection).is_none(), "one probe");
}

#[test]
fn config_routing_drives_policy_at_startup_and_on_publish() {
    let executors = || Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    // Legacy top-level keys and the canonical routing block both reach the scheduler.
    let rt = crate::testing::runtime(
        crate::testing::config("request-retry: 2\nrouting:\n  strategy: ff\n  session-affinity-ttl: 250ms\n"),
        Vec::new(),
        executors(),
    );
    let policy = rt.policy();
    assert_eq!(policy.strategy, crate::scheduler::Strategy::FillFirst);
    assert_eq!(policy.request_retry, 2);
    assert_eq!(policy.session_affinity_ttl, Duration::from_secs(1));

    rt.publish_config(Config::parse("routing:\n  strategy: wrr\n").unwrap());
    let policy = rt.policy();
    assert_eq!(policy.strategy, crate::scheduler::Strategy::WeightedRoundRobin);
    assert_eq!(policy.request_retry, 0);
    assert_eq!(policy.session_affinity_ttl, Duration::from_secs(3600));
}

#[test]
fn a_request_snapshot_pairs_each_config_with_its_own_policy() {
    let rt = Arc::new(crate::testing::runtime(
        crate::testing::config(""),
        Vec::new(),
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let configs = ["ff", "wrr"].map(|s| crate::testing::config(&format!("routing:\n  strategy: {s}\n")));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = std::thread::spawn({
        let (rt, done) = (rt.clone(), done.clone());
        move || {
            let mut reads = 0u64;
            while !done.load(Ordering::Relaxed) {
                let (config, policy) = rt.request_snapshot();
                assert_eq!(
                    Policy::from_config(&config).strategy,
                    policy.strategy,
                    "after {reads} reads"
                );
                reads += 1;
            }
        }
    });
    for i in 0..50_000 {
        rt.publish_config(configs[i % 2].clone());
    }
    done.store(true, Ordering::Relaxed);
    reader.join().unwrap();
}

/// Go `nextModelPoolOffset` (conductor_models.go): per-key cursor, no advance for
/// single-entry pools or blank keys, reset to zero at the int32 guard.
#[test]
fn pool_offsets_rotate_per_key_like_go() {
    let rt = crate::testing::runtime(
        crate::testing::config(""),
        Vec::new(),
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    );
    let offsets: Vec<usize> = (0..4).map(|_| rt.next_pool_offset("a|openai|m", 3)).collect();
    assert_eq!(offsets, [0, 1, 2, 0]);
    assert_eq!(rt.next_pool_offset(" a|openai|m ", 3), 1, "keys are trimmed");
    assert_eq!(rt.next_pool_offset("b|openai|m", 3), 0, "keys rotate independently");
    assert_eq!(rt.next_pool_offset("c", 1), 0);
    assert_eq!(rt.next_pool_offset("c", 2), 0, "a single-entry pool does not advance");
    assert_eq!(rt.next_pool_offset("  ", 2), 0);
    rt.pool_offsets.lock().unwrap().insert("d".into(), 2_147_483_641);
    assert_eq!(rt.next_pool_offset("d", 3), 0, "the guard resets before use");
    assert_eq!(rt.next_pool_offset("d", 3), 1);
}

/// Go `Manager.persist` after `MarkResult`: the auth file gains `"disabled": false`
/// in `json.Marshal` form (sorted keys, float64 numbers, HTML escaping, no newline)
/// once, and is not rewritten while it already holds that JSON.
#[test]
fn first_use_persists_disabled_in_go_marshal_form() {
    let dir = std::env::temp_dir().join(format!(
        "persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.json");
    let original = "{\"type\":\"claude\",\"email\":\"a<b>@x.invalid\",\"n\":1.50,\"big\":1e3,\"nested\":{\"z\":1,\"a\":[true,null]}}\n";
    std::fs::write(&path, original).unwrap();
    let metadata: Map<String, Value> = serde_json::from_str(original).unwrap();
    let store = CredentialStore::new(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
    let revision = store.get("a.json").unwrap().revision;
    store
        .select(Selection::new("claude", "m"))
        .unwrap()
        .complete(Outcome::Success);
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        written,
        r#"{"big":1000,"disabled":false,"email":"a\u003cb\u003e@x.invalid","n":1.5,"nested":{"a":[true,null],"z":1},"type":"claude"}"#
    );
    let current = store.get("a.json").unwrap();
    assert_eq!(current.revision, revision, "a neutral write keeps the revision");
    assert_eq!(current.metadata.get("disabled"), Some(&Value::Bool(false)));
    assert_eq!(current.metadata.get("big"), Some(&Value::from(1000)));
    // The watcher's reload of the written file changes nothing.
    let reloaded: Map<String, Value> = serde_json::from_str(&written).unwrap();
    store.reconcile(vec![Credential::from_file(&dir, &path, reloaded).unwrap()]);
    assert_eq!(store.get("a.json").unwrap().revision, revision);
    // Same JSON in another layout is left alone.
    std::fs::write(&path, "{\"type\":\"claude\", \"disabled\":false}").unwrap();
    let mut metadata = Map::new();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("disabled".into(), false.into());
    store.reconcile(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
    store
        .select(Selection::new("claude", "m"))
        .unwrap()
        .complete(Outcome::Success);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"type\":\"claude\", \"disabled\":false}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A result recorded after a refresh committed a newer revision must not write the
/// older token back (Go persists under the manager lock with generation checks).
#[test]
fn stale_result_does_not_overwrite_a_refreshed_credential() {
    let dir = std::env::temp_dir().join(format!(
        "stale-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.json");
    std::fs::write(&path, r#"{"type":"claude","access_token":"old"}"#).unwrap();
    let metadata: Map<String, Value> = serde_json::from_str(r#"{"type":"claude","access_token":"old"}"#).unwrap();
    let store = CredentialStore::new(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
    let stale = store.get("a.json").unwrap();
    let mut patch = MetadataPatch::default();
    patch.set.insert("access_token".into(), "new".into());
    store.apply_patch("a.json", stale.revision, &patch).unwrap();
    let refreshed = std::fs::read_to_string(&path).unwrap();
    store.persist_credential(&stale);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), refreshed);
    assert!(refreshed.contains("\"new\"") && refreshed.contains("\"disabled\":false"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Go `save-cooldown-status`: a cooldown change writes `<auth-dir>/<file>.cds`, a new
/// process restores it, reset clears it, and a file Go wrote restores too.
#[test]
fn cooldowns_persist_restore_and_reset_through_cds_files() {
    let dir = std::env::temp_dir().join(format!(
        "cds-rt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config = || {
        Config::parse(&format!(
            "auth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n",
            dir.display()
        ))
        .unwrap()
    };
    let creds = || {
        let metadata = |p: &str| {
            let mut m = Map::new();
            m.insert("type".into(), p.into());
            m
        };
        vec![
            Credential::from_file(&dir, &dir.join("a.json"), metadata("claude")).unwrap(),
            Credential::from_file(&dir, &dir.join("b.json"), metadata("claude")).unwrap(),
        ]
    };
    let executors = || Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        openai: Default::default(),
        google: Default::default(),
        devices: Default::default(),
    };
    let rt = crate::testing::runtime(config(), creds(), executors());
    let mut selection = Selection::new("claude", "m1");
    selection.exclude.push("b.json".into());
    let lease = rt.store.select(selection.clone()).unwrap();
    let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
    quota.retry_after = Some(Duration::from_secs(120));
    lease.complete(Outcome::Failure(quota));
    let file = dir.join("a.cds");
    let written = std::fs::read_to_string(&file).expect("cooldown written");
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600,
        "Go os.CreateTemp mode"
    );
    assert!(
        written.contains("\"model\": \"m1\"") && written.contains("\"reason\": \"quota\""),
        "{written}"
    );
    assert!(!dir.join("b.cds").exists());
    // A successful attempt on another credential changes no cooldown: no rewrite.
    let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
    let mut other = Selection::new("claude", "m1");
    other.exclude.push("a.json".into());
    rt.store.select(other).unwrap().complete(Outcome::Success);
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified);

    // A new process restores the cooldown.
    drop(rt);
    let rt = crate::testing::runtime(config(), creds(), executors());
    let a = rt.store.get("a.json").unwrap();
    let wait = rt
        .store
        .scheduler
        .lock()
        .unwrap()
        .wait(&a, "m1", Instant::now())
        .unwrap();
    assert!(
        wait > Duration::from_secs(100) && wait <= Duration::from_secs(120),
        "{wait:?}"
    );
    assert_eq!(rt.store.reset_cooldown("a.json"), ["m1"]);
    assert!(!file.exists(), "reset removes the stale file");

    // A file CLIProxyAPI wrote (local time with an offset) restores model and
    // credential-wide quota records; an expired record is dropped on rewrite.
    let at = |secs: i64| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs))
            .with_timezone(&chrono::FixedOffset::east_opt(7200).unwrap())
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
    };
    let go = format!(
        r#"{{"version":1,"auth_id":"b.json","provider":"claude","updated_at":"{now}","records":[
{{"provider":"claude","auth_id":"b.json","status":"cooling","next_retry_after":"{late}","reason":"credential_quota","quota":{{"exceeded":true,"reason":"credential_quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"last_error":{{"message":"credential quota","retryable":false,"http_status":429}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m2","status":"cooling","next_retry_after":"{soon}","reason":"unauthorized","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"last_error":{{"message":"unauthorized","retryable":false,"http_status":401}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m4","status":"cooling","next_retry_after":"{soon}","reason":"quota","quota":{{"exceeded":true,"reason":"quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m5","status":"cooling","next_retry_after":"{past}","reason":"quota","quota":{{"exceeded":true,"reason":"quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m3","status":"cooling","next_retry_after":"{past}","reason":"boom","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"gone.json","model":"m1","status":"cooling","next_retry_after":"{late}","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}}]}}"#,
        now = at(0),
        late = at(600),
        soon = at(60),
        past = at(-5),
    );
    std::fs::write(dir.join("b.cds"), go).unwrap();
    drop(rt);
    let rt = crate::testing::runtime(config(), creds(), executors());
    let b = rt.store.get("b.json").unwrap();
    let now = Instant::now();
    let scheduler = rt.store.scheduler.lock().unwrap();
    let wait = |m: &str| scheduler.wait(&b, m, now).unwrap_or_default().as_secs();
    assert!((590..=600).contains(&wait("m9")), "credential quota blocks every model");
    assert!(scheduler.quota_cooling(&b, "m9", now));
    let records = scheduler.records(&b, now, std::time::SystemTime::now());
    drop(scheduler);
    let models: Vec<&str> = records.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(
        models,
        ["", "m2", "m4"],
        "expired (by retry deadline) and unknown-credential records are dropped"
    );
    let m4 = records.iter().find(|r| r.model == "m4").unwrap();
    let left = m4
        .next_retry_after
        .unwrap()
        .duration_since(std::time::SystemTime::now())
        .unwrap();
    assert!(
        left > Duration::from_secs(590),
        "the later quota recovery wins: {left:?}"
    );
    let rewritten = std::fs::read_to_string(dir.join("b.cds")).unwrap();
    assert!(!rewritten.contains("gone.json") && !rewritten.contains("\"m3\""));

    // Turning the option off persists the final state and stops writing.
    rt.publish_config(Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap());
    rt.store.reset_cooldown("b.json");
    assert!(dir.join("b.cds").exists(), "no writes once disabled");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A private, empty directory for one cooldown test.
fn cooldown_test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A cooldown change on a request thread hands the snapshot over and returns; the
/// save runs elsewhere, so a slow disk or store never holds a request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cooldown_saves_never_block_the_caller() {
    struct Gated {
        armed: std::sync::atomic::AtomicBool,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        saves: std::sync::Mutex<Vec<bool>>,
    }
    impl crate::cooldown_store::Backend for Gated {
        fn load(&self) -> Result<Vec<crate::cooldown_store::Record>, String> {
            Ok(Vec::new())
        }
        fn save(&self, _: Vec<crate::cooldown_store::Record>, _: std::time::SystemTime) -> Result<(), String> {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                let released = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .is_ok();
                self.saves.lock().unwrap().push(released);
            }
            Ok(())
        }
    }
    let dir = cooldown_test_dir("cds-async");
    let (release, waiting) = std::sync::mpsc::channel();
    let backend = Arc::new(Gated {
        armed: Default::default(),
        release: std::sync::Mutex::new(waiting),
        saves: Default::default(),
    });
    let store = CredentialStore::new(Vec::new());
    store.set_cooldown_backend(backend.clone());
    store.configure_cooldown_store(Some(dir.clone()));
    store.flush_cooldowns();
    backend.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    store.persist_cooldowns();
    release.send(()).unwrap();
    store.flush_cooldowns();
    assert_eq!(*backend.saves.lock().unwrap(), [true], "the save waited for release");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cooldown change on one credential rewrites only that credential's `.cds` file.
#[test]
fn a_cooldown_change_leaves_other_credentials_files_alone() {
    let dir = cooldown_test_dir("cds-one");
    let config = Config::parse(&format!(
        "auth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n",
        dir.display()
    ))
    .unwrap();
    let metadata = || {
        let mut m = Map::new();
        m.insert("type".into(), "claude".into());
        m
    };
    let creds = vec![
        Credential::from_file(&dir, &dir.join("a.json"), metadata()).unwrap(),
        Credential::from_file(&dir, &dir.join("b.json"), metadata()).unwrap(),
    ];
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        openai: Default::default(),
        google: Default::default(),
        devices: Default::default(),
    };
    let rt = crate::testing::runtime(config, creds, executors);
    let fail = |id: &str, model: &str| {
        let mut selection = Selection::new("claude", model);
        selection
            .exclude
            .push(if id == "a.json" { "b.json" } else { "a.json" }.into());
        let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
        quota.retry_after = Some(Duration::from_secs(120));
        rt.store.select(selection).unwrap().complete(Outcome::Failure(quota));
    };
    fail("a.json", "m1");
    fail("b.json", "m1");
    std::fs::write(dir.join("b.cds"), "untouched by a's change").unwrap();
    fail("a.json", "m2");
    let a = std::fs::read_to_string(dir.join("a.cds")).unwrap();
    assert!(a.contains("\"m1\"") && a.contains("\"m2\""), "{a}");
    assert_eq!(
        std::fs::read_to_string(dir.join("b.cds")).unwrap(),
        "untouched by a's change"
    );
    rt.store.reset_cooldown("a.json");
    assert!(!dir.join("a.cds").exists(), "a cleared credential's file is removed");
    assert_eq!(
        std::fs::read_to_string(dir.join("b.cds")).unwrap(),
        "untouched by a's change"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn preparation_waiter_observes_deletion_and_refresh_loop_is_replaceable() {
    let rt = Arc::new(crate::testing::runtime(
        crate::testing::config(""),
        vec![cred("a.json", "claude", false)],
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let lock = rt.store.prepare_lock("a.json");
    let guard = lock.lock().await;
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.prepare_credential("a.json", &rt.config(), None).await })
    };
    tokio::task::yield_now().await;
    assert!(!worker.is_finished());
    rt.store.reconcile(Vec::new());
    drop(guard);
    let error = worker.await.unwrap().unwrap_err();
    assert_eq!(error.scope, FailureScope::Request);
    assert!(rt.store.snapshot().is_empty());
    rt.start_auto_refresh();
    let first = rt.refresh_task.lock().unwrap().clone().unwrap();
    rt.start_auto_refresh();
    tokio::task::yield_now().await;
    assert!(first.is_finished());
    let second = rt.refresh_task.lock().unwrap().clone().unwrap();
    rt.stop_auto_refresh();
    tokio::task::yield_now().await;
    assert!(second.is_finished());
    assert!(rt.refresh_task.lock().unwrap().is_none());
}

/// The loop sets no timer while nothing can come due, and a credential added later
/// is tried after the store change, not on a schedule. Its failure is logged without
/// the refresh token.
#[tokio::test]
async fn refresh_loop_sleeps_until_the_store_changes() {
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let capture = Capture::default();
    let writer = capture.clone();
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish(),
    );
    let rt = Arc::new(crate::testing::runtime(
        crate::testing::config(""),
        Vec::new(),
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    rt.start_auto_refresh();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut due = cred("a.json", "claude", false);
    due.metadata.insert("refresh_token".into(), "fake-refresh".into());
    due.metadata.insert(
        "expired".into(),
        (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339().into(),
    );
    rt.store.reconcile(vec![due]);
    let added = rt.store.get("a.json").unwrap();
    // The refresh fails (no upstream; the OAuth client retries twice with 1 s and
    // 2 s pauses), which backs off for five minutes.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let retry = rt.refresh_state.lock().unwrap().retry_at(&added);
            if retry.is_some_and(|at| at > Instant::now() + Duration::from_secs(200)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the added credential was tried and backed off");
    rt.stop_auto_refresh();
    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("credential refresh failed: a.json (claude)") && log.contains("retry_in=300s"),
        "{log}"
    );
    assert!(!log.contains("fake-refresh"), "{log}");
}

#[tokio::test]
async fn every_lease_reports_exactly_once() {
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let stats = |s, f, c| AttemptStats {
        success: s,
        failure: f,
        cancelled: c,
    };

    // Dropped without completing (client gone mid-execute): cancelled.
    drop(store.select(sel("claude")).unwrap());
    assert_eq!(store.stats(), stats(0, 0, 1));
    // Completed explicitly: the drop that follows does not report again.
    store.select(sel("claude")).unwrap().complete(Outcome::Success);
    assert_eq!(store.stats(), stats(1, 0, 1));

    // A stream that errors and would then hang forever: the wrapper stops after Err.
    let err = ExecError::local(502, FailureScope::Transport, "boom");
    let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a")), Err(err)])
        .chain(futures_util::stream::pending())
        .boxed();
    let mut s = Completing::new(inner, store.select(sel("claude")).unwrap());
    assert!(s.next().await.unwrap().is_ok());
    assert!(s.next().await.unwrap().is_err());
    assert!(s.next().await.is_none(), "fused after the first error");
    drop(s);
    assert_eq!(store.stats(), stats(1, 1, 1));

    let mut done = Completing::new(
        futures_util::stream::empty().boxed(),
        store.select(sel("claude")).unwrap(),
    );
    assert!(done.next().await.is_none());
    drop(done);
    assert_eq!(store.stats(), stats(2, 1, 1));

    // Dropped mid-stream: cancelled.
    let mut mid = Completing::new(
        futures_util::stream::iter(vec![Ok(Bytes::new())])
            .chain(futures_util::stream::pending())
            .boxed(),
        store.select(sel("claude")).unwrap(),
    );
    assert!(mid.next().await.is_some());
    drop(mid);
    assert_eq!(store.stats(), stats(2, 1, 2));
}

/// A remote lease ends once, never through the local scheduler, and only after the
/// upstream body it was streaming has been dropped.
#[tokio::test]
async fn remote_leases_end_after_the_upstream_body_closes() {
    use std::sync::atomic::AtomicBool;
    struct Probe(Arc<AtomicBool>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let dropped = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut lease = store.select(sel("claude")).unwrap();
    let (seen, probe) = (observed.clone(), dropped.clone());
    lease.remote = Some(crate::remote::RemoteEnd {
        end: Some(Box::new(move || {
            seen.lock().unwrap().push(probe.load(Ordering::SeqCst));
            None
        })),
        releases: Default::default(),
        cancel: None,
    });
    let guard = Probe(dropped.clone());
    let err = ExecError::local(502, FailureScope::Transport, "boom");
    let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a")), Err(err)])
        .chain(futures_util::stream::pending())
        .map(move |item| {
            let _ = &guard;
            item
        })
        .boxed();
    let mut stream = Completing::new(inner, lease);
    assert!(stream.next().await.unwrap().is_ok());
    assert!(observed.lock().unwrap().is_empty(), "still streaming");
    assert!(stream.next().await.unwrap().is_err());
    assert_eq!(
        *observed.lock().unwrap(),
        vec![true],
        "body dropped first, then one end"
    );
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(
        store.stats(),
        AttemptStats::default(),
        "the local scheduler saw nothing"
    );
}
/// Go binds the attempt's cancellation to the Home selection: a drain closes the
/// upstream body and ends the lease while the client has stopped reading
/// (backpressure), and the reader then sees the cancellation once.
#[tokio::test]
async fn a_drain_ends_a_remote_stream_the_client_stopped_reading() {
    use std::sync::atomic::AtomicBool;
    struct Probe(Arc<AtomicBool>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
    let dropped = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut lease = store.select(sel("claude")).unwrap();
    let (seen, probe) = (observed.clone(), dropped.clone());
    let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
    lease.remote = Some(crate::remote::RemoteEnd {
        end: Some(Box::new(move || {
            seen.lock().unwrap().push(probe.load(Ordering::SeqCst));
            None
        })),
        releases: Default::default(),
        cancel: Some(cancel),
    });
    let guard = Probe(dropped.clone());
    let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a"))])
        .chain(futures_util::stream::pending())
        .map(move |item| {
            let _ = &guard;
            item
        })
        .boxed();
    let mut stream = Completing::new(inner, lease);
    assert!(stream.next().await.unwrap().is_ok());
    // The client stops polling; the dispatcher drains.
    cancel_tx.send(true).unwrap();
    for _ in 0..100 {
        if !observed.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *observed.lock().unwrap(),
        vec![true],
        "body dropped first, then one end, without a poll"
    );
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(String::from_utf8_lossy(&error.body), "context canceled");
    assert!(stream.next().await.is_none());
    drop(stream);
    assert_eq!(observed.lock().unwrap().len(), 1);
}
