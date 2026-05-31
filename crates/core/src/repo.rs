//! Repository layer (`sqlx` queries) for the M2 schema.
//!
//! CLAUDE.md §10 mandates compile-time checked queries (`query!`/`query_as!`).
//! The `actor` table demonstrates the full pattern (including how to override
//! Rust types for JSONB columns via the `as "col: Type"` syntax). Other
//! tables ship with a single insert helper here to prove they are wired and
//! reachable; richer accessors (mutations, listings, joins) are added in the
//! milestone that first needs them (M3 for `delivery_queue`, M8 for `emoji`
//! / `reaction`, etc).

pub mod actor;
pub mod announce;
pub mod api_token;
pub mod delivery_queue;
pub mod emoji;
pub mod follow;
pub mod media;
pub mod note;
pub mod reaction;
