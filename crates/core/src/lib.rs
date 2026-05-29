//! Sakurasato core: shared domain types, configuration, and persistence.

#![forbid(unsafe_code)]

pub mod config;
pub mod model;
pub mod repo;

pub use config::Config;

/// Embed the workspace's `migrations/` directory so it ships with the binary.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[cfg(test)]
mod tests {
    #[test]
    fn migrator_embeds_all_six_m2_migrations() {
        // 0001 actor / 0002 note / 0003 follow / 0004 delivery_queue
        // 0005 emoji / 0006 reaction
        assert_eq!(crate::MIGRATOR.migrations.len(), 6);
    }
}
