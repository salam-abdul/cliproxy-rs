//! Go `net/url.Parse` for xAI endpoint validation: `cpa_core::config::go_url` with Go's
//! error text, and the check that the HTTP client contacts the authority Go parses.

use cpa_common::gostr::quote;
pub use cpa_core::config::go_url::GoUrl;

/// `url.Parse`. The error is Go's `*url.Error` text.
pub fn parse(raw: &str) -> Result<GoUrl, String> {
    cpa_core::config::go_url::parse(raw).map_err(|e| e.message(|b| quote(b)))
}

/// Whether the HTTP client, which parses URLs with WHATWG rules (`url`), would contact
/// the same scheme, host and port that Go's `net/url` parses from `raw`. They disagree on
/// inputs such as `https://auth.x.ai\@evil.example/`; a request must never go to a host
/// Go would not contact.
pub fn same_authority(raw: &str) -> bool {
    let (Ok(go), Ok(wh)) = (parse(raw), url::Url::parse(raw)) else {
        return false;
    };
    if go.scheme != wh.scheme() {
        return false;
    }
    let host_matches = match wh.host() {
        Some(url::Host::Domain(domain)) => {
            matches!(url::Host::parse(&go.hostname), Ok(url::Host::Domain(d)) if d == domain)
        }
        Some(url::Host::Ipv4(ip)) => go.hostname.parse::<std::net::Ipv4Addr>() == Ok(ip),
        Some(url::Host::Ipv6(ip)) => go.hostname.parse::<std::net::Ipv6Addr>() == Ok(ip),
        None => false,
    };
    let go_port = if go.port.is_empty() {
        url::Url::parse(&format!("{}://h/", go.scheme))
            .ok()
            .and_then(|u| u.port_or_known_default())
    } else {
        go.port.parse::<u16>().ok()
    };
    host_matches && go_port.is_some() && go_port == wh.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_must_agree_with_go_on_the_authority() {
        for ok in [
            "https://auth.x.ai/oauth2/token",
            "https://AUTH.X.AI:443/t",
            "https://auth.x.ai:8443/t?q=1#f",
            "https://u:p@auth.x.ai/t",
            "https://例.x.ai/t",
            "http://127.0.0.1:9/t",
            "https://[::1]:8443/t",
        ] {
            assert!(same_authority(ok), "{ok}");
        }
        for bad in [
            r"https://attacker.example\[::1%25.x.ai]/oauth2/token",
            r"https://auth.x.ai\@evil.example/t",
            "https://auth.x.ai:99999/t",
            "https://%61uth.x.ai/t",
            "https:auth.x.ai/t",
        ] {
            assert!(!same_authority(bad), "{bad}");
        }
    }
}
