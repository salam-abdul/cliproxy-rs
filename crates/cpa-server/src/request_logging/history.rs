//! Opt-in Parquet history of model traffic (`observability.history`): every upstream
//! send with its request and response bodies, for analysis outside the server. The
//! files hold prompts and outputs; headers, URLs, tokens and keys are never stored.

use std::path::PathBuf;

use cpa_core::config::Config;

pub(crate) mod capture;
pub mod delete;
pub(crate) mod file;
pub(crate) mod native;
pub mod writer;

const DEFAULT_MAX_TOTAL_MB: u64 = 10240;

/// Where history goes and how much disk it may use. Present only while history is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub dir: PathBuf,
    /// `None`: no cap.
    pub max_total_bytes: Option<u64>,
}

impl Settings {
    /// `None` unless `enable` is true, `dir` is set and commercial mode is off. An
    /// enabled history without a `dir` stays off rather than guessing a folder: every
    /// existing default can sit inside a working tree or a synced folder.
    pub fn from_config(cfg: &Config) -> Option<Settings> {
        let section = cfg.document.get("observability")?.get("history")?;
        if section.get("enable").and_then(serde_yaml_ng::Value::as_bool) != Some(true) || super::commercial_mode(cfg) {
            return None;
        }
        let Some(dir) = configured_dir(cfg) else {
            tracing::error!(
                "observability.history.enable is true but observability.history.dir is empty; history stays off"
            );
            return None;
        };
        let mb = section
            .get("max-total-size-mb")
            .and_then(serde_yaml_ng::Value::as_i64)
            .map_or(DEFAULT_MAX_TOTAL_MB, |mb| {
                u64::try_from(mb).unwrap_or(DEFAULT_MAX_TOTAL_MB)
            });
        Some(Settings {
            dir,
            max_total_bytes: (mb > 0).then(|| mb.saturating_mul(1 << 20)),
        })
    }
}

/// `observability.history.dir`, whether or not recording is on, so history can be
/// managed after it was turned off.
pub fn configured_dir(cfg: &Config) -> Option<PathBuf> {
    let dir = cfg
        .document
        .get("observability")?
        .get("history")?
        .get("dir")?
        .as_str()?
        .trim();
    (!dir.is_empty()).then(|| cpa_core::config::expand_home(dir))
}

/// Whether model traffic is recorded under `cfg`. Cached per config snapshot, so the
/// missing-`dir` error is logged once per publish, not per request.
pub(crate) fn on(cfg: &Config) -> bool {
    settings(cfg).is_some()
}

pub(crate) fn settings(cfg: &Config) -> std::sync::Arc<Option<Settings>> {
    cfg.derived(Settings::from_config)
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
