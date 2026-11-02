use super::cooldown::{Cooldown, MAX_TRUST_WINDOW, PROBE_HOLD};
use super::pick::session_keys;
use super::policy::{compat_cooling, parse_oauth_rules, weight};
use super::*;
use cpa_core::config::RoutingConfig;
use std::path::Path;

#[test]
fn entitlement_cooldown_stays_bound_to_the_requested_account_and_model() {
    let c = cred("restricted", serde_json::json!({}));
    let other = cred("available", serde_json::json!({}));
    let mut scheduler = Scheduler::default();
    let now = Instant::now();
    let error = ExecError::local(
        403,
        FailureScope::Credential,
        r#"{"error":{"code":"model_access_denied"}}"#,
    );
    scheduler.record(&c, "blocked-model", &Outcome::Failure(error), &Policy::default(), now);
    assert_eq!(
        scheduler.wait(&c, "blocked-model", now),
        Some(Duration::from_secs(43200))
    );
    assert_eq!(scheduler.wait(&c, "other-model", now), None);
    assert_eq!(scheduler.wait(&other, "blocked-model", now), None);
}

fn tag<'a>(creds: &[&'a Credential]) -> Vec<(&'a Credential, &'static str)> {
    creds.iter().map(|c| (*c, "claude")).collect()
}

fn cred(id: &str, extra: Value) -> Credential {
    let mut metadata = extra.as_object().unwrap().clone();
    metadata.insert("type".into(), "claude".into());
    Credential::from_file(Path::new("/mock"), &Path::new("/mock").join(id), metadata).unwrap()
}

fn selection(model: &str) -> Selection {
    Selection {
        provider: "claude".into(),
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn normalization_matches_go_runtime_not_example_config() {
    for (raw, expected) in [
        (" WRR ", Strategy::WeightedRoundRobin),
        ("weightedroundrobin", Strategy::WeightedRoundRobin),
        ("weighted-round-robin", Strategy::WeightedRoundRobin),
        (" FF", Strategy::FillFirst),
        ("fillfirst", Strategy::FillFirst),
        ("fill-first", Strategy::FillFirst),
        ("soonest-reset", Strategy::SoonestReset),
        (" Reset-First ", Strategy::SoonestReset),
        ("resetfirst", Strategy::SoonestReset),
        ("", Strategy::RoundRobin),
        ("anything", Strategy::RoundRobin),
    ] {
        assert_eq!(
            Policy::from(&RoutingConfig {
                strategy: raw.into(),
                ..Default::default()
            })
            .strategy,
            expected
        );
    }
    for (ttl, seconds) in [
        ("", 3600),
        ("bad", 3600),
        ("-1h", 3600),
        ("0", 3600),
        ("0.5s", 1),
        ("0.0000000001s", 3600),
        ("1h30m", 5400),
        (" 2m ", 120),
    ] {
        assert_eq!(
            Policy::from(&RoutingConfig {
                session_affinity_ttl: ttl.into(),
                ..Default::default()
            })
            .session_affinity_ttl,
            Duration::from_secs(seconds),
            "{ttl}"
        );
    }
    let policy = Policy::from(&RoutingConfig {
        retry: cpa_core::config::RetryConfig {
            request_retry: -1,
            max_retry_credentials: -5,
            max_retry_interval: -3,
        },
        session_affinity_subagents: Some(false),
        ..Default::default()
    });
    assert_eq!(policy.request_retry, 0);
    assert_eq!(policy.max_retry_credentials, 0);
    assert_eq!(policy.max_retry_interval, Duration::ZERO);
    assert!(policy.session_affinity_subagents, "ignored when affinity disabled");
    assert!(
        !Policy::from(&RoutingConfig {
            session_affinity: true,
            session_affinity_subagents: Some(false),
            ..Default::default()
        })
        .session_affinity_subagents
    );
    assert!(
        Policy::from(&RoutingConfig {
            session_affinity: true,
            ..Default::default()
        })
        .session_affinity_subagents
    );
}

#[test]
fn go_duration_decimal_compound_units_and_overflow() {
    for (raw, expected) in [
        ("1h2m3.4s", 3_723_400_000_000),
        (".5s", 500_000_000),
        ("+1.000000001s", 1_000_000_001),
        ("1.s", 1_000_000_000),
        ("1µs2μs3us", 6000),
        ("0.3333333333333333333h", 1_200_000_000_000),
        ("0.100000000000000000000h", 360_000_000_000),
        ("-9223372036854775808ns", i64::MIN),
        ("9223372036854775807ns", i64::MAX),
        ("0.9ns", 0),
    ] {
        assert_eq!(cpa_core::config::parse_duration(raw), Some(expected), "{raw}");
    }
    for raw in [
        "1d",
        ".s",
        "1",
        "1h 2m",
        "1s-2s",
        "1e3s",
        "9223372036854775808ns",
        "-9223372036854775809ns",
    ] {
        assert_eq!(cpa_core::config::parse_duration(raw), None, "{raw}");
    }
}

#[test]
fn rotation_tracks_previous_identity_and_provider_model_keys() {
    let a = cred("a", serde_json::json!({}));
    let b = cred("b", serde_json::json!({}));
    let c = cred("c", serde_json::json!({}));
    let p = Policy::default();
    let now = Instant::now();
    let mut s = Scheduler::default();
    assert_eq!(s.pick(&tag(&[&c, &b, &a]), &selection("m"), &p, now).unwrap().id, "a");
    assert_eq!(s.pick(&tag(&[&a, &c]), &selection("m"), &p, now).unwrap().id, "c");
    assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
    assert_eq!(s.pick(&tag(&[&a, &b]), &selection("other"), &p, now).unwrap().id, "a");
    assert_eq!(s.pick(&tag(&[&a, &b]), &selection("m(high)"), &p, now).unwrap().id, "b");
}

#[test]
fn smooth_weights_keep_signed_credits_across_temporary_exclusions() {
    let a = cred("a", serde_json::json!({"weight":5}));
    let b = cred("b", serde_json::json!({"weight":1}));
    let c = cred("c", serde_json::json!({"weight":1}));
    let p = Policy {
        strategy: Strategy::WeightedRoundRobin,
        ..Default::default()
    };
    let now = Instant::now();
    let mut s = Scheduler::default();
    let picks: Vec<_> = (0..7)
        .map(|_| {
            s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now)
                .unwrap()
                .id
                .clone()
        })
        .collect();
    assert_eq!(picks, ["a", "a", "b", "a", "c", "a", "a"]);
    let mut s = Scheduler::default();
    assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
    assert_eq!(s.pick(&tag(&[&b, &c]), &selection("m"), &p, now).unwrap().id, "b");
    assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
    assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "c");
    for v in [
        serde_json::json!(0),
        serde_json::json!(-3),
        serde_json::json!(1_000_001),
        serde_json::json!(1.5),
        serde_json::json!("bad"),
    ] {
        assert!(!s.admits(&cred("off", serde_json::json!({"weight":v})), &p));
    }
    assert_eq!(weight(&cred("max", serde_json::json!({"weight":1_000_000}))), 1_000_000);
    assert_eq!(weight(&cred("float", serde_json::json!({"weight":2.0}))), 2);
}

#[test]
fn affinity_beats_recovered_priority_until_unusable_or_expired() {
    let low = cred("a", serde_json::json!({"priority":-1}));
    let high = cred("b", serde_json::json!({"priority":9}));
    let p = Policy {
        strategy: Strategy::FillFirst,
        session_affinity: true,
        session_affinity_ttl: Duration::from_secs(10),
        ..Default::default()
    };
    let sel = Selection {
        session: Some("session".into()),
        ..selection("m")
    };
    let now = Instant::now();
    let mut s = Scheduler::default();
    assert_eq!(s.pick(&tag(&[&low]), &sel, &p, now).unwrap().id, "a");
    assert_eq!(
        s.pick(&tag(&[&low, &high]), &sel, &p, now + Duration::from_secs(5))
            .unwrap()
            .id,
        "a"
    );
    assert_eq!(
        s.pick(&tag(&[&low, &high]), &sel, &p, now + Duration::from_secs(15))
            .unwrap()
            .id,
        "b"
    );
    assert_eq!(
        s.pick(&tag(&[&low]), &sel, &p, now + Duration::from_secs(16))
            .unwrap()
            .id,
        "a"
    );
    assert_eq!(s.pick(&tag(&[&low, &high]), &selection("m"), &p, now).unwrap().id, "b");
}

#[test]
fn max_trusted_cooldown_reads_durations_seconds_and_off() {
    let read = |raw: &str| {
        Policy::from(&RoutingConfig {
            cooldown: cpa_core::config::CooldownConfig {
                max_trusted_cooldown: raw.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .max_trusted_cooldown
    };
    assert_eq!(read(""), Duration::from_secs(3600));
    assert_eq!(read("90m"), Duration::from_secs(5400));
    assert_eq!(read("120"), Duration::from_secs(120));
    assert_eq!(read("0"), Duration::ZERO);
    assert_eq!(read("0s"), Duration::ZERO);
    assert_eq!(read("-5"), Duration::ZERO);
    for unreadable in ["soon", "1d", "false", "1.5"] {
        assert_eq!(read(unreadable), Duration::from_secs(3600), "{unreadable}");
    }
    assert_eq!(read("3"), Duration::from_secs(10), "clamped to Go's floor");
    assert_eq!(read("2s"), Duration::from_secs(10), "clamped to Go's floor");
}

const H: u64 = 3600;
const MIN: u64 = 60;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A 429 whose stated reset is `stated` from `at`.
fn hinted(scope: FailureScope, stated: Duration) -> Outcome {
    let mut e = ExecError::local(429, scope, "usage_limit_reached");
    e.retry_after = Some(stated);
    Outcome::Failure(e)
}

fn unhinted(scope: FailureScope) -> Outcome {
    Outcome::Failure(ExecError::local(429, scope, "rate limited"))
}

fn entry<'a>(s: &'a Scheduler, c: &Credential, model: &str) -> &'a Cooldown {
    s.cooldown(&c.id, model).unwrap()
}

/// A stated reset six days away is trusted for one hour, then for doubling windows
/// while the account keeps answering 429, and never past the stated reset. Success
/// clears the escalation; `0` trusts the stated reset as Go does.
#[test]
fn quota_resets_are_trusted_up_to_a_doubling_bound() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let reset = start + secs(6 * 24 * H);
    for (scope, key) in [(FailureScope::Model, "m"), (FailureScope::Credential, "")] {
        let mut s = Scheduler::default();
        let mut now = start;
        for window in [1, 2, 4, 8, 16, 32, 64] {
            s.record(&c, "m", &hinted(scope, reset - now), &p, now);
            assert_eq!(s.wait(&c, "m", now), Some(secs(window * H)), "{scope:?}");
            assert_eq!(entry(&s, &c, key).trust.stated, Some(reset), "{scope:?}: stated kept");
            assert_eq!(entry(&s, &c, key).level, 0, "{scope:?}: Go's level untouched");
            now += secs(window * H);
            assert!(s.retry_eligible(&c, "m", now));
        }
        // 127 hours have passed; the next window would be 128 hours, past the reset.
        s.record(&c, "m", &hinted(scope, reset - now), &p, now);
        assert_eq!(
            s.wait(&c, "m", now),
            Some(reset - now),
            "{scope:?}: capped by the stated reset"
        );
        // The account came back early: success clears the escalation.
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(scope, reset - start), &p, start);
        let then = start + secs(H);
        s.record(&c, "m", &hinted(scope, reset - then), &p, then);
        assert_eq!(s.wait(&c, "m", then), Some(secs(2 * H)));
        let later = start + secs(3 * H);
        s.record(&c, "m", &Outcome::Success, &p, later);
        s.record(&c, "m", &hinted(scope, reset - later), &p, later);
        assert_eq!(
            s.wait(&c, "m", later),
            Some(secs(H)),
            "{scope:?}: back to the first bound"
        );
    }
    // A reset shorter than the bound is kept as stated.
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, secs(30 * MIN)), &p, start);
    assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
    assert!(!entry(&s, &c, "m").trust.bounded);
    // `0` is Go: the whole stated reset.
    let go = Policy {
        max_trusted_cooldown: Duration::ZERO,
        ..Policy::default()
    };
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, reset - start), &go, start);
    assert_eq!(s.wait(&c, "m", start), Some(reset - start));
}

/// The trust count is its own: Go's no-hint backoff level neither raises the first
/// bound nor is changed by a cut window.
#[test]
fn trust_count_and_go_backoff_level_are_separate() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    // No-hint 429s first: Go's level climbs (1 s, 2 s, 4 s, ...).
    let mut s = Scheduler::default();
    let mut now = Instant::now();
    for _ in 0..9 {
        s.record(&c, "m", &unhinted(FailureScope::Model), &p, now);
        now += secs(600);
    }
    assert_eq!(entry(&s, &c, "m").level, 9);
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    assert_eq!(s.wait(&c, "m", now), Some(secs(H)), "the first bound, not 2^9 hours");
    assert_eq!(entry(&s, &c, "m").level, 9, "Go's level keeps its meaning");
    // Hinted first, then no-hint 429s after the window: Go's backoff runs from its
    // own level, and the next cut window still doubles.
    let mut s = Scheduler::default();
    let mut now = Instant::now();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    now += secs(H);
    s.record(&c, "m", &unhinted(FailureScope::Model), &p, now);
    assert_eq!(s.wait(&c, "m", now), Some(secs(1)), "Go's first backoff step");
    now += secs(1);
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    assert_eq!(s.wait(&c, "m", now), Some(secs(2 * H)), "the trust count survived");
}

/// A stated reset that lands during a cooldown the bound did not set (a 5xx, a
/// 401, a no-hint 429 backoff) opens a bounded window instead of probing at once.
#[test]
fn a_reset_inside_another_cooldown_gets_the_bound() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    let first = |status: u16, scope| {
        let mut s = Scheduler::default();
        s.record(
            &c,
            "m",
            &Outcome::Failure(ExecError::local(status, scope, "earlier")),
            &p,
            start,
        );
        s
    };
    for (status, scope, earlier) in [
        (503, FailureScope::Credential, secs(60)),
        (429, FailureScope::Model, secs(1)),
    ] {
        let mut s = first(status, scope);
        assert_eq!(s.wait(&c, "m", start), Some(earlier));
        let at = start + Duration::from_millis(500);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, at);
        assert_eq!(s.wait(&c, "m", at), Some(secs(H)), "after {status}");
        assert!(entry(&s, &c, "m").trust.bounded, "after {status}");
        // A concurrent 6-day answer inside that window neither extends nor escalates.
        let later = at + secs(60);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, later);
        assert_eq!(s.wait(&c, "m", at), Some(secs(H)), "after {status}");
        assert_eq!(entry(&s, &c, "m").trust.window, Some(0), "after {status}");
    }
    // A longer cooldown from another cause stands, and is not marked bounded.
    let mut s = first(401, FailureScope::Credential);
    let cap = Policy {
        max_trusted_cooldown: secs(600),
        ..Policy::default()
    };
    s.record(&c, "m", &hinted(FailureScope::Model, days), &cap, start);
    assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
    assert!(!entry(&s, &c, "m").trust.bounded);
}

/// Inside a live bounded window, a concurrent stated reset keeps the probe deadline
/// unless it is earlier; with the setting off Go's merge applies.
#[test]
fn concurrent_resets_keep_or_shorten_the_window() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let window = |s: &mut Scheduler, policy: &Policy| {
        s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), policy, start);
    };
    // A 45-minute reset at minute 30 does not push the probe to minute 75.
    let mut s = Scheduler::default();
    window(&mut s, &p);
    let at = start + secs(30 * MIN);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(45 * MIN)), &p, at);
    assert_eq!(s.wait(&c, "m", start), Some(secs(H)));
    // A 30-minute reset at minute 1 moves it up to minute 31.
    let mut s = Scheduler::default();
    window(&mut s, &p);
    let at = start + secs(MIN);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(30 * MIN)), &p, at);
    assert_eq!(s.wait(&c, "m", start), Some(secs(31 * MIN)));
    assert_eq!(entry(&s, &c, "m").trust.window, Some(0), "not a new window");
    // Setting off: Go keeps the later deadline.
    let go = Policy {
        max_trusted_cooldown: Duration::ZERO,
        ..Policy::default()
    };
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, secs(H)), &go, start);
    let at = start + secs(30 * MIN);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(45 * MIN)), &go, at);
    assert_eq!(s.wait(&c, "m", start), Some(secs(75 * MIN)));
}

/// The first pick after a bounded window reserves the probe: the window keeps
/// cooling until the probe's answer, and is never waited for by retries.
#[test]
fn one_probe_per_ended_window() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    for (scope, key) in [(FailureScope::Model, "m"), (FailureScope::Credential, "")] {
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(scope, secs(6 * 24 * H)), &p, start);
        let near = start + secs(H) - secs(5);
        assert!(
            !s.retry_eligible(&c, "m", near),
            "{scope:?}: a bounded window is not waited for"
        );
        let end = start + secs(H);
        assert_eq!(s.wait(&c, "m", end), None, "{scope:?}: the probe is due");
        s.reserve_probe(&c, "m", end);
        assert_eq!(s.wait(&c, "m", end), Some(PROBE_HOLD), "{scope:?}: reserved");
        assert!(s.quota_cooling(&c, "m", end), "{scope:?}");
        assert!(!s.retry_eligible(&c, "m", end), "{scope:?}");
        // An unrelated model of the same credential waits too when the window is the
        // credential's.
        assert_eq!(s.wait(&c, "other", end).is_some(), key.is_empty(), "{scope:?}");
        // The probe's 429 opens the next window; its success would clear all.
        s.record(&c, "m", &hinted(scope, secs(5 * 24 * H)), &p, end + secs(2));
        assert_eq!(s.wait(&c, "m", end + secs(2)), Some(secs(2 * H)), "{scope:?}");
        assert_eq!(entry(&s, &c, key).trust.probe, None);
        // A cancelled probe frees the window at once; a lost one after PROBE_HOLD.
        let next = end + secs(2) + secs(2 * H);
        s.reserve_probe(&c, "m", next);
        s.record(&c, "m", &Outcome::Cancelled, &p, next + secs(1));
        assert_eq!(s.wait(&c, "m", next + secs(1)), None, "{scope:?}");
        s.reserve_probe(&c, "m", next + secs(1));
        assert_eq!(s.wait(&c, "m", next + secs(1) + PROBE_HOLD), None, "{scope:?}");
    }
    // Only bounded windows reserve: an ended Go backoff does not.
    let mut s = Scheduler::default();
    s.record(&c, "m", &unhinted(FailureScope::Model), &p, start);
    s.reserve_probe(&c, "m", start + secs(2));
    assert_eq!(s.wait(&c, "m", start + secs(2)), None);
}

/// A credential-wide window escalates the model keys too; when the credential
/// recovers through a success on another model, they forget the count.
#[test]
fn a_success_on_another_model_clears_the_escalation() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let days = secs(6 * 24 * H);
    let mut s = Scheduler::default();
    s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
    assert_eq!(entry(&s, &c, "a").trust.window, Some(0));
    let end = start + secs(H);
    s.record(&c, "b", &Outcome::Success, &p, end);
    s.record(&c, "a", &hinted(FailureScope::Model, days), &p, end);
    assert_eq!(s.wait(&c, "a", end), Some(secs(H)), "the first bound again");
}

/// A probe the upstream accepted (a stream started) ends the window's question at
/// once: the next pick goes through while the stream runs, and a later hinted 429
/// still doubles. A credential-wide window's copies on model keys are disarmed too;
/// a newer live window is left alone.
#[test]
fn an_accepted_probe_frees_the_account_and_keeps_the_count() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    let end = start + secs(H);
    // Model scope.
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
    s.reserve_probe(&c, "m", end);
    assert!(s.wait(&c, "m", end).is_some(), "reserved");
    s.accept_probe(&c, "m", end);
    assert_eq!(s.wait(&c, "m", end), None, "the next pick goes through");
    s.reserve_probe(&c, "m", end + secs(1));
    assert_eq!(s.wait(&c, "m", end + secs(1)), None, "no second reservation");
    let later = end + secs(60);
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, later);
    assert_eq!(s.wait(&c, "m", later), Some(secs(2 * H)), "the count survived");
    // A live window (another model's newer one) is untouched.
    s.accept_probe(&c, "m", later + secs(1));
    assert!(entry(&s, &c, "m").trust.bounded);
    // Credential scope: B's accepted probe disarms A's copy as well.
    let mut s = Scheduler::default();
    s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
    s.reserve_probe(&c, "b", end);
    s.accept_probe(&c, "b", end);
    assert_eq!(s.wait(&c, "b", end), None);
    s.reserve_probe(&c, "a", end);
    assert_eq!(s.wait(&c, "a", end), None, "A's copy does not reserve again");
    assert_eq!(entry(&s, &c, "a").trust.window, Some(0), "count kept");
}

/// The credential's probe answers its window and the window's copies on model keys,
/// not a model's own window that ended unprobed before it: that model still gets a
/// probe of its own, after a streamed probe and after a plain success alike, and the
/// credential probe's later success leaves that model's newer probe state alone.
#[test]
fn a_credential_probe_leaves_a_models_own_ended_window() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    for streamed in [true, false] {
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
        let opened = start + secs(H) + secs(MIN);
        s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, opened);
        let end = opened + secs(H);
        s.reserve_probe(&c, "b", end);
        if streamed {
            s.accept_probe(&c, "b", end);
        } else {
            s.record(&c, "b", &Outcome::Success, &p, end);
        }
        s.reserve_probe(&c, "a", end);
        assert_eq!(s.wait(&c, "a", end), None, "streamed {streamed}: A's copy is answered");
        s.reserve_probe(&c, "m", end);
        assert_eq!(
            s.wait(&c, "m", end),
            Some(PROBE_HOLD),
            "streamed {streamed}: m reserves its own probe"
        );
    }
    // The credential's stream started, then m's probe was taken over by a later one
    // that was accepted too. The credential stream's success comes from an older
    // lease: it leaves m's accepted window alone, so the superseded m probe's late
    // 429 is still ignored and the later probe's success frees m.
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
    let opened = start + secs(H) + secs(MIN);
    s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, opened);
    let end = opened + secs(H);
    let credential_probe = s.reserve_probe(&c, "b", end);
    s.accept_probe_picked(&c, "b", credential_probe, end);
    let first = s.reserve_probe(&c, "m", end);
    let lapsed = end + PROBE_HOLD + secs(1);
    let second = s.reserve_probe(&c, "m", lapsed);
    assert!(credential_probe != first && first != second, "real generations");
    s.accept_probe_picked(&c, "m", second, lapsed);
    let done = lapsed + secs(1);
    s.record_picked(&c, "b", &Outcome::Success, &p, credential_probe, done);
    s.record_picked(&c, "m", &hinted(FailureScope::Model, days), &p, first, done);
    assert_eq!(s.wait(&c, "m", done), None, "no window from the superseded probe");
    s.record_picked(&c, "m", &Outcome::Success, &p, second, done + secs(1));
    assert!(s.cooldown(&c.id, "m").is_none(), "m recovered");
}

/// A probe whose hold lapsed was taken over by a later probe: its late 429 neither
/// opens the next window nor touches the later probe's reservation, also once the
/// later probe was accepted, so the later probe's success frees the account.
#[test]
fn a_superseded_probes_429_leaves_the_window_to_the_current_probe() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    for scope in [FailureScope::Model, FailureScope::Credential] {
        // The second probe is still unanswered, or already accepted (its stream's
        // first event arrived) when the first one's 429 comes back.
        for accepted in [false, true] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &hinted(scope, days), &p, start);
            let end = start + secs(H);
            let first = s.reserve_probe(&c, "m", end);
            let lapsed = end + PROBE_HOLD + secs(1);
            let second = s.reserve_probe(&c, "m", lapsed);
            if accepted {
                s.accept_probe_picked(&c, "m", second, lapsed);
            }
            let late = lapsed + secs(1);
            s.record_picked(&c, "m", &hinted(scope, days - secs(H)), &p, first, late);
            assert_eq!(
                s.wait(&c, "m", late),
                (!accepted).then(|| PROBE_HOLD - secs(1)),
                "{scope:?}, accepted {accepted}: still the second probe's"
            );
            s.record_picked(&c, "m", &Outcome::Success, &p, second, late + secs(1));
            assert_eq!(
                s.wait(&c, "m", late + secs(1)),
                None,
                "{scope:?}, accepted {accepted}: recovered"
            );
        }
    }
}

/// A credential-wide window that its probe's 429 renews carries its copies on model
/// keys along, ended ones included: when the next probe recovers the account, no copy
/// is left to ask for a probe of its own or to keep the count.
#[test]
fn a_renewed_credential_window_carries_its_ended_copies() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    for streamed in [true, false] {
        let mut s = Scheduler::default();
        s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
        let first_end = start + secs(H);
        s.reserve_probe(&c, "b", first_end);
        let renewed = first_end + secs(1);
        s.record(&c, "b", &hinted(FailureScope::Credential, days - secs(H)), &p, renewed);
        let second_end = renewed + secs(2 * H);
        s.reserve_probe(&c, "c", second_end);
        if streamed {
            s.accept_probe(&c, "c", second_end);
        } else {
            s.record(&c, "c", &Outcome::Success, &p, second_end);
        }
        s.reserve_probe(&c, "a", second_end);
        assert_eq!(s.wait(&c, "a", second_end), None, "streamed {streamed}: no probe for A");
        if !streamed {
            s.record(&c, "a", &hinted(FailureScope::Model, days), &p, second_end);
            assert_eq!(s.wait(&c, "a", second_end), Some(secs(H)), "the count is cleared");
        }
    }
}

/// A lease picked outside selection (the credits fallback) on a credential whose
/// ended window has a probe in flight neither frees that probe nor ends the window,
/// whatever its outcome; nor does it answer a window opened after it.
#[test]
fn outside_selection_leases_answer_no_bounded_window() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, start);
    let ended = start + secs(H);
    let probe = s.reserve_probe(&c, "m", ended);
    let credits = s.outside_selection_generation();
    for outcome in [Outcome::Cancelled, Outcome::Success] {
        s.record_picked(&c, "m", &outcome, &p, credits, ended);
        assert!(s.wait(&c, "m", ended).is_some(), "the reserved probe holds the window");
    }
    s.accept_probe_picked(&c, "m", credits, ended);
    assert!(s.wait(&c, "m", ended).is_some(), "a credits stream accepts nothing");
    s.record_picked(&c, "m", &Outcome::Success, &p, probe, ended);
    assert_eq!(s.wait(&c, "m", ended), None, "the probe's own success ends it");
    // A window opened after the credits lease was picked is newer than it too.
    let credits = s.outside_selection_generation();
    let later = ended + secs(MIN);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, later);
    let reopened = later + secs(H);
    s.reserve_probe(&c, "m", reopened);
    s.record_picked(&c, "m", &Outcome::Cancelled, &p, credits, reopened);
    assert!(s.wait(&c, "m", reopened).is_some(), "the later probe still holds");
}

/// Generations compare in serial-number order: a window opened just after the counter
/// wraps is newer than a lease picked just before, and older than one picked after.
#[test]
fn generations_compare_across_the_wrap() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let mut s = Scheduler {
        generation: u32::MAX,
        ..Scheduler::default()
    };
    let before = s.reserve_probe(&c, "m", start);
    assert_eq!(before, u32::MAX);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, start);
    assert_eq!(entry(&s, &c, "m").trust.generation, 0, "wrapped");
    let at = start + secs(MIN);
    s.record_picked(&c, "m", &Outcome::Success, &p, before, at);
    assert_eq!(s.wait(&c, "m", at), Some(secs(H - MIN)), "an older success leaves it");
    s.record_picked(&c, "m", &Outcome::Success, &p, 0, at);
    assert_eq!(s.wait(&c, "m", at), None, "a newer success ends it");
}

/// An earlier credential-wide reset moves up the model keys that inherited the
/// window; an independent longer cooldown stands.
#[test]
fn an_earlier_credential_reset_shortens_inherited_model_windows() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let mut s = Scheduler::default();
    let support = ExecError::local(400, FailureScope::Model, "model is not supported");
    s.record(&c, "support", &Outcome::Failure(support), &p, start);
    s.record(&c, "sibling", &unhinted(FailureScope::Model), &p, start);
    s.record(&c, "a", &hinted(FailureScope::Credential, secs(6 * 24 * H)), &p, start);
    let at = start + secs(MIN);
    s.record(&c, "a", &hinted(FailureScope::Credential, secs(30 * MIN)), &p, at);
    for model in ["a", "sibling", "unrelated"] {
        assert_eq!(s.wait(&c, model, start), Some(secs(31 * MIN)), "{model}");
    }
    assert_eq!(s.wait(&c, "support", start), Some(secs(12 * H)));
}

/// A model's own bounded window that a credential-wide window covers keeps its own
/// probe time: an earlier credential reset moves the credential's probe up, never
/// the model's before its own bound, whether the credential's 429 came from another
/// model or from that one.
#[test]
fn an_earlier_credential_reset_never_moves_a_models_own_probe_earlier() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    let own_end = start + secs(H);
    for failing in ["a", "m"] {
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
        let opened = start + secs(30 * MIN);
        s.record(&c, failing, &hinted(FailureScope::Credential, days), &p, opened);
        let at = opened + secs(MIN);
        s.record(&c, failing, &hinted(FailureScope::Credential, secs(10 * MIN)), &p, at);
        let moved = at + secs(10 * MIN);
        assert_eq!(
            s.wait(&c, "other", moved),
            None,
            "{failing}: the credential's probe is due"
        );
        assert_eq!(
            s.wait(&c, "m", moved),
            Some(own_end - moved),
            "{failing}: m waits for its own bound"
        );
    }
}

/// A model's own cooldown of another cause (a 401's 30 minutes) under a bounded
/// credential window keeps its own deadline, as Go keeps the later one: an earlier
/// credential reset brings the credential's probe forward, never the model back
/// before its own deadline, whether the credential's 429 came from another model or
/// from that one.
#[test]
fn an_earlier_credential_reset_never_shortens_a_models_own_cooldown() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let unauthorized = Outcome::Failure(ExecError::local(401, FailureScope::Credential, "unauthorized"));
    for failing in ["a", "m"] {
        let mut s = Scheduler::default();
        s.record(&c, "m", &unauthorized, &p, start);
        assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
        let opened = start + secs(MIN);
        s.record(
            &c,
            failing,
            &hinted(FailureScope::Credential, secs(6 * 24 * H)),
            &p,
            opened,
        );
        let at = start + secs(2 * MIN);
        s.record(&c, failing, &hinted(FailureScope::Credential, secs(MIN)), &p, at);
        let moved = start + secs(3 * MIN);
        assert_eq!(
            s.wait(&c, "other", moved),
            None,
            "{failing}: the credential's probe is due"
        );
        assert_eq!(
            s.wait(&c, "m", moved),
            Some(secs(27 * MIN)),
            "{failing}: m keeps its own 30 minutes"
        );
    }
}

/// Provenance follows the winning deadline: the failing model's own longer bounded
/// window keeps its flag and stated reset through a credential-wide merge.
#[test]
fn the_own_key_keeps_its_window_through_a_credential_merge() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    let start = Instant::now();
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
    s.record(
        &c,
        "m",
        &hinted(FailureScope::Model, days - secs(H)),
        &p,
        start + secs(H),
    );
    let own_end = start + secs(3 * H);
    let at = start + secs(H) + secs(MIN);
    s.record(&c, "m", &hinted(FailureScope::Credential, days), &p, at);
    let own = entry(&s, &c, "m");
    assert_eq!(own.deadline, own_end);
    assert!(own.trust.bounded && own.trust.stated == Some(start + days));
    s.reserve_probe(&c, "m", own_end);
    assert!(
        entry(&s, &c, "m").trust.probe.is_some(),
        "it reserves a probe at its end"
    );
}

/// A restore into a scheduler that already holds a later deadline keeps that
/// entry's provenance and classification, merging the counts.
#[test]
fn a_restore_keeps_the_winner_trust() {
    use crate::cooldown_store::{Quota, Record};
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let (now, wall) = (Instant::now(), SystemTime::now());
    let days = secs(6 * 24 * H);
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    let live = entry(&s, &c, "m").trust;
    let shorter = Record {
        auth_id: "a".into(),
        model: "m".into(),
        next_retry_after: Some(wall + secs(10 * MIN)),
        quota: Quota {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(wall + secs(10 * MIN)),
            trust_windows: Some(3),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(s.restore(&c, &shorter, true, secs(H), now, wall));
    let merged = entry(&s, &c, "m");
    assert_eq!(merged.deadline, now + secs(H));
    assert!(merged.trust.bounded && merged.trust.stated == live.stated);
    assert_eq!(merged.trust.window, Some(2), "counts merge");
    assert_eq!(merged.trust.generation, live.generation, "the live window's generation");
}

/// A restore while serving (`save-cooldown-status` turned on) that reinstalls a
/// bounded window gives it a fresh generation: a stream picked before the window
/// opened still cannot end it.
#[test]
fn a_restore_while_serving_keeps_the_window_newer_than_older_leases() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let (now, wall) = (Instant::now(), SystemTime::now());
    let mut s = Scheduler::default();
    let stream = s.reserve_probe(&c, "m", now);
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, now);
    let saved = s.records(&c, now, wall);
    assert!(s.restore(&c, &saved[0], true, secs(H), now, wall));
    assert!(entry(&s, &c, "m").trust.bounded);
    let at = now + secs(MIN);
    s.record_picked(&c, "m", &Outcome::Success, &p, stream, at);
    assert_eq!(s.wait(&c, "m", at), Some(secs(H - MIN)), "the window stands");
}

/// A 503 from a request already in flight inside a live bounded window: the window
/// stands, and its saved record keeps the stated reset and count.
#[test]
fn a_503_inside_a_bounded_window_keeps_it() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let (now, wall) = (Instant::now(), SystemTime::now());
    let days = secs(6 * 24 * H);
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    let at = now + secs(30 * MIN);
    let unavailable = ExecError::local(503, FailureScope::Model, "overloaded");
    s.record(&c, "m", &Outcome::Failure(unavailable), &p, at);
    let saved = &s.records(&c, at, wall + secs(30 * MIN))[0];
    assert!(saved.quota.exceeded);
    assert_eq!(saved.quota.next_recover_at, Some(wall + days));
    assert_eq!(saved.quota.trust_windows, Some(1));
    assert_eq!(saved.reason, "quota");
}

/// A Cloudflare challenge from a request already in flight inside a live bounded
/// window leaves the window as it is but still raises Go's level, as Go does.
#[test]
fn a_cloudflare_challenge_inside_a_bounded_window_raises_the_level() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let start = Instant::now();
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, start);
    let challenge = Outcome::Failure(ExecError::local(403, FailureScope::Model, "cloudflare challenge"));
    for (n, level) in [(1, 1), (2, 2)] {
        s.record(&c, "m", &challenge, &p, start + secs(n * MIN));
        assert_eq!(entry(&s, &c, "m").level, level);
        assert_eq!(s.wait(&c, "m", start), Some(secs(H)), "the window stands");
        assert!(entry(&s, &c, "m").trust.bounded);
    }
}

/// `.cds` records: the probe time is the retry, the stated reset Go's recovery time,
/// and the count is its own field. Restores never escalate, and Go's records get
/// the first bound whatever their backoff level.
#[test]
fn restored_quota_resets_are_bounded_too() {
    use crate::cooldown_store::{Quota, Record};
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let (now, wall) = (Instant::now(), SystemTime::now());
    let cap = secs(H);
    let days = secs(6 * 24 * H);
    let mut s = Scheduler::default();
    s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
    let saved = s.records(&c, now, wall);
    assert_eq!(saved[0].next_retry_after, Some(wall + secs(H)));
    assert_eq!(saved[0].quota.next_recover_at, Some(wall + days));
    assert_eq!(
        (saved[0].quota.trust_windows, saved[0].quota.backoff_level),
        (Some(1), 0)
    );
    // Our own record restores as it was: one hour, the same window.
    let mut back = Scheduler::default();
    assert!(back.restore(&c, &saved[0], true, cap, now, wall));
    assert_eq!(back.wait(&c, "m", now), Some(secs(H)));
    assert_eq!(back.records(&c, now, wall), saved);
    let record_at = |retry: Duration, trust_windows, backoff_level| Record {
        auth_id: "a".into(),
        model: "m".into(),
        next_retry_after: Some(wall + retry),
        quota: Quota {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(wall + days),
            backoff_level,
            trust_windows,
            ..Default::default()
        },
        ..Default::default()
    };
    // Go's record with backoff level 2: the first bound; the level stays Go's.
    let mut s = Scheduler::default();
    assert!(s.restore(&c, &record_at(days, None, 2), true, cap, now, wall));
    assert_eq!(s.wait(&c, "m", now), Some(secs(H)));
    assert_eq!(entry(&s, &c, "m").level, 2);
    // Our record in its third window (4 h, count 2): restored as it is, and the
    // count survives, so the next cut window is 8 h.
    let mut s = Scheduler::default();
    assert!(s.restore(&c, &record_at(secs(4 * H), Some(3), 0), true, cap, now, wall));
    assert_eq!(s.wait(&c, "m", now), Some(secs(4 * H)));
    let trust = entry(&s, &c, "m").trust;
    assert!(trust.bounded && trust.window == Some(2));
    assert_eq!(trust.stated, Some(now + days));
    let later = now + secs(4 * H);
    s.record(&c, "m", &hinted(FailureScope::Model, days - secs(4 * H)), &p, later);
    assert_eq!(s.wait(&c, "m", later), Some(secs(8 * H)));
    // A reset trusted as stated restores as it was, unbounded, keeping the count.
    let ninety = secs(90 * MIN);
    let trusted = Record {
        quota: Quota {
            next_recover_at: Some(wall + ninety),
            ..record_at(ninety, Some(1), 0).quota
        },
        ..record_at(ninety, Some(1), 0)
    };
    let mut s = Scheduler::default();
    assert!(s.restore(&c, &trusted, true, cap, now, wall));
    assert_eq!(s.wait(&c, "m", now), Some(ninety));
    let back = &s.records(&c, now, wall)[0];
    assert_eq!(
        (
            back.next_retry_after,
            back.quota.next_recover_at,
            back.quota.trust_windows
        ),
        (trusted.next_retry_after, trusted.quota.next_recover_at, Some(1))
    );
    assert!(!entry(&s, &c, "m").trust.bounded);
    // An absurd saved count is clamped, not overflowed.
    let mut s = Scheduler::default();
    assert!(s.restore(&c, &record_at(secs(H), Some(u32::MAX), 0), true, cap, now, wall));
    assert_eq!(entry(&s, &c, "m").trust.window, Some(MAX_TRUST_WINDOW));
    // Setting off: Go's six days.
    let mut s = Scheduler::default();
    assert!(s.restore(&c, &record_at(days, Some(3), 0), true, Duration::ZERO, now, wall));
    assert_eq!(s.wait(&c, "m", now), Some(days));
}

/// A longer cooldown of another cause that a credential-wide quota marked as quota
/// (model B's 12 h model-support cooldown during a six-day 429 on model A) keeps its
/// own deadline through a save and a restore: it is cliproxy-rs's record, not Go's.
#[test]
fn a_marked_sibling_restores_its_own_deadline() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let (now, wall) = (Instant::now(), SystemTime::now());
    let mut s = Scheduler::default();
    let support = ExecError::local(400, FailureScope::Model, "model is not supported");
    s.record(&c, "b", &Outcome::Failure(support), &p, now);
    assert_eq!(s.wait(&c, "b", now), Some(secs(12 * H)), "Go's model-support cooldown");
    s.record(&c, "a", &hinted(FailureScope::Credential, secs(6 * 24 * H)), &p, now);
    let records = s.records(&c, now, wall);
    let b = records.iter().find(|r| r.model == "b").unwrap();
    assert!(b.quota.exceeded && b.quota.trust_windows.is_some(), "{b:?}");
    let mut back = Scheduler::default();
    for r in &records {
        back.restore(&c, r, true, secs(H), now, wall);
    }
    assert_eq!(back.wait(&c, "b", now), Some(secs(12 * H)));
    assert!(!entry(&back, &c, "b").trust.bounded, "not cut, so not a bounded window");
}

/// A credential-wide window does not lower a model's own count: a model already in
/// its fourth window (8 h) keeps it when a credential-wide 1 h window outlasts its
/// own, so its next cut window is 16 h, not 2 h. Both the sibling and the failing
/// model's own key.
#[test]
fn credential_wide_windows_keep_the_larger_model_count() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let days = secs(6 * 24 * H);
    for failing in ["other", "m"] {
        let mut now = Instant::now();
        let mut s = Scheduler::default();
        for window in [1, 2, 4, 8] {
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
            assert_eq!(s.wait(&c, "m", now), Some(secs(window * H)));
            if window < 8 {
                now += secs(window * H);
            }
        }
        // Ten minutes before m's 8 h window ends, a credential-wide hint whose 1 h
        // window outlasts it.
        let late = now + secs(8 * H) - secs(10 * MIN);
        s.record(&c, failing, &hinted(FailureScope::Credential, days), &p, late);
        assert_eq!(entry(&s, &c, "m").trust.window, Some(3), "{failing}: count kept");
        let end = late + secs(H);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, end);
        assert_eq!(s.wait(&c, "m", end), Some(secs(16 * H)), "{failing}");
    }
}

/// Go's level climbs under Cloudflare challenges too, even inside a live window; a
/// stated reset there still gets the first bound.
#[test]
fn a_reset_inside_a_cloudflare_window_gets_the_first_bound() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let mut now = Instant::now();
    let mut s = Scheduler::default();
    for _ in 0..5 {
        let challenge = ExecError::local(403, FailureScope::Model, "cloudflare challenge");
        s.record(&c, "m", &Outcome::Failure(challenge), &p, now);
        now += secs(1);
    }
    assert!(entry(&s, &c, "m").level >= 5);
    assert!(s.wait(&c, "m", now).is_some(), "inside the challenge window");
    s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, now);
    assert_eq!(s.wait(&c, "m", now), Some(secs(H)));
}

#[test]
fn cooldown_deadlines_floor_backoff_and_sibling_success() {
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let now = Instant::now();
    let mut s = Scheduler::default();
    let fail = |status, scope, hint| {
        let mut e = ExecError::local(status, scope, "failure");
        e.retry_after = hint;
        Outcome::Failure(e)
    };
    s.record(&c, "sibling", &fail(404, FailureScope::Model, None), &p, now);
    s.record(
        &c,
        "m",
        &fail(429, FailureScope::Credential, Some(Duration::from_secs(2))),
        &p,
        now,
    );
    assert_eq!(s.wait(&c, "m", now), Some(Duration::from_secs(10)));
    assert_eq!(s.wait(&c, "new", now), Some(Duration::from_secs(10)));
    assert_eq!(
        s.wait(&c, "sibling", now),
        Some(Duration::from_secs(43200)),
        "quota must not shorten sibling"
    );
    s.record(&c, "sibling", &Outcome::Success, &p, now + Duration::from_secs(1));
    assert_eq!(
        s.wait(&c, "sibling", now),
        Some(Duration::from_secs(43200)),
        "inflight success cannot clear live quota"
    );
    assert_eq!(
        s.wait(&c, "new", now + Duration::from_secs(10)),
        None,
        "deadline boundary"
    );
    s.record(&c, "new", &Outcome::Success, &p, now + Duration::from_secs(10));
    assert!(s.wait(&c, "sibling", now + Duration::from_secs(10)).is_some());
    let mut s = Scheduler::default();
    s.record(&c, "m", &fail(429, FailureScope::Model, None), &p, now);
    assert_eq!(s.wait(&c, "m(high)", now), Some(Duration::from_secs(1)));
    s.record(&c, "m", &fail(429, FailureScope::Model, None), &p, now);
    assert_eq!(
        s.wait(&c, "m", now),
        Some(Duration::from_secs(1)),
        "active quota reused"
    );
    s.record(
        &c,
        "m",
        &fail(429, FailureScope::Model, None),
        &p,
        now + Duration::from_secs(1),
    );
    assert_eq!(
        s.wait(&c, "m", now + Duration::from_secs(1)),
        Some(Duration::from_secs(2))
    );
    assert_eq!(s.wait(&c, "other", now), None, "model failure stays local");
    for (status, seconds) in [
        (401, 1800),
        (402, 1800),
        (403, 1800),
        (404, 43200),
        (408, 60),
        (500, 60),
        (526, 60),
    ] {
        let mut s = Scheduler::default();
        s.record(&c, "m", &fail(status, FailureScope::Model, None), &p, now);
        assert_eq!(
            s.wait(&c, "m", now),
            Some(Duration::from_secs(seconds)),
            "status {status}"
        );
    }
}

/// Go `Manager.MarkResult` cooldowns: goldens from tests/reference/server/main.go,
/// replayed with upstream-shaped errors (headers present, executor scopes).
#[test]
fn cooldowns_match_go_mark_result() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
    let cases = fixture["cooldown"].as_array().unwrap();
    assert_eq!(cases.len(), 25);
    // Go trusts every stated reset.
    let policy = Policy {
        max_trusted_cooldown: Duration::ZERO,
        ..Policy::default()
    };
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let c = cred(&format!("{name}.json"), serde_json::json!({}));
        let mut s = Scheduler::default();
        let now = Instant::now();
        for step in case["steps"].as_array().unwrap() {
            let status = step["status"].as_u64().unwrap() as u16;
            let scope = if step["credential_scope"].as_bool().unwrap() {
                FailureScope::Credential
            } else {
                match status {
                    429 => FailureScope::Model,
                    401..=403 | 408 | 500.. => FailureScope::Credential,
                    _ => FailureScope::Request,
                }
            };
            let mut error = ExecError::local(status, scope, step["message"].as_str().unwrap());
            error
                .headers
                .insert("content-type", "application/json".parse().unwrap());
            let hint = step["retry_after_ms"].as_i64().unwrap();
            error.retry_after = (hint >= 0).then(|| Duration::from_millis(hint as u64));
            s.record(
                &c,
                step["model"].as_str().unwrap(),
                &Outcome::Failure(error),
                &policy,
                now,
            );
        }
        for (model, seconds) in case["seconds"].as_object().unwrap() {
            let wait = s.wait(&c, model, now).unwrap_or_default();
            assert_eq!(
                wait.as_secs_f64().round() as u64,
                seconds.as_u64().unwrap(),
                "{name} {model} wait"
            );
            let quota = case["quota"][model].as_bool().unwrap();
            // Go keeps `Quota.Exceeded` on an expired state; only a live one matters.
            assert_eq!(
                s.quota_cooling(&c, model, now),
                quota && wait > Duration::ZERO,
                "{name} {model} quota"
            );
        }
    }
}

/// Go `SessionAffinitySelector` (`Enrich`, `Pick` with fill-first, `OnResult`):
/// goldens from tests/reference/server/main.go.
#[test]
fn session_affinity_matches_go_selector() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
    let cases = fixture["affinity"].as_array().unwrap();
    assert_eq!(cases.len(), 25);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let lcp_case = case["lcp"].as_bool().unwrap_or_default();
        let creds: Vec<Credential> = ["a", "b", "c"]
            .iter()
            .map(|id| {
                let mut c = cred(id, serde_json::json!({}));
                if let Some(p) = case["priorities"][*id].as_str() {
                    c.attributes.insert("priority".into(), p.into());
                }
                c
            })
            .collect();
        let policy = Policy {
            strategy: Strategy::FillFirst,
            session_affinity: true,
            session_affinity_subagents: case["subagent_affinity"].as_bool().unwrap(),
            ..Default::default()
        };
        let mut s = Scheduler::default();
        let now = Instant::now();
        for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let mut headers = axum::http::HeaderMap::new();
            for pair in step["headers"].as_array().into_iter().flatten() {
                headers.append(
                    axum::http::HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap(),
                    pair[1].as_str().unwrap().parse().unwrap(),
                );
            }
            let payload = step["payload"].as_str().unwrap_or_default().as_bytes();
            let format = cpa_core::format::Format::parse(case["format"].as_str().unwrap_or("openai")).unwrap();
            let caller = match step["caller"].as_str() {
                Some(c) => c,
                None => case["caller"].as_str().unwrap_or_default(),
            };
            let caller = if lcp_case { caller } else { "" };
            let session = crate::session::resolve(format, &headers, payload, None, caller);
            // dispatch::run's LCP step.
            let lcp = (lcp_case && !session.explicit)
                .then(|| crate::lcp::Request::new(format.as_str(), payload, caller))
                .flatten()
                .map(std::sync::Arc::new);
            let sel = Selection {
                session: session.id,
                session_parent: session.parent,
                session_fork: session.fork,
                lcp,
                ..selection("m")
            };
            let find = |id: &str| creds.iter().find(|c| c.id == id).unwrap();
            let outcome = |op: &str| {
                if op.ends_with("ok") {
                    Outcome::Success
                } else {
                    let status = step["status"].as_u64().unwrap() as u16;
                    let scope = if status >= 500 {
                        FailureScope::Credential
                    } else {
                        FailureScope::Request
                    };
                    let mut e = ExecError::local(status, scope, step["message"].as_str().unwrap());
                    e.headers.insert("content-type", "text/plain".parse().unwrap());
                    Outcome::Failure(e)
                }
            };
            match step["op"].as_str().unwrap() {
                op @ ("pick" | "pick_ok" | "pick_fail") => {
                    let available: Vec<&Credential> = step["available"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| find(id.as_str().unwrap()))
                        .collect();
                    let (picked, bound) = s
                        .pick_session(&tag(&available), &sel, &policy, &|_| Windows::default(), now)
                        .unwrap();
                    assert_eq!(picked.id, step["picked"].as_str().unwrap(), "{name} step {i}");
                    // The metadata Go's pickLCP wrote, as the attempt reports it.
                    let got = bound.as_ref().map(|m| {
                        let (node_kind, fork, compaction) = m.node();
                        serde_json::json!({
                            "session": m.session, "parent": m.parent, "node_kind": node_kind,
                            "fork": fork, "compaction": compaction, "generation": m.access,
                        })
                    });
                    assert_eq!(got.as_ref(), step.get("lcp"), "{name} step {i} lcp");
                    if op != "pick" {
                        let picked = picked.clone();
                        s.session_result(&picked, &sel, &outcome(op), &policy, bound.as_ref(), now);
                    }
                }
                op => {
                    s.session_result(
                        find(step["auth"].as_str().unwrap()),
                        &sel,
                        &outcome(op),
                        &policy,
                        None,
                        now,
                    );
                }
            }
        }
    }
}

/// The failure of a request picked before a policy change removes nothing bound
/// after it, though the change replaced the matcher.
#[test]
fn delayed_failure_keeps_binding_made_after_policy_change() {
    let a = cred("a", serde_json::json!({}));
    let p = Policy {
        session_affinity: true,
        ..Default::default()
    };
    let next = Policy {
        session_affinity_ttl: Duration::from_secs(600),
        ..p.clone()
    };
    let now = Instant::now();
    let mut s = Scheduler::default();
    let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let request = crate::lcp::Request::new("openai", body, "client-key").unwrap();
    let sel = Selection {
        lcp: Some(std::sync::Arc::new(request.clone())),
        ..selection("m")
    };
    let ranks = |_: &Credential| Windows::default();
    let mut old = None;
    for _ in 0..5 {
        old = s.pick_session(&tag(&[&a]), &sel, &p, &ranks, now).unwrap().1;
    }
    let old = old.unwrap();
    s.configure(&p, &next);
    let fresh = s
        .pick_session(&tag(&[&a]), &sel, &next, &ranks, now)
        .unwrap()
        .1
        .unwrap();
    assert!(fresh.access > old.access, "{} after {}", fresh.access, old.access);
    let failure = Outcome::Failure(ExecError::local(500, FailureScope::Credential, "boom"));
    s.session_result(&a, &sel, &failure, &p, Some(&old), now);
    let namespace = request.namespace("m");
    let kept = s.lcp.find(&namespace, request.prepared(), now);
    assert_eq!(kept.map(|m| m.auth), Some("a".to_owned()));
}

/// After session affinity is turned off, a request picked while it was on binds
/// nothing: neither its result nor a retry under its policy.
#[test]
fn leases_from_before_disabling_affinity_bind_nothing() {
    let a = cred("a", serde_json::json!({}));
    let on = Policy {
        session_affinity: true,
        ..Default::default()
    };
    let off = Policy::default();
    let now = Instant::now();
    let mut s = Scheduler::default();
    s.configure(&off, &on);
    let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let request = crate::lcp::Request::new("openai", body, "client-key").unwrap();
    let namespace = request.namespace("m");
    let lcp = Selection {
        lcp: Some(std::sync::Arc::new(request.clone())),
        ..selection("m")
    };
    let explicit = Selection {
        session: Some("s1".into()),
        ..selection("m")
    };
    let ranks = |_: &Credential| Windows::default();
    let bound = s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1;
    assert!(bound.is_some());
    s.configure(&on, &off);
    assert!(s.lcp.find(&namespace, request.prepared(), now).is_none());
    s.session_result(&a, &lcp, &Outcome::Success, &on, bound.as_ref(), now);
    s.session_result(&a, &explicit, &Outcome::Success, &on, None, now);
    let retry = s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1;
    assert!(retry.is_none());
    s.pick_session(&tag(&[&a]), &explicit, &on, &ranks, now);
    assert!(s.lcp.find(&namespace, request.prepared(), now).is_none());
    assert!(s.affinity.get(&session_keys(&explicit).unwrap().primary, now).is_none());
    // Turning it back on binds again.
    s.configure(&off, &on);
    assert!(s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1.is_some());
}

#[test]
fn policy_change_resets_rotation_and_affinity_but_keeps_cooldowns() {
    let a = cred("a", serde_json::json!({}));
    let b = cred("b", serde_json::json!({}));
    let p = Policy {
        session_affinity: true,
        ..Default::default()
    };
    let now = Instant::now();
    let mut s = Scheduler::default();
    let mut selection = selection("m");
    selection.session = Some("session".into());
    assert_eq!(s.pick(&tag(&[&a, &b]), &selection, &p, now).unwrap().id, "a");
    s.record(
        &a,
        "other",
        &Outcome::Failure(ExecError::local(429, FailureScope::Model, "quota")),
        &p,
        now,
    );
    let mut next = p.clone();
    next.request_retry = 2;
    s.configure(&p, &next);
    assert!(!s.rotations.is_empty(), "retry-only change preserves selector state");
    assert!(!s.affinity.is_empty());
    next.session_affinity_ttl = Duration::from_secs(5);
    s.configure(&p, &next);
    assert!(s.rotations.is_empty());
    assert!(s.affinity.is_empty());
    assert_eq!(s.wait(&a, "other", now), Some(Duration::from_secs(1)));
}

#[test]
fn forced_cooling_uses_fallback_and_quota_does_not_reuse_transient_state() {
    let now = Instant::now();
    // Go: a forced cooldown re-enables the normal policy despite disable_cooling
    // (conductor_cooldown.go MarkResult), so the status decides the duration.
    for (status, seconds) in [(401, 1800), (404, 43200), (429, 1), (503, 60)] {
        let c = cred(
            "a",
            serde_json::json!({"disable_cooling":true,
            "request_scoped_errors":[{"status":status,"match":["forced"],"action":"stop-and-cooldown"}]}),
        );
        let mut s = Scheduler::default();
        s.record(
            &c,
            "m",
            &Outcome::Failure(ExecError::local(status, FailureScope::Credential, "forced")),
            &Policy::default(),
            now,
        );
        assert_eq!(
            s.wait(&c, "m", now),
            Some(Duration::from_secs(seconds)),
            "status {status}"
        );
    }
    // With transient cooldowns disabled, the forced cooldown falls back to one minute.
    let c = cred(
        "a",
        serde_json::json!({"request_scoped_errors":[{"status":503,"match":["forced"],"action":"continue-and-cooldown"}]}),
    );
    let no_transient = Policy {
        transient_error_cooldown_seconds: -1,
        ..Policy::default()
    };
    let mut s = Scheduler::default();
    let forced = Outcome::Failure(ExecError::local(503, FailureScope::Credential, "forced"));
    s.record(&c, "m", &forced, &no_transient, now);
    assert_eq!(s.wait(&c, "m", now), Some(Duration::from_secs(60)));
    let plain = Outcome::Failure(ExecError::local(503, FailureScope::Credential, "plain"));
    let mut s = Scheduler::default();
    s.record(&c, "m", &plain, &no_transient, now);
    assert_eq!(
        s.wait(&c, "m", now),
        None,
        "negative transient seconds disable 503 cooldowns"
    );
    let c = cred("a", serde_json::json!({}));
    let p = Policy::default();
    let mut s = Scheduler::default();
    s.record(
        &c,
        "m",
        &Outcome::Failure(ExecError::local(503, FailureScope::Model, "transient")),
        &p,
        now,
    );
    s.record(
        &c,
        "m",
        &Outcome::Failure(ExecError::local(429, FailureScope::Model, "quota")),
        &p,
        now,
    );
    assert!(
        s.quota_cooling(&c, "m", now),
        "quota must replace transient classification"
    );
    assert_eq!(
        s.wait(&c, "m", now),
        Some(Duration::from_secs(60)),
        "live deadline never shortened"
    );
    let mut hint = ExecError::local(418, FailureScope::Model, "other status");
    hint.retry_after = Some(Duration::from_secs(900));
    s.record(&c, "other", &Outcome::Failure(hint), &p, now);
    assert_eq!(
        s.wait(&c, "other", now),
        Some(Duration::from_secs(60)),
        "unlisted statuses ignore hint"
    );
}

/// Go matches rules against a local missing-thread error's structured body, which
/// names `not_found_error`; its message does not.
#[test]
fn rules_match_the_structured_body_of_a_missing_thread() {
    let c = cred(
        "a",
        serde_json::json!({"request_scoped_errors":[{"status":404,"match":["not_found_error"],"action":"continue"}]}),
    );
    let e = ExecError::local(404, FailureScope::Request, cpa_exec::claude::THREAD_NOT_FOUND);
    let action = Policy::default().error_action(&c, &e);
    assert!(action.matched && !action.stop);
    // Any other 404 is matched by its text, including an upstream answer with the
    // same words.
    let other = ExecError::local(404, FailureScope::Request, "not found");
    assert!(!Policy::default().error_action(&c, &other).matched);
    let mut upstream = e.clone();
    upstream
        .headers
        .insert(axum::http::header::CONTENT_TYPE, "text/plain".parse().unwrap());
    assert!(!Policy::default().error_action(&c, &upstream).matched);
}

#[test]
fn config_rules_are_parsed_once_and_unusable_parts_are_reported() {
    let cfg = cpa_core::config::Config::parse(
        "oauth:\n  request-scoped-errors:\n    Claude:\n      - {status: 429, match-regexr: ['[', 'quota [0-9]+'], action: ' Continue '}\n      - {status: 400, match: [x], action: explode}\n",
    )
    .unwrap();
    let (rules, problems) = parse_oauth_rules(&cfg);
    let claude = &rules["claude"];
    assert_eq!(claude.len(), 1);
    assert_eq!((claude[0].action, claude[0].regex.len()), (RuleAction::Continue, 1));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems[0].starts_with("claude: status 429: match-regexr \"[\" is invalid"),
        "{problems:?}"
    );
    assert_eq!(problems[1], "claude: status 400: unknown action \"explode\"");
    let action = Policy::from_config(&cfg).error_action(
        &cred("a", serde_json::json!({})),
        &ExecError::local(429, FailureScope::Request, "quota 12"),
    );
    assert!(action.matched && !action.stop && !action.cooldown);
}

#[test]
fn rules_are_ordered_status_and_body_matches_with_canonical_precedence() {
    let c = cred(
        "a",
        serde_json::json!({"disable_cooling":true, "request_scoped_errors":[
        {"status":429,"match":[],"action":"stop"},
        {"status":500,"match":["quota"],"action":"stop"},
        {"status":429,"match-regexr":["[", "quota [0-9]+"],"action":" CONTINUE-AND-COOLDOWN "},
        {"status":429,"match":["quota"],"action":"stop"}]}),
    );
    let p = Policy::default();
    let e = ExecError::local(429, FailureScope::Request, "quota 12");
    let action = p.error_action(&c, &e);
    assert!(!action.stop);
    assert!(action.cooldown && action.force_cooldown);
    let now = Instant::now();
    let mut s = Scheduler::default();
    s.record(&c, "m", &Outcome::Failure(e), &p, now);
    assert!(
        s.wait(&c, "m", now).is_some(),
        "force cooling bypasses disabled cooling"
    );
    let c = cred(
        "a",
        serde_json::json!({"request_retry":0, "request-retry":5,
        "disable_cooling":false,"disable-cooling":true}),
    );
    assert_eq!(p.retry_limit(&c), 0);
    assert!(!p.cooling_disabled(&c));
    let legacy = cred(
        "legacy",
        serde_json::json!({"request_retry":null,"request-retry":2,
        "disable_cooling":"TrUe","disable-cooling":1}),
    );
    assert_eq!(
        p.retry_limit(&legacy),
        2,
        "invalid canonical override permits legacy fallback"
    );
    assert!(
        p.cooling_disabled(&legacy),
        "Go ParseBool rejects mixed case; numeric legacy accepted"
    );
    assert!(!p.cooling_disabled(&cred("numeric", serde_json::json!({"disable_cooling":0}))));
    assert_eq!(
        Policy {
            request_retry: 3,
            ..Default::default()
        }
        .retry_limit(&cred("b", serde_json::json!({"request_retry":"-1"}))),
        3
    );
    // Go `providerCoolingOverrideForAuth`: the first enabled compat entry named by
    // compat_name, provider_key or provider; credential metadata still wins.
    let cfg = cpa_core::config::Config::parse(
        "openai-compatibility:\n  - {name: skipped, base-url: '', disable-cooling: false}\n  - {name: Router, base-url: http://r.invalid, disable-cooling: true}\n  - {name: plain, base-url: http://p.invalid}\n",
    )
    .unwrap();
    let mut p = Policy {
        compat_disable_cooling: compat_cooling(&cfg),
        ..Default::default()
    };
    assert_eq!(
        p.compat_disable_cooling,
        [("Router".to_owned(), Some(true)), ("plain".to_owned(), None)],
        "legacy openai-compatibility is read through the v8 document"
    );
    let compat = |attrs: &[(&str, &str)], meta: Value| {
        let mut c = cred("compat", meta);
        c.provider = "openai-compatibility".into();
        for (k, v) in attrs {
            c.attributes.insert((*k).into(), (*v).into());
        }
        c
    };
    assert!(p.cooling_disabled(&compat(&[("compat_name", "router")], serde_json::json!({}))));
    assert!(p.cooling_disabled(&compat(&[("provider_key", "ROUTER")], serde_json::json!({}))));
    assert!(!p.cooling_disabled(&compat(
        &[("compat_name", "router")],
        serde_json::json!({"disable_cooling": false})
    )));
    p.disable_cooling = true;
    assert!(
        p.cooling_disabled(&compat(&[("compat_name", "plain")], serde_json::json!({}))),
        "an entry without disable-cooling defers to global"
    );
    p.disable_cooling = false;
    assert!(
        !p.cooling_disabled(&cred("claude-file", serde_json::json!({}))),
        "credentials outside OpenAI compatibility ignore entries"
    );
    for scope in [FailureScope::Request, FailureScope::Transport] {
        let mut s = Scheduler::default();
        s.record(
            &c,
            "m",
            &Outcome::Failure(ExecError::local(502, scope, "fault")),
            &p,
            now,
        );
        assert_eq!(s.wait(&c, "m", now), None);
    }
}

// ---- soonest-reset (cliproxy-rs addition) ----

const DAY: u64 = 24 * 3600;

fn epoch(t: SystemTime) -> String {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs().to_string()
}

/// Claude's measured headers observed at `now`: the 7-day reset `weekly` seconds
/// later and the 5-hour window's status and reset.
fn claude(now: SystemTime, weekly: u64, five_hour: &str, five_hour_reset: SystemTime) -> Windows {
    claude_at(now, now, weekly, five_hour, five_hour_reset)
}

/// [`claude`] observed at `observed` and read at `now`.
fn claude_at(
    observed: SystemTime,
    now: SystemTime,
    weekly: u64,
    five_hour: &str,
    five_hour_reset: SystemTime,
) -> Windows {
    let signals: std::collections::BTreeMap<String, String> = [
        ("Anthropic-Ratelimit-Unified-7d-Status", "allowed".to_owned()),
        ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.40".to_owned()),
        (
            "Anthropic-Ratelimit-Unified-7d-Reset",
            epoch(observed + Duration::from_secs(weekly)),
        ),
        ("Anthropic-Ratelimit-Unified-5h-Status", five_hour.to_owned()),
        ("Anthropic-Ratelimit-Unified-5h-Reset", epoch(five_hour_reset)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    Windows::observed("claude", &signals, observed, now)
}

fn soonest() -> Policy {
    Policy {
        strategy: Strategy::SoonestReset,
        ..Default::default()
    }
}

/// The asymmetric case: `a` resets in four days and `b` in one, so fill-first (id
/// order) would take `a` and round-robin would alternate.
#[test]
fn soonest_reset_spends_the_earliest_weekly_window_first() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let in_an_hour = wall + Duration::from_secs(3600);
    // Both observed at `wall`, read at `now`; `b`'s 5-hour window in `b_five_hour`.
    let ranks_at = |b_five_hour: &'static str, now: SystemTime| {
        let (wa, wb) = (
            claude_at(wall, now, 4 * DAY, "allowed", in_an_hour),
            claude_at(wall, now, DAY, b_five_hour, in_an_hour),
        );
        move |cred: &Credential| if cred.id == "a" { wa } else { wb }
    };
    let policy = soonest();
    let mut s = Scheduler::default();
    let now = Instant::now();
    let ranks = ranks_at("allowed", wall);
    for _ in 0..3 {
        let picked = s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &ranks, now);
        assert_eq!(picked.unwrap().id, "b", "the sooner weekly reset is used up first");
    }
    // `b` cools on a 429: the next soonest takes over.
    s.record(
        &b,
        "m",
        &Outcome::Failure(ExecError::local(429, FailureScope::Credential, "quota")),
        &policy,
        now,
    );
    let ready: Vec<&Credential> = [&a, &b].into_iter().filter(|x| s.wait(x, "m", now).is_none()).collect();
    assert_eq!(ready.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["a"]);
    assert_eq!(
        s.pick_ranked(&tag(&ready), &selection("m"), &policy, &ranks, now)
            .unwrap()
            .id,
        "a"
    );
    // Without the cooldown, `b`'s used-up 5-hour window also hands over to `a`...
    let mut s = Scheduler::default();
    let exhausted = ranks_at("rejected", wall);
    for _ in 0..2 {
        assert_eq!(
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &exhausted, now)
                .unwrap()
                .id,
            "a"
        );
    }
    // ...until that window resets, an hour later.
    let reset = ranks_at("rejected", in_an_hour + Duration::from_secs(1));
    assert_eq!(
        s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &reset, now)
            .unwrap()
            .id,
        "b"
    );
}

/// Accounts with no known reset get one probe request each, then rank by what they
/// reported; one that reported nothing comes after the known resets.
#[test]
fn soonest_reset_probes_unknown_accounts_once_then_ranks_them() {
    let wall = SystemTime::now();
    let creds: Vec<Credential> = ["a", "b", "c", "d"]
        .iter()
        .map(|id| cred(id, serde_json::json!({})))
        .collect();
    let [a, b, c, d] = [&creds[0], &creds[1], &creds[2], &creds[3]];
    let in_an_hour = wall + Duration::from_secs(3600);
    // `d` resets in 3 days, `c` is used up, `a` and `b` were never observed.
    let known = |x: &Credential| match x.id.as_str() {
        "c" => claude(wall, DAY, "rejected", in_an_hour),
        "d" => claude(wall, 3 * DAY, "allowed", in_an_hour),
        _ => Windows::default(),
    };
    let policy = soonest();
    let mut s = Scheduler::default();
    let now = Instant::now();
    let pick = |s: &mut Scheduler, set: &[&Credential], ranks: &Ranks<'_>| {
        s.pick_ranked(&tag(set), &selection("m"), &policy, ranks, now)
            .unwrap()
            .id
            .clone()
    };
    // One probe each for the unknown accounts, then the known reset.
    let order: Vec<String> = (0..4).map(|_| pick(&mut s, &[a, b, c, d], &known)).collect();
    assert_eq!(order, ["a", "b", "d", "d"]);
    // `a`'s probe answered with a reset in 12 hours: it now ranks by it, ahead of
    // `d`; `b`'s answer reported nothing, so it stays behind the known resets.
    let learned = |x: &Credential| match x.id.as_str() {
        "a" => claude(wall + Duration::from_secs(1), 12 * 3600, "allowed", in_an_hour),
        _ => known(x),
    };
    assert_eq!(pick(&mut s, &[a, b, c, d], &learned), "a");
    assert_eq!(pick(&mut s, &[b, c, d], &learned), "d");
    assert_eq!(
        pick(&mut s, &[b, c], &learned),
        "b",
        "probed and unknown, still before used-up"
    );
    assert_eq!(
        pick(&mut s, &[c], &learned),
        "c",
        "a used-up account is the last resort"
    );
    // When `d`'s weekly reset passes, it is unknown again and gets a new probe.
    let passed = |x: &Credential| match x.id.as_str() {
        "d" => claude_at(
            wall,
            wall + Duration::from_secs(3 * DAY + 1),
            3 * DAY,
            "allowed",
            in_an_hour,
        ),
        _ => learned(x),
    };
    assert_eq!(pick(&mut s, &[a, b, d], &passed), "d");
    assert_eq!(pick(&mut s, &[a, b, d], &passed), "a");
}

#[test]
fn soonest_reset_rotates_equal_ranks() {
    let wall = SystemTime::now();
    let creds: Vec<Credential> = ["a", "b"].iter().map(|id| cred(id, serde_json::json!({}))).collect();
    let in_an_hour = wall + Duration::from_secs(3600);
    // The same reset for both.
    let ranks = |_: &Credential| claude(wall, 2 * DAY, "allowed", in_an_hour);
    let mut s = Scheduler::default();
    let now = Instant::now();
    let turns: Vec<String> = (0..4)
        .map(|_| {
            s.pick_ranked(&tag(&[&creds[0], &creds[1]]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone()
        })
        .collect();
    assert_eq!(turns, ["a", "b", "a", "b"]);
}

#[test]
fn soonest_reset_never_moves_a_session_pin() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let in_an_hour = wall + Duration::from_secs(3600);
    let ranks = |x: &Credential| claude(wall, if x.id == "a" { 4 * DAY } else { DAY }, "allowed", in_an_hour);
    let policy = Policy {
        session_affinity: true,
        ..soonest()
    };
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut pinned = selection("m");
    pinned.session = Some("claude:thread-1".into());
    // The thread starts while only `a` is available, and keeps `a` afterwards.
    assert_eq!(
        s.pick_ranked(&tag(&[&a]), &pinned, &policy, &ranks, now).unwrap().id,
        "a"
    );
    for _ in 0..2 {
        assert_eq!(
            s.pick_ranked(&tag(&[&a, &b]), &pinned, &policy, &ranks, now)
                .unwrap()
                .id,
            "a"
        );
    }
    // A new thread starts on the soonest reset.
    let mut other = selection("m");
    other.session = Some("claude:thread-2".into());
    assert_eq!(
        s.pick_ranked(&tag(&[&a, &b]), &other, &policy, &ranks, now).unwrap().id,
        "b"
    );
}

#[test]
fn default_strategy_stays_round_robin_and_ignores_resets() {
    assert_eq!(Policy::default().strategy, Strategy::RoundRobin);
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let ranks = |x: &Credential| claude(wall, if x.id == "a" { 4 * DAY } else { DAY }, "allowed", wall);
    let mut s = Scheduler::default();
    let now = Instant::now();
    let turns: Vec<String> = (0..4)
        .map(|_| {
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &Policy::default(), &ranks, now)
                .unwrap()
                .id
                .clone()
        })
        .collect();
    assert_eq!(turns, ["a", "b", "a", "b"]);
}

#[test]
fn soonest_reset_in_mixed_providers_stays_in_the_first_group() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let x = cred("x", serde_json::json!({}));
    let in_an_hour = wall + Duration::from_secs(3600);
    // The second provider's `x` resets soonest, but fill-first order picks the group.
    let ranks = |c: &Credential| {
        let days = match c.id.as_str() {
            "a" => 4,
            "b" => 2,
            _ => 1,
        };
        claude(wall, days * DAY, "allowed", in_an_hour)
    };
    let sel = Selection {
        providers: vec!["claude".into(), "codex".into()],
        model: "m".into(),
        ..Default::default()
    };
    let candidates = [(&a, "claude"), (&b, "claude"), (&x, "codex")];
    let mut s = Scheduler::default();
    let picked = s.pick_ranked(&candidates, &sel, &soonest(), &ranks, Instant::now());
    assert_eq!(picked.unwrap().id, "b");
}

#[test]
fn windows_read_claude_and_codex_signals() {
    let now = SystemTime::now();
    let map = |pairs: &[(&str, String)]| -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    };
    // A weekly reset already behind is unknown, not "soonest".
    let stale = map(&[(
        "Anthropic-Ratelimit-Unified-7d-Reset",
        epoch(now - Duration::from_secs(60)),
    )]);
    let w = Windows::observed("claude", &stale, now, now);
    assert_eq!(
        (
            w.weekly_reset,
            w.rolled_over.map(|r| (r.window, epoch(r.at))),
            w.exhausted
        ),
        (
            None,
            Some((WindowId::Claude7d, epoch(now - Duration::from_secs(60)))),
            false
        )
    );
    // Utilization 1.0 exhausts the 5-hour window until its reset.
    let full = map(&[
        ("Anthropic-Ratelimit-Unified-5h-Utilization", "1.0".into()),
        (
            "Anthropic-Ratelimit-Unified-5h-Reset",
            epoch(now + Duration::from_secs(600)),
        ),
    ]);
    assert!(Windows::observed("claude", &full, now, now).exhausted);
    assert!(!Windows::observed("claude", &full, now, now + Duration::from_secs(601)).exhausted);
    // Codex: the longer window is weekly whichever header carries it; reset-after
    // counts from the observation.
    let observed = now - Duration::from_secs(100);
    let codex = map(&[
        ("X-Codex-Primary-Used-Percent", "100".into()),
        ("X-Codex-Primary-Window-Minutes", "300".into()),
        ("X-Codex-Primary-Reset-After-Seconds", "3600".into()),
        ("X-Codex-Secondary-Used-Percent", "40".into()),
        ("X-Codex-Secondary-Window-Minutes", "10080".into()),
        ("X-Codex-Secondary-Reset-At", epoch(now + Duration::from_secs(2 * DAY))),
    ]);
    let w = Windows::observed("codex", &codex, observed, now);
    assert!(w.exhausted, "the 5-hour primary is used up");
    assert_eq!(
        w.weekly_reset.map(epoch),
        Some(epoch(now + Duration::from_secs(2 * DAY)))
    );
    assert!(!Windows::observed("codex", &codex, observed, observed + Duration::from_secs(3601)).exhausted);
    let weekly_primary = map(&[
        ("X-Codex-Primary-Used-Percent", "51".into()),
        ("X-Codex-Primary-Window-Minutes", "10080".into()),
        ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(3 * DAY))),
    ]);
    let w = Windows::observed("codex", &weekly_primary, now, now);
    assert_eq!(
        (w.weekly_reset.map(epoch), w.exhausted),
        (Some(epoch(now + Duration::from_secs(3 * DAY))), false)
    );
    let reached = map(&[("X-Codex-Limit-Reached", "True".into())]);
    assert!(Windows::observed("codex", &reached, now, now).exhausted);
    // Another provider's headers mean nothing here.
    let w = Windows::observed("gemini", &codex, now, now);
    assert_eq!((w.weekly_reset, w.exhausted), (None, false));
}

// ---- soonest-reset review fixes ----

fn signals(pairs: &[(&str, String)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

/// P1: an account answering with quota signals but no weekly reset is probed once;
/// each new answer (a newer observation) must not re-arm the probe, or it would take
/// every request ahead of an account with a known reset.
#[test]
fn soonest_reset_answers_without_a_reset_do_not_re_arm_the_probe() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let policy = soonest();
    let mut s = Scheduler::default();
    let now = Instant::now();
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    let mut picks = Vec::new();
    for i in 0..5u64 {
        // `a` has answered `i` times, each time with a fresh 5-hour status only.
        let a_seen = (i > 0).then(|| {
            let seen = wall + Duration::from_secs(i);
            Windows::observed(
                "claude",
                &signals(&[("Anthropic-Ratelimit-Unified-5h-Status", "allowed".into())]),
                seen,
                seen,
            )
        });
        let ranks = |x: &Credential| {
            if x.id == "a" {
                a_seen.unwrap_or_default()
            } else {
                b_known
            }
        };
        picks.push(
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &ranks, now)
                .unwrap()
                .id
                .clone(),
        );
    }
    assert_eq!(picks, ["a", "b", "b", "b", "b"]);
}

/// The probe is reserved when picked: one that fails before any header arrives (no
/// new observation) is spent, so the next picks go to the known reset.
#[test]
fn soonest_reset_a_probe_that_fails_before_headers_is_spent() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    let ranks = |x: &Credential| if x.id == "a" { Windows::default() } else { b_known };
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut pick = || {
        s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
            .unwrap()
            .id
            .clone()
    };
    assert_eq!(pick(), "a", "the probe");
    assert_eq!([pick(), pick()], ["b", "b"]);
}

/// A known reset that passes rolls the window over and re-arms exactly one probe.
#[test]
fn soonest_reset_re_arms_only_on_a_rollover() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    let a_reset = wall + Duration::from_secs(60);
    // `a` reported a weekly reset in 60 s; read 61 s later it has rolled over.
    let a_rolled = |seen: SystemTime| {
        Windows::observed(
            "claude",
            &signals(&[("Anthropic-Ratelimit-Unified-7d-Reset", epoch(a_reset))]),
            seen,
            wall + Duration::from_secs(61),
        )
    };
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut picks = Vec::new();
    for i in 0..3u64 {
        let a_now = a_rolled(wall + Duration::from_secs(i));
        let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
        picks.push(
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone(),
        );
    }
    assert_eq!(picks, ["a", "b", "b"], "one probe per rollover");
    // The next weekly rollover re-arms it once more.
    let next = a_reset + Duration::from_secs(7 * DAY);
    let later = Windows::observed(
        "claude",
        &signals(&[("Anthropic-Ratelimit-Unified-7d-Reset", epoch(next))]),
        wall,
        next + Duration::from_secs(1),
    );
    let ranks = |x: &Credential| if x.id == "a" { later } else { b_known };
    let mut pick = || {
        s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
            .unwrap()
            .id
            .clone()
    };
    assert_eq!([pick(), pick()], ["a", "b"]);
}

/// Codex answers with `Reset-After-Seconds: 0` and no `Reset-At`: the reset is the
/// observation time itself, which must not count as a new rollover on every answer.
#[test]
fn soonest_reset_zero_relative_resets_do_not_re_arm_the_probe() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    let zero = signals(&[
        ("X-Codex-Secondary-Used-Percent", "0".into()),
        ("X-Codex-Secondary-Window-Minutes", "10080".into()),
        ("X-Codex-Secondary-Reset-After-Seconds", "0".into()),
    ]);
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut picks = Vec::new();
    for i in 0..4u64 {
        // `a` has answered `i` times, each identical answer observed a second later
        // and read a second after that.
        let a_now = (i > 0).then(|| {
            let seen = wall + Duration::from_secs(2 * i);
            Windows::observed("codex", &zero, seen, seen + Duration::from_secs(1))
        });
        let ranks = |x: &Credential| {
            if x.id == "a" {
                a_now.unwrap_or_default()
            } else {
                b_known
            }
        };
        picks.push(
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone(),
        );
    }
    assert_eq!(picks, ["a", "b", "b", "b"]);
    // The zero delay still ends a used-up window now.
    let used_up = signals(&[
        ("X-Codex-Primary-Used-Percent", "100".into()),
        ("X-Codex-Primary-Reset-After-Seconds", "0".into()),
    ]);
    assert!(!Windows::observed("codex", &used_up, wall, wall).exhausted);
}

/// Relative resets read around a boundary differ by rounding; rollovers less than
/// half a window apart are the same one, so they never re-arm the probe twice.
#[test]
fn soonest_reset_rollovers_within_half_a_window_are_one_rollover() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    // `a` reports its weekly reset `after` seconds from an observation at `seen`.
    let codex = |seen: SystemTime, after: u64, read: SystemTime| {
        Windows::observed(
            "codex",
            &signals(&[
                ("X-Codex-Secondary-Window-Minutes", "10080".into()),
                ("X-Codex-Secondary-Reset-After-Seconds", after.to_string()),
            ]),
            seen,
            read,
        )
    };
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut pick = |a_now: Windows| {
        let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
        s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
            .unwrap()
            .id
            .clone()
    };
    let boundary = wall + Duration::from_secs(600);
    // Reset reached: probed once.
    assert_eq!(pick(codex(wall, 600, boundary + Duration::from_secs(1))), "a");
    // Lagging answers keep reporting one second left, each from a later observation.
    for i in 1..4u64 {
        let seen = boundary + Duration::from_secs(i);
        assert_eq!(pick(codex(seen, 1, seen + Duration::from_secs(2))), "b", "answer {i}");
    }
    // A week later the window really rolls over again: one more probe.
    let next = boundary + Duration::from_secs(7 * DAY);
    assert_eq!(
        pick(codex(next - Duration::from_secs(5), 5, next + Duration::from_secs(1))),
        "a"
    );
    assert_eq!(
        pick(codex(next - Duration::from_secs(5), 5, next + Duration::from_secs(1))),
        "b"
    );
}

/// The oracle's sequence: a probe made for one window's rollover must not suppress a
/// later rollover of another window.
#[test]
fn soonest_reset_probe_markers_are_kept_per_window() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 5 * DAY, "allowed", wall + Duration::from_secs(3600));
    let r = wall + Duration::from_secs(600);
    let after = |t: SystemTime, secs: u64| t + Duration::from_secs(secs);
    // 1. Only `Secondary-Reset-At: R` and no lengths: secondary is taken as weekly.
    let partial = signals(&[("X-Codex-Secondary-Reset-At", epoch(r))]);
    // 2. The probe's answer is complete: the weekly window is primary, resetting at
    //    R + 1 day; secondary is the 5-hour window.
    let complete = signals(&[
        ("X-Codex-Primary-Used-Percent", "10".into()),
        ("X-Codex-Primary-Window-Minutes", "10080".into()),
        ("X-Codex-Primary-Reset-At", epoch(after(r, DAY))),
        ("X-Codex-Secondary-Used-Percent", "10".into()),
        ("X-Codex-Secondary-Window-Minutes", "300".into()),
        ("X-Codex-Secondary-Reset-At", epoch(after(r, 5 * 3600))),
    ]);
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut pick = |a_now: Windows| {
        let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
        s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
            .unwrap()
            .id
            .clone()
    };
    // Just after R, `a`'s secondary rolled over: its probe.
    let w = Windows::observed("codex", &partial, wall, after(r, 1));
    assert_eq!(w.rolled_over.map(|r| r.window), Some(WindowId::CodexSecondary));
    assert_eq!(pick(w), "a");
    // The answer reports primary resetting in a day, sooner than `b`: `a` keeps it.
    assert_eq!(
        pick(Windows::observed("codex", &complete, after(r, 1), after(r, 2))),
        "a"
    );
    // 3. Just after R + 1 day, primary rolls over: one probe, not suppressed by the
    //    secondary's marker R (R + 1 day is within half of primary's week of R).
    let w = Windows::observed("codex", &complete, after(r, 1), after(r, DAY + 1));
    assert_eq!(w.rolled_over.map(|r| r.window), Some(WindowId::CodexPrimary));
    assert_eq!(pick(w), "a");
    assert_eq!(pick(w), "b", "probed for that rollover");
}

/// P2: a management reset clears the probe record (the account is probed again) and
/// leaves session affinity alone.
#[test]
fn soonest_reset_management_reset_re_arms_the_probe_and_keeps_affinity() {
    let wall = SystemTime::now();
    let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
    let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
    let ranks = |x: &Credential| if x.id == "a" { Windows::default() } else { b_known };
    let policy = Policy {
        session_affinity: true,
        ..soonest()
    };
    let mut s = Scheduler::default();
    let now = Instant::now();
    let mut thread = selection("m");
    thread.session = Some("claude:thread-1".into());
    let mut pick =
        |sel: &Selection, set: &[&Credential]| s.pick_ranked(&tag(set), sel, &policy, &ranks, now).unwrap().id.clone();
    // The thread starts on `a` while it is the only account: that was `a`'s probe.
    assert_eq!(pick(&thread, &[&a]), "a");
    let fresh = selection("m");
    assert_eq!(pick(&fresh, &[&a, &b]), "b");
    s.reset_cooldowns("a");
    let mut pick =
        |sel: &Selection, set: &[&Credential]| s.pick_ranked(&tag(set), sel, &policy, &ranks, now).unwrap().id.clone();
    assert_eq!(pick(&fresh, &[&a, &b]), "a", "probed again");
    assert_eq!(pick(&fresh, &[&a, &b]), "b");
    assert_eq!(pick(&thread, &[&a, &b]), "a", "the pin is kept");
}

/// P2: an oversized finite window length neither panics nor holds a used-up window
/// past the missing-length policy (five hours).
#[test]
fn windows_survive_unrepresentable_window_lengths() {
    let now = SystemTime::now();
    for (used, exhausted) in [("0", false), ("100", true)] {
        let huge = signals(&[
            ("X-Codex-Primary-Used-Percent", used.into()),
            ("X-Codex-Primary-Window-Minutes", "1e308".into()),
        ]);
        assert_eq!(
            Windows::observed("codex", &huge, now, now).exhausted,
            exhausted,
            "{used}%"
        );
        let later = now + Duration::from_secs(5 * 3600 + 1);
        assert!(
            !Windows::observed("codex", &huge, now, later).exhausted,
            "{used}% after five hours"
        );
    }
    // Long but representable lengths still count.
    let long = signals(&[
        ("X-Codex-Primary-Used-Percent", "100".into()),
        ("X-Codex-Primary-Window-Minutes", "1e9".into()),
    ]);
    assert!(Windows::observed("codex", &long, now, now + Duration::from_secs(DAY)).exhausted);
}

/// P2: `X-Codex-Limit-Reached` ends with the used-up window's own reset; the
/// five-hour fallback applies only when no window reports a reset.
#[test]
fn codex_limit_reached_expires_with_the_window_reset() {
    let now = SystemTime::now();
    let flagged = signals(&[
        ("X-Codex-Limit-Reached", "true".into()),
        ("X-Codex-Primary-Used-Percent", "100".into()),
        ("X-Codex-Primary-Window-Minutes", "300".into()),
        ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(60))),
    ]);
    assert!(Windows::observed("codex", &flagged, now, now).exhausted);
    assert!(!Windows::observed("codex", &flagged, now, now + Duration::from_secs(61)).exhausted);
    // No window used up: the flag holds until the soonest reported reset.
    let below = signals(&[
        ("X-Codex-Limit-Reached", "true".into()),
        ("X-Codex-Primary-Used-Percent", "40".into()),
        ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(600))),
    ]);
    assert!(Windows::observed("codex", &below, now, now + Duration::from_secs(599)).exhausted);
    assert!(!Windows::observed("codex", &below, now, now + Duration::from_secs(601)).exhausted);
    // No reset anywhere: five hours.
    let bare = signals(&[("X-Codex-Limit-Reached", "true".into())]);
    assert!(Windows::observed("codex", &bare, now, now + Duration::from_secs(5 * 3600 - 1)).exhausted);
    assert!(!Windows::observed("codex", &bare, now, now + Duration::from_secs(5 * 3600 + 1)).exhausted);
}

/// P2: Codex usage, reset and length are parsed independently, so a window whose
/// usage is missing still reports its reset.
#[test]
fn codex_windows_parse_usage_reset_and_length_independently() {
    let now = SystemTime::now();
    let weekly = now + Duration::from_secs(3 * DAY);
    let no_usage = signals(&[
        ("X-Codex-Secondary-Window-Minutes", "10080".into()),
        ("X-Codex-Secondary-Reset-At", epoch(weekly)),
    ]);
    let w = Windows::observed("codex", &no_usage, now, now);
    assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), false));
    // Reset only, no usage or length: secondary is weekly.
    let reset_only = signals(&[("X-Codex-Secondary-Reset-After-Seconds", "7200".into())]);
    let w = Windows::observed("codex", &reset_only, now, now);
    assert_eq!(w.weekly_reset.map(epoch), Some(epoch(now + Duration::from_secs(7200))));
}

/// Both Codex windows, the primary the longer one: the primary is weekly.
#[test]
fn codex_primary_longer_than_secondary_is_the_weekly_window() {
    let now = SystemTime::now();
    let weekly = now + Duration::from_secs(3 * DAY);
    let both = signals(&[
        ("X-Codex-Primary-Used-Percent", "51".into()),
        ("X-Codex-Primary-Window-Minutes", "10080".into()),
        ("X-Codex-Primary-Reset-At", epoch(weekly)),
        ("X-Codex-Secondary-Used-Percent", "100".into()),
        ("X-Codex-Secondary-Window-Minutes", "300".into()),
        ("X-Codex-Secondary-Reset-After-Seconds", "3600".into()),
    ]);
    let w = Windows::observed("codex", &both, now, now);
    assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), true));
    let w = Windows::observed("codex", &both, now, now + Duration::from_secs(3601));
    assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), false));
}
