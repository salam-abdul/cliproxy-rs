//! Error identifiers that mark a failure as the request's own fault
//! (internal/clienterror/client_error.go `requestFaultCodes` and `requestFaultTypes`): no
//! account can serve the request, so it ends without failover and cools no account.
//! Callers match them against an error's structured `code` or `type`, never against its
//! message, which can echo the prompt.

/// Safety-policy rejections of the prompt.
const POLICY_CODES: [&str; 6] = [
    "cyber_policy",
    "cyber_policy_violation",
    "bio_policy",
    "misalignment_policy_violation",
    "content_policy_violation",
    "image_content_policy_violation",
];

const OTHER_CODES: [&str; 8] = [
    "context_length_exceeded",
    "message_too_big",
    "string_above_max_length",
    "invalid_prompt",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "previous_response_not_found",
];

const TYPES: [&str; 4] = [
    "invalid_request",
    "invalid_request_error",
    "bad_request_error",
    "invalid_prompt",
];

fn listed(list: &[&str], value: &str) -> bool {
    let value = value.trim();
    list.iter().any(|item| item.eq_ignore_ascii_case(value))
}

/// An error `code` that marks a request fault, ignoring case and surrounding space.
pub fn is_code(code: &str) -> bool {
    listed(&POLICY_CODES, code) || listed(&OTHER_CODES, code)
}

/// An error `type` that marks a request fault, ignoring case and surrounding space.
pub fn is_type(kind: &str) -> bool {
    listed(&TYPES, kind)
}

/// A code that rejects the prompt itself: a safety policy or `invalid_prompt`. A stream
/// failure that carries one is a 400 whatever its error type says.
pub fn is_prompt_rejection(code: &str) -> bool {
    listed(&POLICY_CODES, code) || code.trim().eq_ignore_ascii_case("invalid_prompt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_codes_are_request_faults_in_any_case() {
        for code in POLICY_CODES {
            assert!(is_code(code) && is_prompt_rejection(code), "{code}");
            assert!(is_code(&format!(" {} ", code.to_uppercase())), "{code}");
        }
        assert!(is_prompt_rejection("invalid_prompt"));
        assert!(is_code("context_length_exceeded") && !is_prompt_rejection("context_length_exceeded"));
        assert!(!is_code("server_error") && !is_code("") && !is_code("bio_policy is mentioned here"));
        assert!(is_type("Invalid_Request_Error") && !is_type("server_error"));
    }
}
