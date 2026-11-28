use axum::http::{HeaderMap, HeaderValue, header::CONTENT_TYPE};
use cpa_core::exec::{AfterAccept, ExecError, FailureScope, HeldStream};

use super::fault::{Fault, Reached, Verdict, VerdictInput};
use crate::scheduler::ErrorAction;

/// An error the upstream answered with: it carries response headers.
fn answered(status: u16, scope: FailureScope, body: &str) -> ExecError {
    let mut error = ExecError::local(status, scope, body);
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    error.headers = Box::new(headers);
    error
}

fn fault(error: ExecError, reached: Reached) -> Fault {
    Fault {
        error,
        bootstrap: None,
        reached,
    }
}

const NO_RULE: ErrorAction = ErrorAction {
    matched: false,
    stop: false,
    cooldown: true,
    force_cooldown: false,
};

const RULE_CONTINUE: ErrorAction = ErrorAction {
    matched: true,
    stop: false,
    cooldown: false,
    force_cooldown: false,
};

const RULE_STOP: ErrorAction = ErrorAction {
    matched: true,
    stop: true,
    cooldown: false,
    force_cooldown: false,
};

fn plain_attempt() -> VerdictInput<'static> {
    VerdictInput {
        compact: false,
        count_tokens: None,
        last_model: false,
    }
}

fn verdict(stop: bool, neutral: bool, last_here: bool) -> Verdict {
    Verdict {
        stop,
        neutral,
        last_here,
    }
}

/// Once output was consumed, or the executor reported a request failure of its own,
/// the request ends even when a matched rule says continue: inference may have run.
#[test]
fn a_continue_rule_never_resends_a_request_that_may_have_run() {
    let consumed = fault(answered(503, FailureScope::Credential, "overloaded"), Reached::Output);
    assert_eq!(
        consumed.verdict(&RULE_CONTINUE, plain_attempt()),
        verdict(true, false, true)
    );
    let local = fault(
        ExecError::local(400, FailureScope::Request, "bad body"),
        Reached::Executing,
    );
    assert_eq!(
        local.verdict(&RULE_CONTINUE, plain_attempt()),
        verdict(true, false, true)
    );
}

/// A matched rule decides whether a failure the request could survive stops it.
#[test]
fn a_matched_rule_decides_stop() {
    let unavailable = fault(
        answered(503, FailureScope::Credential, "unavailable"),
        Reached::Executing,
    );
    assert_eq!(
        unavailable.verdict(&RULE_STOP, plain_attempt()),
        verdict(true, false, true)
    );
    assert_eq!(
        unavailable.verdict(&NO_RULE, plain_attempt()),
        verdict(false, false, false)
    );
    let invalid = fault(answered(400, FailureScope::Request, "bad request"), Reached::Executing);
    assert_eq!(
        invalid.verdict(&RULE_CONTINUE, plain_attempt()),
        verdict(false, false, false)
    );
}

/// Without a rule an invalid request stops, except a response that broke off before any
/// output, which may fail over.
#[test]
fn an_invalid_request_stops_unless_it_broke_off_silently() {
    let invalid = fault(answered(400, FailureScope::Request, "bad request"), Reached::Executing);
    assert_eq!(invalid.verdict(&NO_RULE, plain_attempt()), verdict(true, false, true));
    let broke = fault(
        answered(400, FailureScope::Request, "bad request").accepted(AfterAccept::Truncated),
        Reached::NoOutput,
    );
    assert_eq!(broke.verdict(&NO_RULE, plain_attempt()), verdict(false, false, false));
}

/// A compact request stops on a compact fault and leaves the credential's availability
/// alone, unless a matched rule forces a cooldown.
#[test]
fn a_compact_fault_stops_and_is_neutral_unless_a_rule_forces_cooldown() {
    let missing = fault(answered(404, FailureScope::Model, "not found"), Reached::Executing);
    assert_eq!(missing.verdict(&NO_RULE, plain_attempt()), verdict(false, false, false));
    let compact = VerdictInput {
        compact: true,
        ..plain_attempt()
    };
    assert_eq!(missing.verdict(&NO_RULE, compact), verdict(true, true, true));
    let forced = ErrorAction {
        matched: true,
        stop: false,
        cooldown: true,
        force_cooldown: true,
    };
    assert_eq!(missing.verdict(&forced, compact), verdict(false, false, false));
}

/// A count_tokens 404 that names no model is a missing endpoint: neutral, not a stop.
/// One that says the model was not found changes availability like any 404.
#[test]
fn a_missing_count_tokens_endpoint_is_neutral() {
    let missing = fault(answered(404, FailureScope::Model, "not found"), Reached::Executing);
    let count = VerdictInput {
        count_tokens: Some("claude-sonnet"),
        ..plain_attempt()
    };
    assert_eq!(missing.verdict(&NO_RULE, count), verdict(false, true, false));
    let model = fault(
        answered(404, FailureScope::Model, r#"{"error":{"code":"model_not_found"}}"#),
        Reached::Executing,
    );
    assert_eq!(model.verdict(&NO_RULE, count), verdict(false, false, false));
}

/// A connection fault after a response was accepted is a break, so it fails over
/// once per account; before any response it is an ordinary transport fault.
#[test]
fn a_transport_fault_after_acceptance_broke_off() {
    let error = ExecError::local(502, FailureScope::Transport, "connection reset");
    let mut fault = Fault::executing(error);
    assert!(!fault.broke_silently());
    assert!(!fault.terminal());
    fault.reached = Reached::NoOutput;
    assert!(fault.broke_silently());
    assert!(!fault.terminal());
    fault.reached = Reached::Output;
    assert!(fault.terminal());
}

/// The credential's turn ends after the last model of its pool, on a credential-wide
/// quota, and on a held first-event error, without stopping the request.
#[test]
fn the_credential_turn_ends_without_stopping_the_request() {
    let unavailable = fault(
        answered(503, FailureScope::Credential, "unavailable"),
        Reached::Executing,
    );
    let last = VerdictInput {
        last_model: true,
        ..plain_attempt()
    };
    assert_eq!(unavailable.verdict(&NO_RULE, last), verdict(false, false, true));
    let quota = fault(answered(429, FailureScope::Credential, "quota"), Reached::Executing);
    assert_eq!(quota.verdict(&NO_RULE, plain_attempt()), verdict(false, false, true));
    let mut overloaded = answered(529, FailureScope::Credential, "overloaded");
    overloaded.held = Some(std::sync::Arc::new(HeldStream {
        events: vec![],
        end: overloaded.clone(),
    }));
    let held = fault(overloaded, Reached::Executing);
    assert_eq!(held.verdict(&NO_RULE, plain_attempt()), verdict(false, false, true));
}
