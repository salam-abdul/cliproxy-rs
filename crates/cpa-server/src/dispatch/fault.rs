//! One attempt's failure, and the verdict on it: whether the request stops, whether the
//! failure changes the credential's availability, and whether the credential's lease
//! ends with it.

use axum::http::HeaderMap;
use cpa_core::exec::{AfterAccept, ExecError, FailureScope};

use super::{Failure, RunError};
use crate::classify;
use crate::scheduler::ErrorAction;

/// One attempt's failure.
#[derive(Clone)]
pub(super) struct Fault {
    pub(super) error: ExecError,
    pub(super) bootstrap: Option<Box<HeaderMap>>,
    pub(super) reached: Reached,
}

/// How far an attempt's response got before it failed, as dispatch saw it. With the
/// executor's [`AfterAccept`], it decides failover: only a response that broke off
/// before any of it was consumed may move to another credential.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reached {
    /// The executor failed without a response.
    Executing,
    /// An accepted response failed before any of it was consumed.
    NoOutput,
    /// Part of an accepted response was already consumed.
    Output,
}

/// What [`Fault::verdict`] needs to know about the attempt beyond its error.
pub(super) struct VerdictInput<'a> {
    /// The request is a `responses/compact` request.
    pub(super) compact: bool,
    /// The upstream model of a count_tokens attempt.
    pub(super) count_tokens: Option<&'a str>,
    /// The attempt used the last model of the credential's alias pool.
    pub(super) last_model: bool,
}

/// What dispatch does after an attempt's failure.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Verdict {
    /// The request ends with this failure.
    pub(super) stop: bool,
    /// The failure must not change the credential's availability.
    pub(super) neutral: bool,
    /// The credential tries no further model of its alias pool, and its lease ends here.
    pub(super) last_here: bool,
}

impl Fault {
    pub(super) fn executing(error: ExecError) -> Self {
        Self {
            error,
            bootstrap: None,
            reached: Reached::Executing,
        }
    }

    /// The verdict on this failure under the credential's error rule `action`.
    pub(super) fn verdict(&self, action: &ErrorAction, input: VerdictInput<'_>) -> Verdict {
        let error = &self.error;
        // A compact request fault, or a count_tokens 404 that names no model, leaves the
        // credential's availability alone unless a matched rule forces a cooldown.
        let neutral = !action.force_cooldown
            && ((input.compact && classify::is_compact_neutral(error))
                || input
                    .count_tokens
                    .is_some_and(|model| classify::is_count_endpoint_missing(error, model)));
        // A local request failure, or a response that broke off after the executor had it
        // or after output, is terminal even when a custom rule says continue: inference
        // may already have run. One that broke off before any output may fail over.
        let stop = self.terminal()
            || if action.matched {
                action.stop
            } else {
                (input.compact && classify::is_compact_fault(error))
                    || (!self.broke_silently() && classify::is_request_invalid(error))
            };
        // Go returns stopping bootstrap failures directly, outside handler bootstrap
        // retries.
        let last_here = stop || input.last_model || classify::credential_scoped(error) || error.held.is_some();
        Verdict {
            stop,
            neutral,
            last_here,
        }
    }

    /// Whether the failover rule ends the request here: output was consumed, an
    /// accepted response broke off after the executor had it, or the executor reported
    /// a request failure of its own. A response that broke off before any output may
    /// fail over.
    pub(super) fn terminal(&self) -> bool {
        let error = &self.error;
        let broke = self.broke_off();
        match self.reached {
            Reached::Output => true,
            Reached::NoOutput if broke => false,
            _ => broke || (error.scope == FailureScope::Request && error.headers.is_empty()),
        }
    }

    /// The response broke off: as the executor reports it, or a connection fault once a
    /// response was accepted.
    fn broke_off(&self) -> bool {
        self.error.after_accept.is_some_and(AfterAccept::broke_off)
            || (self.reached != Reached::Executing && self.error.scope == FailureScope::Transport)
    }

    /// A response that broke off before any of it was consumed.
    pub(super) fn broke_silently(&self) -> bool {
        self.reached == Reached::NoOutput && self.broke_off()
    }

    /// The account is not tried again for this request: its response broke off before
    /// any output, or its stream was held back on a first-event error
    /// ([`ExecError::held`]).
    pub(super) fn spends_account(&self) -> bool {
        self.broke_silently() || self.error.held.is_some()
    }

    pub(super) fn into_run_error(self) -> RunError {
        RunError {
            failure: Failure::Exec(self.error),
            bootstrap: self.bootstrap.is_some(),
        }
    }
}
