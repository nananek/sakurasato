//! Sakurasato server library: HTTP router, state, init/serve commands.
//!
//! Exposed as a library so integration tests in `tests/` can build the
//! router against a `sqlx::PgPool` produced by `#[sqlx::test]` without
//! spinning up a real TCP socket or DB pool of their own.

#![forbid(unsafe_code)]

pub mod cli;
pub mod delivery;
pub(crate) mod dispatch;
pub mod emoji_import;
pub(crate) mod extract;
pub(crate) mod http_client;
pub mod init;
pub mod local_api;
pub mod media_proxy_client;
pub mod multikey;
pub(crate) mod net_guard;
pub(crate) mod remote_actor;
pub mod routes;
pub mod serve;
pub(crate) mod sign;
pub mod state;
pub mod token;

#[cfg(test)]
mod inbox_signature_tests;
