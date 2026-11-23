//! Background refresh backoff. Instants are supplied so tests never sleep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, Refusal};

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

/// A refresh token the provider no longer accepts, which backs off exponentially: any
/// reported [`Refusal`], or an `invalid_grant` named in the error text of a provider that
/// reports none. A [`definite`] one is not retried, so its backoff is never used.
pub(crate) fn invalid_grant(error: &ExecError) -> bool {
    error.refusal.is_some()
        || String::from_utf8_lossy(&error.body)
            .to_ascii_lowercase()
            .contains("invalid_grant")
}

/// What started a credential preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The credential was due: the refresh loop, a request, or a one-off account call.
    Due,
    /// An upstream refused the access token of this revision with a 401. Preparation
    /// runs even when not due, unless another task already replaced that revision.
    Unauthorized(u64),
    /// An operator asked the management API to refresh this revision. It runs even when
    /// not due, and is the only trigger that sends a refused refresh token again.
    Management(u64),
}

/// The refusal in a failed refresh of `c` when it is definite: the refresh token will
/// never work again. Go `isTerminalRefreshError` and `hasUnauthorizedAuthFailure`: an
/// expired, reused, invalidated or revoked refresh token is definite, and so is a Codex
/// `invalid_grant`. Any other `invalid_grant` is definite once the access token no
/// longer serves requests (`access_serves`): Go keeps retrying it only because that
/// token may still work.
pub(crate) fn definite(c: &Credential, error: &ExecError, access_serves: bool) -> Option<Refusal> {
    let refusal = error.refusal?;
    let confirmed = !access_serves || c.provider == "codex";
    (refusal != Refusal::InvalidGrant || confirmed).then_some(refusal)
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
        let refused = |refusal| {
            let mut error = ExecError::local(401, FailureScope::Credential, "mock refusal");
            error.refusal = Some(refusal);
            error
        };
        let (grant, expired) = (refused(Refusal::InvalidGrant), refused(Refusal::Expired));
        for serves in [true, false] {
            assert_eq!(definite(&claude, &expired, serves), Some(Refusal::Expired));
            assert_eq!(definite(&codex, &grant, serves), Some(Refusal::InvalidGrant));
            let transient = ExecError::local(400, FailureScope::Credential, "invalid_grant");
            assert_eq!(definite(&claude, &transient, serves), None, "text is not a refusal");
        }
        assert_eq!(definite(&claude, &grant, true), None);
        assert_eq!(definite(&claude, &grant, false), Some(Refusal::InvalidGrant));
    }
}
