use super::*;

/// What `soonest-reset` knows about a credential's usage windows, from its latest
/// passive quota observation (Claude `anthropic-ratelimit-unified-*`, Codex
/// `x-codex-*`). The default is "nothing known".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Windows {
    /// When the long (weekly) window resets, while that is still ahead.
    pub weekly_reset: Option<SystemTime>,
    /// The long window's reset once it has passed: the window rolled over, so what was
    /// learned about it is stale and the credential may be probed again. Only a reset
    /// upstream stated (an absolute time, or a delay above zero) identifies a rollover;
    /// a zero delay would make every answer's observation time a new one.
    pub rolled_over: Option<Rollover>,
    /// A window (Claude 5-hour or 7-day, Codex primary or secondary) is used up and has
    /// not reset yet: upstream would refuse until it does.
    pub exhausted: bool,
}

/// Which window a rollover belongs to. Probe markers are kept per window, so a
/// rollover of one window is never compared with a probe made for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowId {
    Claude7d,
    Claude5h,
    CodexPrimary,
    CodexSecondary,
}

/// A long window that rolled over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rollover {
    pub window: WindowId,
    /// The stated reset that passed.
    pub at: SystemTime,
    /// How far apart two rollovers of this window must be to count as different ones:
    /// half its length (five hours when unknown), which absorbs the rounding of
    /// relative resets around a boundary.
    pub gap: Duration,
}

/// How long a used-up window without a usable reset or length is assumed to hold.
pub(super) const FALLBACK_WINDOW: Duration = Duration::from_secs(5 * 3600);

/// One usage window read from quota signals; each part may be missing.
#[derive(Debug, Clone, Copy)]
pub(super) struct Window {
    id: WindowId,
    used_up: bool,
    reset: Option<SystemTime>,
    /// `reset` when it can identify a rollover (not synthesized from a zero delay).
    stated_reset: Option<SystemTime>,
    /// The window length, when one was reported and can be represented.
    length: Option<Duration>,
    /// The percent of the window used, as stated.
    used: Option<f64>,
}

impl Window {
    /// Until when a used-up window holds: its reset, else its length (or five hours)
    /// after the observation. `None` when it is not used up.
    fn exhausted_until(&self, observed_at: SystemTime) -> Option<SystemTime> {
        if !self.used_up {
            return None;
        }
        self.reset.or_else(|| {
            self.length
                .and_then(|l| observed_at.checked_add(l))
                .or_else(|| observed_at.checked_add(FALLBACK_WINDOW))
        })
    }
}

/// A window length in minutes, when positive and representable as a deadline from
/// `observed_at`; otherwise the missing-length policy applies.
pub(super) fn window_length(minutes: f64, observed_at: SystemTime) -> Option<Duration> {
    if !minutes.is_finite() || minutes <= 0.0 {
        return None;
    }
    let length = Duration::try_from_secs_f64(minutes * 60.0).ok()?;
    observed_at.checked_add(length).map(|_| length)
}

use crate::quota_time::reset_time;

/// A signal's value by case-insensitive header name.
fn signal<'a>(signals: &'a std::collections::BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    signals
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

/// Codex's (long, short) windows: the longer window is the weekly one, whichever header
/// carries it; without lengths, secondary is weekly. A lone window is weekly only when it
/// lasts at least a day, or, for secondary, when its length is unknown.
fn codex_long_short(primary: Option<Window>, secondary: Option<Window>) -> (Option<Window>, Option<Window>) {
    let day = Duration::from_secs(24 * 3600);
    match (primary, secondary) {
        (Some(p), Some(s)) if p.length.unwrap_or(Duration::ZERO) > s.length.unwrap_or(Duration::MAX) => {
            (Some(p), Some(s))
        }
        (Some(p), Some(s)) => (Some(s), Some(p)),
        (Some(p), None) if p.length.is_some_and(|l| l >= day) => (Some(p), None),
        (None, Some(s)) if s.length.is_none_or(|l| l >= day) => (Some(s), None),
        (only, None) | (None, only) => (None, only),
    }
}

/// A snapshot's (long, short) windows for `provider` (`claude` or `codex`; anything else
/// has none).
fn parse_windows(
    provider: &str,
    signals: &std::collections::BTreeMap<String, String>,
    observed_at: SystemTime,
) -> (Option<Window>, Option<Window>) {
    let get = |name: &str| signal(signals, name);
    let number = |name: &str| get(name).and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite());
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => {
            let window = |id: WindowId, label: &str, minutes: f64| {
                let reset = get(&format!("anthropic-ratelimit-unified-{label}-reset")).and_then(reset_time);
                let utilization = number(&format!("anthropic-ratelimit-unified-{label}-utilization"));
                Window {
                    id,
                    used_up: get(&format!("anthropic-ratelimit-unified-{label}-status"))
                        .is_some_and(|s| s.eq_ignore_ascii_case("rejected"))
                        || utilization.is_some_and(|u| u >= 1.0),
                    reset,
                    stated_reset: reset,
                    length: window_length(minutes, observed_at),
                    // Claude states utilization as a fraction.
                    used: utilization.map(|u| u * 100.0),
                }
            };
            (
                Some(window(WindowId::Claude7d, "7d", 7.0 * 24.0 * 60.0)),
                Some(window(WindowId::Claude5h, "5h", 300.0)),
            )
        }
        "codex" => {
            // Usage, reset and length are read independently; a window exists when
            // any of them was reported.
            let window = |id: WindowId, label: &str| {
                let p = format!("x-codex-{label}-");
                let used = number(&format!("{p}used-percent"));
                let absolute = get(&format!("{p}reset-at")).and_then(reset_time);
                let after = number(&format!("{p}reset-after-seconds")).filter(|s| *s >= 0.0);
                let relative = after.and_then(|a| observed_at.checked_add(Duration::try_from_secs_f64(a).ok()?));
                let reset = absolute.or(relative);
                // A zero delay still ends an exhausted window now, but its time is
                // just the observation's, so it names no rollover.
                let stated_reset = absolute.or(relative.filter(|_| after.is_some_and(|a| a > 0.0)));
                let length = number(&format!("{p}window-minutes")).and_then(|m| window_length(m, observed_at));
                (used.is_some() || reset.is_some() || length.is_some()).then_some(Window {
                    id,
                    used_up: used.is_some_and(|u| u >= 100.0),
                    reset,
                    stated_reset,
                    length,
                    used,
                })
            };
            codex_long_short(
                window(WindowId::CodexPrimary, "primary"),
                window(WindowId::CodexSecondary, "secondary"),
            )
        }
        _ => (None, None),
    }
}

/// One quota window's use, as a snapshot states it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// The weekly window, or the shorter one.
    pub long: bool,
    pub used_percent: f64,
    pub reset: Option<SystemTime>,
}

/// Each quota window's use in a snapshot's `signals`, for `claude` or `codex`.
/// A window that states no use gives no reading.
pub fn readings(
    provider: &str,
    signals: &std::collections::BTreeMap<String, String>,
    observed_at: SystemTime,
) -> Vec<Reading> {
    let (long, short) = parse_windows(provider, signals, observed_at);
    [(true, long), (false, short)]
        .into_iter()
        .filter_map(|(long, window)| {
            let window = window?;
            let used_percent = window.used.filter(|u| (0.0..=100.0).contains(u))?;
            Some(Reading {
                long,
                used_percent,
                reset: window.reset,
            })
        })
        .collect()
}

/// One quota observation's windows, parsed once: what [`Windows`] reads, before it is
/// judged at a point in time.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    observed_at: SystemTime,
    /// The long (weekly) window.
    long: Option<Window>,
    /// Until when a used-up window, or Codex's limit flag, holds.
    until: Option<SystemTime>,
}

impl Observation {
    /// Reads a snapshot's `signals` (header name to value), observed at `observed_at`,
    /// for `provider` (`claude` or `codex`; anything else knows nothing).
    pub fn parse(
        provider: &str,
        signals: &std::collections::BTreeMap<String, String>,
        observed_at: SystemTime,
    ) -> Self {
        let codex = provider.trim().eq_ignore_ascii_case("codex");
        let (long, short) = parse_windows(provider, signals, observed_at);
        let windows = [long, short];
        let until = windows
            .iter()
            .flatten()
            .filter_map(|w| w.exhausted_until(observed_at))
            .max();
        // `X-Codex-Limit-Reached` lasts as long as the used-up windows do; when none is
        // used up, until the soonest reset any window reports; with no reset at all,
        // five hours from the observation.
        let flag_until = (codex
            && signal(signals, "x-codex-limit-reached").is_some_and(|v| v.eq_ignore_ascii_case("true")))
        .then(|| {
            until
                .or_else(|| windows.iter().flatten().filter_map(|w| w.reset).min())
                .or_else(|| observed_at.checked_add(FALLBACK_WINDOW))
        })
        .flatten();
        Self {
            observed_at,
            long,
            until: until.max(flag_until),
        }
    }

    pub fn observed_at(&self) -> SystemTime {
        self.observed_at
    }

    /// What the observation says at `now`.
    pub fn at(&self, now: SystemTime) -> Windows {
        let long = self.long;
        Windows {
            weekly_reset: long.and_then(|w| w.reset).filter(|r| *r > now),
            rolled_over: long.and_then(|w| {
                let at = w.stated_reset.filter(|r| *r <= now)?;
                Some(Rollover {
                    window: w.id,
                    at,
                    gap: w.length.unwrap_or(FALLBACK_WINDOW) / 2,
                })
            }),
            exhausted: self.until.is_some_and(|u| u > now),
        }
    }
}

impl Windows {
    /// [`Observation::parse`] judged at `now`.
    pub fn observed(
        provider: &str,
        signals: &std::collections::BTreeMap<String, String>,
        observed_at: SystemTime,
        now: SystemTime,
    ) -> Self {
        Observation::parse(provider, signals, observed_at).at(now)
    }

    /// `soonest-reset` order. Usable accounts come before used-up ones. Among them, an
    /// account without a known reset that has not been probed since its long window
    /// last rolled over gets one probe request, then known resets go earliest first,
    /// then accounts whose probe taught nothing.
    pub(super) fn order(&self, probed: bool) -> (bool, u8, Option<SystemTime>) {
        let class = match (self.weekly_reset, probed) {
            (Some(_), _) => 1,
            (None, false) => 0,
            (None, true) => 2,
        };
        (self.exhausted, class, self.weekly_reset)
    }
}

/// A credential's `soonest-reset` probes: present once its probe for an unknown reset
/// was spent, with the last rollover probed for, per window.
pub(super) type Probes = HashMap<WindowId, SystemTime>;

/// Whether a credential was already probed for what it reports now. A probe holds
/// until its long window rolls over after it: a reset of that same window that passed
/// and is more than the window's gap later than the rollover last probed for it.
pub(super) fn probed(record: Option<&Probes>, rolled_over: Option<Rollover>) -> bool {
    match (record, rolled_over) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(markers), Some(r)) => markers
            .get(&r.window)
            .is_some_and(|at| at.checked_add(r.gap).is_none_or(|limit| r.at <= limit)),
    }
}

/// A credential's [`Windows`] for one pick.
pub type Ranks<'a> = dyn Fn(&Credential) -> Windows + 'a;
