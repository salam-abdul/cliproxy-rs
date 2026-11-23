use super::*;

const MINUTE: i64 = 60_000;
const H: i64 = 3_600_000;
const D: i64 = 24 * H;
const R: i64 = 1_800_000_000_000;

fn point(at: i64, used: f64, reset: Option<i64>) -> Point {
    Point { at, used, reset }
}

#[test]
fn a_stated_reset_that_passes_starts_a_new_cycle() {
    let reset = 2 * D;
    let points = [
        point(0, 10.0, Some(reset)),
        point(D, 60.0, Some(reset)),
        point(2 * D + 11 * MINUTE, 62.0, Some(reset + 7 * D)),
    ];
    let cycles = cycles(&points, 2 * D + H);
    assert_eq!(cycles.len(), 2, "{cycles:?}");
    assert_eq!((cycles[0].last, cycles[0].ended), (60.0, true));
    assert_eq!((cycles[1].first, cycles[1].ended), (62.0, false));
}

#[test]
fn a_fall_of_more_than_five_points_starts_a_new_cycle_without_a_stated_reset() {
    let points = [point(0, 40.0, None), point(H, 70.0, None), point(2 * H, 3.0, None)];
    let cycles = cycles(&points, 3 * H);
    assert_eq!(cycles.len(), 2, "{cycles:?}");
    assert_eq!((cycles[0].last, cycles[0].ended), (70.0, true));
    assert!(!cycles[1].ended);
}

#[test]
fn a_fall_of_five_points_or_less_does_not() {
    let points = [point(0, 40.0, None), point(H, 35.0, None)];
    let cycles = cycles(&points, 2 * H);
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert_eq!((cycles[0].peak, cycles[0].last), (40.0, 35.0));
}

#[test]
fn a_drifting_reset_stays_one_cycle() {
    let points: Vec<Point> = (0..10)
        .map(|k| point(k * H, 10.0 + k as f64, Some(R + k * 1_000)))
        .collect();
    let cycles = cycles(&points, 10 * H);
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert_eq!(cycles[0].reset, Some(R + 9_000));
}

#[test]
fn a_missing_reset_is_not_a_reset() {
    let reset = 5 * D;
    let points = [
        point(0, 10.0, Some(reset)),
        point(H, 12.0, None),
        point(2 * H, 14.0, Some(reset)),
    ];
    let cycles = cycles(&points, 3 * H);
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert_eq!(cycles[0].reset, Some(reset));
    let unstated = super::cycles(&points[..2], 3 * H);
    assert_eq!(unstated[0].reset, Some(reset), "{unstated:?}");
    let past = [point(0, 10.0, Some(H)), point(2 * H, 12.0, None)];
    assert_eq!(super::cycles(&past, 3 * H).len(), 1);
}

#[test]
fn a_reset_in_the_past_ends_the_last_cycle() {
    let points = [point(0, 30.0, Some(H))];
    assert!(cycles(&points, 2 * H)[0].ended);
    assert!(!cycles(&points, H / 2)[0].ended);
}
