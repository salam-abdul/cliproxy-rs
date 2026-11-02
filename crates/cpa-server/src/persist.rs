//! Remote persistence for config and auth files (Go's `storePersister` and the token
//! store's `Delete`). The Postgres, git and object stores mirror their content into a
//! local workspace; the server keeps working on those files and reports each change
//! here so the store can push it.

pub use cpa_core::store::StorePersister;
