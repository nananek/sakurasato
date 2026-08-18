//! Sakurasato server library: HTTP router, state, init/serve commands.
//!
//! Exposed as a library so integration tests in `tests/` can build the
//! router against a `sqlx::PgPool` produced by `#[sqlx::test]` without
//! spinning up a real TCP socket or DB pool of their own.

#![forbid(unsafe_code)]
// `miauth::meta` の `/api/meta` builder で `serde_json::json!{}` を ~50 field の
// top-level に対して展開するため、default 128 では不足する。256 で足りる。
#![recursion_limit = "256"]

pub mod actor_admin;
pub mod block;
pub mod cli;
pub mod delivery;
pub(crate) mod dispatch;
pub mod emoji_backfill;
pub mod emoji_import;
pub(crate) mod emoji_learn;
pub mod event_bus;
pub(crate) mod extract;
pub(crate) mod fetch_rate_limit;
pub mod follow;
pub mod follow_request;
pub(crate) mod http_client;
pub mod init;
pub mod list_cli;
pub mod local_api;
pub mod media_proxy_client;
// M14 #157: MiAuth foundation (= 親 issue #150)。実 endpoint は #158-#160 で
// 順次追加され、本 module ツリーが膨らんでいく。
pub mod miauth;
pub mod miauth_cli;
pub mod move_accept;
pub mod move_out;
pub mod multikey;
pub(crate) mod net_guard;
pub mod notification;
pub mod prune;
pub mod remote_actor;
pub mod routes;
pub mod serve;
pub(crate) mod sign;
pub mod state;
pub(crate) mod text;
pub mod token;
pub mod webfinger_guard;

#[cfg(test)]
mod inbox_signature_tests;
