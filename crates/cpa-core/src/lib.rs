//! Contracts shared by every cliproxy-rs crate.
//!
//! - [`config`]: `config.yaml`, compatible with CLIProxyAPI.
//! - [`credential`]: runtime credentials loaded from `auth-dir` or config.
//! - [`exec`]: the execution envelope passed between the runtime and executors.
//! - [`format`]: request/response wire formats.
//! - [`registry`]: the pinned static model catalog.
//! - [`store`]: the contracts between the server and remote config, auth and cooldown stores.
//! - [`private_fs`]: owner-only files and directories.
//! - [`gotime`]: Go `time.Time` text.

pub mod config;
pub mod credential;
pub mod exec;
pub mod format;
pub mod gotime;
pub mod private_fs;
pub mod registry;
pub mod store;
