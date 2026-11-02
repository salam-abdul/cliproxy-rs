//! The cooldown records shared by the `.cds` files and remote cooldown stores, in Go's
//! `CooldownStateRecord` layout and field names.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Go `CooldownStateRecord`. An empty `model` is the credential-level record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default)]
    pub auth_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, with = "go_time")]
    pub next_retry_after: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default)]
    pub quota: Quota,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,
    #[serde(default, with = "go_time")]
    pub updated_at: Option<SystemTime>,
    /// The credential file (Go `AuthFile`, `json:"-"`): decides the `.cds` path.
    #[serde(skip)]
    pub auth_file: Option<PathBuf>,
}

/// Go `QuotaState` cooldown fields (`cooldownFieldsOf`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub exceeded: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, with = "go_time")]
    pub next_recover_at: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub backoff_level: u32,
    #[serde(default, with = "go_time")]
    pub observed_at: Option<SystemTime>,
    /// cliproxy-rs only (`max-trusted-cooldown`): bounded windows spent, 0 for none.
    /// Written on every quota record cliproxy-rs saves, so its presence marks a record
    /// whose deadline restores as it is. Go's decoder ignores it; records without it
    /// (Go's, or older) get the first bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_windows: Option<u32>,
}

/// Go `Error`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastError {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub http_status: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Go `time.Time` JSON: RFC 3339 with trimmed nanoseconds; the zero time is
/// `0001-01-01T00:00:00Z` (`None` here).
pub mod go_time {
    use super::*;

    const ZERO: &str = "0001-01-01T00:00:00Z";

    pub fn serialize<S: serde::Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&rfc3339_nano(*t)),
            None => s.serialize_str(ZERO),
        }
    }

    /// Go `time.RFC3339Nano`: nanoseconds with trailing zeros trimmed, no dot when zero.
    pub fn rfc3339_nano(t: SystemTime) -> String {
        let text = DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Nanos, true);
        let (head, _) = text.split_once('Z').unwrap_or((&text, ""));
        let head = head.trim_end_matches('0').trim_end_matches('.');
        format!("{head}Z")
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        let raw = Option::<String>::deserialize(d)?.unwrap_or_default();
        if raw.is_empty() {
            return Ok(None);
        }
        let parsed = DateTime::parse_from_rfc3339(&raw).map_err(serde::de::Error::custom)?;
        // Go's zero time and anything before the epoch mean "no time".
        Ok(u64::try_from(parsed.timestamp())
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::new(secs, parsed.timestamp_subsec_nanos())))
    }
}

/// A cooldown store other than `.cds` files (Go `CooldownStateStore` from a token store
/// that is a `CooldownStateStoreProvider`: PGSTORE's `cooldown_store` table). Used
/// while `save-cooldown-status` is on, in place of the files.
pub trait Backend: Send + Sync {
    /// Go `Load`.
    fn load(&self) -> Result<Vec<Record>, String>;
    /// Go `Save`: the complete live set; records no longer present are cleared.
    fn save(&self, records: Vec<Record>, now: SystemTime) -> Result<(), String>;
}
