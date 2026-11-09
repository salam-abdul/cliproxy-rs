use super::*;

#[test]
fn bodies_past_the_in_flight_budget_are_truncated_and_flagged() {
    let budget = Budget::new(10);
    let mut body = Body::new(&budget);
    body.push_slice(b"123456");
    body.push_slice(b"7890ab");
    assert_eq!(body.total(), 12);
    assert!(body.truncated());
    assert_eq!(budget.used(), 6);
    let mut other = Body::new(&budget);
    other.push_bytes(&Bytes::from_static(b"abcd"));
    assert_eq!(budget.used(), 10);
    assert!(!other.truncated());
    assert_eq!(body.into_vec(), b"123456");
    assert_eq!(budget.used(), 4);
    drop(other);
    assert_eq!(budget.used(), 0);
}

#[test]
fn a_body_keeps_its_first_32_mib() {
    let budget = Budget::new(u64::MAX);
    let mut body = Body::new(&budget);
    body.push_bytes(&Bytes::from(vec![b'a'; (BODY_CAP + 5) as usize]));
    assert_eq!(body.total(), BODY_CAP + 5);
    assert!(body.truncated());
    assert_eq!(body.into_vec().len() as u64, BODY_CAP);
}

#[test]
fn lines_are_joined_without_a_trailing_newline() {
    let budget = Budget::new(u64::MAX);
    let mut body = Body::new(&budget);
    body.push_line(b"event: a");
    body.push_line(b"data: {}");
    assert_eq!(body.into_vec(), b"event: a\ndata: {}");
}
