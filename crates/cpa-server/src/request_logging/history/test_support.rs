//! Helpers shared by the history tests.

use super::body::{Bodies, Budget, HttpBodies, PendingRow};
use super::row::Row;

/// `row` as an HTTP send with empty bodies.
pub(crate) fn http(row: Row, budget: &Budget) -> PendingRow {
    PendingRow {
        row,
        bodies: Bodies::Http(HttpBodies::new(budget)),
    }
}

/// The HTTP bodies of `pending`, which [`http`] made.
pub(crate) fn http_bodies(pending: &mut PendingRow) -> &mut HttpBodies {
    match &mut pending.bodies {
        Bodies::Http(http) => http,
        Bodies::Websocket { .. } => panic!("not an HTTP row"),
    }
}
