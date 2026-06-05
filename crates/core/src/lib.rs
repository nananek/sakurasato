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
        // 0014 drop notification_channel.enabled (master 撤去 / `--event all`
        //   と `toggle` の混乱を解消)
        // 0015 miauth (= 親 issue #150 / M14 #157: MiAuth 互換 API endpoint の
        //   foundation。miauth_session + miauth_token を一括追加。実 endpoint
        //   は #158 以降で乗る)
        // 0016 miauth_session_raw_token (= M14 #158: check polling で raw token
        //   を冪等返却するための列追加)
        // 0017 emoji_shortcode_128 (= Issue #188: shortcode CHECK 制約の長さ
        //   上限を 64 → 128 に緩和、Misskey に揃え)
        // 0018 emoji_image_key_nullable (= Issue #135: remote emoji 取得失敗を
        //   表現するため image_key 列を NULL 許容化)
        // 0019 emoji_last_failed_at (= Issue #192: fetch 失敗 row への backoff
        //   と旧 URL row の regression 回避用に last_failed_at TIMESTAMPTZ NULL を追加)
        // 0020 notification (= #206 PR1: in-app 通知フィード本体テーブル。
        //   webhook 宛先の notification_channel とは別物、TUI / MiAuth が一覧する)
        // 0021 normalize_reaction_content (= reaction shortcode mismatch 修正:
        //   PR #183/#187 以前に書かれた `:foo@host:` stale 行を `:foo:` に畳む
        //   backfill。Aria でのリアクション絵文字「増殖」を解消)
        // 0022 normalize_note_summary (= Pleroma の `summary: ""` を保存した
        //   stale 行の空 summary を NULL に畳む backfill。Aria で全ノートが
        //   「警告文の無い CW」に見える症状を解消)
        assert_eq!(crate::MIGRATOR.migrations.len(), 22);
    }
}
