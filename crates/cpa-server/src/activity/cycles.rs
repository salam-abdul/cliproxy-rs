/// One weekly quota reading of a credential.
#[derive(Clone, Copy)]
pub(super) struct Point {
    pub at: i64,
    pub used: f64,
    pub reset: Option<i64>,
}

/// The readings of one credential between two quota resets.
#[derive(Debug, PartialEq)]
pub(super) struct Cycle {
    pub start: i64,
    pub end: i64,
    pub reset: Option<i64>,
    pub first: f64,
    pub peak: f64,
    pub last: f64,
    pub ended: bool,
}

/// How long past a stated reset a reading must come to start a new cycle.
const RESET_GRACE_MS: i64 = 600_000;
/// A fall in use by more than this many points starts a new cycle.
const FALL: f64 = 5.0;

/// Splits one credential's `points`, in time order, into cycles. A new cycle starts when
/// the current one's latest stated reset passed more than [`RESET_GRACE_MS`] before a
/// reading that states a reset, or when use falls by more than [`FALL`] points.
pub(super) fn cycles(points: &[Point], now: i64) -> Vec<Cycle> {
    let mut cycles: Vec<Cycle> = Vec::new();
    for point in points {
        let current = cycles.last_mut().filter(|cycle| {
            let reset_passed = point.reset.is_some()
                && cycle
                    .reset
                    .is_some_and(|reset| point.at > reset.saturating_add(RESET_GRACE_MS));
            !reset_passed && point.used >= cycle.last - FALL
        });
        match current {
            Some(cycle) => {
                cycle.end = point.at;
                cycle.reset = point.reset.or(cycle.reset);
                cycle.peak = cycle.peak.max(point.used);
                cycle.last = point.used;
            }
            None => {
                if let Some(previous) = cycles.last_mut() {
                    previous.ended = true;
                }
                cycles.push(Cycle {
                    start: point.at,
                    end: point.at,
                    reset: point.reset,
                    first: point.used,
                    peak: point.used,
                    last: point.used,
                    ended: false,
                });
            }
        }
    }
    if let Some(last) = cycles.last_mut() {
        last.ended = last.reset.is_some_and(|reset| reset < now);
    }
    cycles
}

#[cfg(test)]
#[path = "cycles_tests.rs"]
mod tests;
