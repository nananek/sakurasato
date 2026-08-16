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
        // 0023 restore_remote_reaction_host (= Issue #242: remote custom emoji の
        //   reaction content に `@host` を復元する backfill。0021 が真リモート絵文字
        //   まで `:foo:` に畳んで Aria でローカル非保有 shortcode が描画できなくなった
        //   のを is_local=FALSE 行に限り `:shortcode@host:` へ戻す)
        // 0024 emoji_license_sensitive (= 絵文字 metadata の round-trip export 用に
        //   emoji.license TEXT + emoji.is_sensitive BOOLEAN を追加)
        // 0025 user_list (= リスト機能: user_list + user_list_member テーブルを追加)
        // 0026 video_media (= 動画添付対応: media.duration_ms / poster_storage_key を追加)
        // 0027 actor_birthday (= MiAuth `i/update` の `birthday` フィールド用に
        //   actor.birthday TEXT を追加。"YYYY-MM-DD" ISO 日付文字列)
        // 0028 actor_profile_fields (= MiAuth `i/update` の location/lang/
        //   followedMessage/fields 用に actor へ 4 列追加。fields は JSONB 配列)
        // 0029 actor_remote_counts (= remote actor の followers/following/outbox
        //   Collection `totalItems` キャッシュ 3 列を actor に追加。MiAuth の
        //   `/api/users/show` が Aria プロフィールの followersCount/
        //   followingCount/notesCount に出す値の出所)
        // 0030 note_source (= MFM ソース列。ローカル投稿の生本文を AP
        //   `Note.source` / `_misskey_content` として配送するために保存する)
        assert_eq!(crate::MIGRATOR.migrations.len(), 30);
    }
}
