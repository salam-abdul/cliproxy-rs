//! Background refresh backoff. Instants are supplied so tests never sleep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;

struct Entry {
    revision: u64,
    next: Instant,
    invalid_grants: u32,
}

#[derive(Default)]
pub(crate) struct RefreshState {
    entries: HashMap<String, Entry>,
}

impl RefreshState {
    pub fn reserve(&mut self, c: &Credential, now: Instant) -> bool {
        let entry = self.entries.entry(c.id.clone()).or_insert(Entry {
            revision: c.revision,
            next: now,
            invalid_grants: 0,
        });
        if entry.revision != c.revision {
            *entry = Entry {
                revision: c.revision,
                next: now,
                invalid_grants: 0,
            };
        }
        if c.disabled || entry.next > now {
            return false;
        }
        entry.next = now + Duration::from_secs(60);
        true
    }

    /// Records the outcome and returns how long the credential now waits before another try.
    pub fn finish(&mut self, c: &Credential, error: Option<&ExecError>, ineffective: bool, now: Instant) -> Duration {
        let Some(entry) = self.entries.get_mut(&c.id) else {
            return Duration::ZERO;
        };
        entry.revision = c.revision;
        let seconds = if let Some(error) = error {
            if invalid_grant(error) {
                let seconds = (60u64 << entry.invalid_grants.min(5)).min(1800);
                entry.invalid_grants += u32::from(seconds < 1800);
                seconds
            } else {
                entry.invalid_grants = 0;
                300
            }
        } else {
            entry.invalid_grants = 0;
            if ineffective { 30 } else { 0 }
        };
        entry.next = now + Duration::from_secs(seconds);
        Duration::from_secs(seconds)
    }

    /// When a reserved or backed-off credential may be tried again.
    pub fn retry_at(&self, c: &Credential) -> Option<Instant> {
        self.entries.get(&c.id).map(|e| e.next)
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        let ids: std::collections::HashSet<&str> = credentials.iter().map(|c| c.id.as_str()).collect();
        self.entries.retain(|id, _| ids.contains(id.as_str()));
    }
}

/// A refresh token the provider no longer accepts, which backs off exponentially.
pub(crate) fn invalid_grant(error: &ExecError) -> bool {
    String::from_utf8_lossy(&error.body)
        .to_ascii_lowercase()
        .contains("invalid_grant")
}

/// Whether a failed refresh of `c` is a definite answer that its refresh token will never
/// work again, so the credential leaves rotation until its tokens change. `confirmed`
/// means an upstream just refused the access token this refresh started from, so
/// neither token works. Go `isTerminalRefreshError` and `hasUnauthorizedAuthFailure`:
/// an expired, reused, invalidated or revoked refresh token is terminal; `invalid_grant`
/// is terminal for Codex, and for other providers only once `confirmed`, since Go keeps
/// retrying a Claude `invalid_grant` while its access token may still work. Go's
/// `invalid_client` is left out: Claude and Codex send one client ID for every account.
pub(crate) fn refused_for_good(c: &Credential, error: &ExecError, confirmed: bool) -> bool {
    const TERMINAL: &[&str] = &[
        "refresh_token_expired",
        "refresh_token_reused",
        "refresh_token_invalidated",
        "token_revoked",
    ];
    if !matches!(error.status, 400 | 401) {
        return false;
    }
    let text = String::from_utf8_lossy(&error.body).to_ascii_lowercase();
    TERMINAL.iter().any(|code| text.contains(code))
        || text.contains("invalid_grant") && (confirmed || c.provider.trim().eq_ignore_ascii_case("codex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::exec::FailureScope;
    use std::path::Path;

    #[test]
    fn pending_failure_ineffective_rotation_and_invalid_grant_backoffs() {
        let mut c = Credential::from_file(
            Path::new("/mock"),
            Path::new("/mock/a"),
            serde_json::from_str(r#"{"type":"claude"}"#).unwrap(),
        )
        .unwrap();
        c.revision = 1;
        let now = Instant::now();
        let mut s = RefreshState::default();
        assert!(s.reserve(&c, now));
        assert!(!s.reserve(&c, now + Duration::from_secs(59)));
        assert!(s.reserve(&c, now + Duration::from_secs(60)));
        let error = ExecError::local(500, FailureScope::Transport, "mock failure");
        s.finish(&c, Some(&error), false, now);
        assert!(!s.reserve(&c, now + Duration::from_secs(299)));
        assert!(s.reserve(&c, now + Duration::from_secs(300)));
        c.revision = 2;
        s.finish(&c, None, true, now);
        assert!(!s.reserve(&c, now + Duration::from_secs(29)));
        assert!(s.reserve(&c, now + Duration::from_secs(30)));
        let error = ExecError::local(400, FailureScope::Credential, "invalid_grant");
        let mut t = now;
        for delay in [60, 120, 240, 480, 960, 1800, 1800] {
            s.finish(&c, Some(&error), false, t);
            assert!(!s.reserve(&c, t + Duration::from_secs(delay - 1)));
            t += Duration::from_secs(delay);
            assert!(s.reserve(&c, t));
        }
        c.revision = 3;
        assert!(s.reserve(&c, t), "management edit invalidates backoff");
        c.disabled = true;
        c.revision = 4;
        assert!(!s.reserve(&c, t), "disabled credentials never refresh");
        s.reconcile(&[]);
        assert!(s.entries.is_empty(), "deletion drops refresh state");
    }

    #[test]
    fn only_a_definite_refusal_ends_refreshes() {
        let credential = |provider: &str| {
            Credential::from_file(
                Path::new("/mock"),
                Path::new("/mock/a"),
                serde_json::from_str(&format!(r#"{{"type":"{provider}"}}"#)).unwrap(),
            )
            .unwrap()
        };
        let (claude, codex) = (credential("claude"), credential("codex"));
        let error = |status, text: &str| ExecError::local(status, FailureScope::Credential, text);
        let expired = error(400, "Claude OAuth acquisition failed: refresh_token_expired");
        let grant = error(400, "Claude OAuth acquisition failed: invalid_grant");
        let codex_grant = error(
            401,
            "token refresh failed after 3 attempts: token refresh failed with status 400: invalid_grant",
        );
        let reused = error(401, "token refresh failed with status 400: refresh_token_reused");
        assert!(refused_for_good(&claude, &expired, false));
        assert!(
            !refused_for_good(&claude, &grant, false),
            "an unconfirmed Claude invalid_grant"
        );
        assert!(refused_for_good(&claude, &grant, true));
        assert!(refused_for_good(&codex, &codex_grant, false));
        assert!(refused_for_good(&codex, &reused, false));
        for transient in [
            error(502, "Claude OAuth acquisition failed"),
            error(503, "home refresh temporarily unavailable"),
            error(429, "Claude refresh temporarily blocked"),
            error(400, "Claude OAuth acquisition failed"),
            error(400, "Claude OAuth acquisition failed: invalid_client"),
            error(502, "token refresh failed with status 503: invalid_grant"),
        ] {
            assert!(!refused_for_good(&claude, &transient, true), "{transient:?}");
            assert!(!refused_for_good(&codex, &transient, true), "{transient:?}");
        }
    }
}
