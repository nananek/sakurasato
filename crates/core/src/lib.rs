//! Sakurasato core: shared domain types, configuration, and persistence.

#![forbid(unsafe_code)]

pub mod config;
pub mod model;
pub mod net_guard;
pub mod repo;
pub mod unicode_emoji;

pub use config::{Config, Listen};

/// Embed the workspace's `migrations/` directory so it ships with the binary.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[cfg(test)]
mod tests {
    #[test]
    fn migrator_embeds_expected_migrations() {
        // 0001 actor / 0002 note / 0003 follow / 0004 delivery_queue
        // 0005 emoji / 0006 reaction
        // 0007 actor_ed25519 (M3b: デュアル鍵)
        // 0008 api_token (M4: ローカル API Bearer 認証)
        // 0009 media (M7: アップロードメディアのメタデータ)
        // 0010 note_edited_at (M11: Update Note 受信時刻)
        // 0011 announce (M11: Boost 受信)
        // 0012 actor_manually_approves (M12 / Issue #66: 鍵アカフラグ)
        // 0013 notification_channel (Discord 互換 webhook 通知の宛先テーブル)
        assert_eq!(crate::MIGRATOR.migrations.len(), 13);
    }
}
