//! Sakurasato server library: HTTP router, state, init/serve commands.
//!
//! Exposed as a library so integration tests in `tests/` can build the
//! router against a `sqlx::PgPool` produced by `#[sqlx::test]` without
//! spinning up a real TCP socket or DB pool of their own.

#![forbid(unsafe_code)]

pub mod cli;
pub mod init;
pub mod routes;
pub mod serve;
pub mod state;
