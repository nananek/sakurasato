//! `MissUser` 変換層 (= M14 #158, 親 issue #150)。
//!
//! Sakurasato の [`ActorRow`] + 集計 count を Misskey 互換クライアントが
//! 期待する **`MissUser` (`UserLite` + `UserDetailed` 一部)** の JSON 形に変換する。
//!
//! ## AGPL discipline
//!
//! 本変換は [misskey-hub.net](https://misskey-hub.net/) の **公開 API 仕様** と
//! [api-doc.misskey.io](https://api-doc.misskey.io/) `OpenAPI` を一次資料とし、
//! Misskey の TypeScript handler は読まずに書く clean-room 実装 (=
//! [[agpl-discipline-miauth]] / [`crate::miauth`] module doc 参照)。フィールド
//! 名 / 型は **interface = 著作権対象外** (Oracle v Google) なので翻訳問題は
//! 起きない。
//!
//! ## #158 で返す `MissUser` の最小スコープ
//!
//! 親 issue #158 acceptance criteria より:
//!
//! ```text
//! id, name, username, host, avatarUrl, isLocked,
//! followersCount, followingCount, notesCount
//! ```
//!
//! ── Misskey の `UserLite` + `UserDetailed` の **必須フィールド** だけを抜き
//! 出した形。Milktea / `MissRirica` 等は他にも optional フィールド (= `avatarBlurhash`
//! / `bannerUrl` / `emojis` / `createdAt` 等) を読み得るが、Misskey 公式 JSON
//! schema 上いずれも **nullable** / **optional** なので、本 PR では必須フィー
//! ルドのみで「クライアントが panic しない最低限」を成立させる。Optional フィ
//! ールドは #159 (read endpoints) で `MissNote` / `MissEmoji` を追加する際に同
//! タイミングで揃える計画。
//!
//! ## ID 形式の差異
//!
//! Misskey の `id` は **string** (= `aidx` フォーマットの 16 文字 ULID-like)。
//! Sakurasato の `actor.id` は `BIGSERIAL` (= `i64`)。**クライアントは id を
//! opaque string として扱う** (= `AmazonOAuth` と同じ思想で「内部数値か文字列か
//! は問わず、サーバが返した値をそのまま echo」する) ので、本 PR では
//! `format!("{i64}")` で stringify した値を返す。Misskey 側と完全一致はしない
//! が **型一致** (= string) は維持する。
//!
//! parity test (`tests/federation/test_miauth_flow_parity.py`) は「`id` が
//! string であること + 同 client から見て後続 `/api/users/show?userId=<id>`
//! が同じ user を返すこと」までを検証し、**値の bit 同一性は要求しない**
//! (= 別 instance の actor を比較するので当然違う)。
//!
//! ## host の扱い
//!
//! Misskey は **local user に対しては `host: null`** を返す (= `UserLite` 仕様)。
//! Sakurasato でも `is_local == true` のとき `host: null` に倒す ── お一人様
//! サーバ前提で `/api/i` は常に local actor を返すため、実質常に null。
//! remote actor を返す経路 (= #159 で `users/show` を生やすとき) は host を
//! `Some(actor.host)` に倒す。

use serde::Serialize;

use sakurasato_core::model::ActorRow;

/// `MissUser` (Misskey 互換) の最小サブセット。
///
/// `#[serde(rename_all = "camelCase")]` で `is_locked` → `isLocked` /
/// `followers_count` → `followersCount` のように JSON フィールド名を Misskey
/// 慣行に合わせる。`null` 表現は `Option<T>` (= Misskey 仕様の `nullable`)。
///
/// `Option<String>` フィールドは `serde` 既定で **null として出力** される
/// (= `skip_serializing_if = "Option::is_none"` は付けない)。Misskey の
/// `UserLite` spec で `name` / `host` / `avatarUrl` は **`null` 明示が必須** で、
/// `omitted` (= フィールドごと消える) は許容しないため。これは `serde_json`
/// のデフォルト挙動と一致する。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissUser {
    /// `actor.id` を stringify した値 ── Misskey は string、Sakurasato 内部は
    /// `i64` なので `format!("{}", id)` で変換する。後方互換性の観点では
    /// `i64` 直渡しも考えられるが、Misskey クライアントは string 前提で parse
    /// するので **型一致** のためにも string が正解。
    pub id: String,
    /// display name (= Sakurasato `actor.display_name`)。未設定なら `null`。
    pub name: Option<String>,
    /// `actor.preferred_username` (= local 部分のみ、`@` も `host` も含まない)。
    pub username: String,
    /// local user は `null`、remote user は `Some(host)`。詳細は module doc。
    pub host: Option<String>,
    /// `actor.icon_url`。未設定なら `null` (= Misskey の `avatarUrl` は
    /// クライアント側で default avatar に倒される)。
    pub avatar_url: Option<String>,
    /// `actor.manually_approves_followers` (= 鍵アカウント, #66 / M12)。
    /// Misskey の `isLocked` 慣行と完全に同義。
    pub is_locked: bool,
    /// `follow` テーブルの `state = 'accepted' AND followed = me` の件数。
    pub followers_count: i64,
    /// `follow` テーブルの `state = 'accepted' AND follower = me` の件数。
    pub following_count: i64,
    /// `note` テーブルの `is_local = TRUE` の件数 (お一人様サーバ前提で
    /// 「自分の投稿数」と同義)。
    pub notes_count: i64,
}

/// Sakurasato の [`ActorRow`] + 集計 count から `MissUser` を組み立てる。
///
/// 呼び出し側 (= [`crate::miauth::i`] handler) は `repo::actor::get_by_*` +
/// `repo::follow::count_*` + `repo::note::count_local` を直接叩いて値を集めて
/// から本関数に渡す。本関数はアロケーションだけで I/O を持たない (= unit test
/// で DB を立てずに変換ロジックだけ検証可能)。
pub fn from_actor_and_counts(
    actor: &ActorRow,
    followers_count: i64,
    following_count: i64,
    notes_count: i64,
) -> MissUser {
    MissUser {
        id: actor.id.to_string(),
        name: actor.display_name.clone(),
        username: actor.preferred_username.clone(),
        // local user は host を `null` で返す ── Misskey UserLite spec。
        host: if actor.is_local {
            None
        } else {
            Some(actor.host.clone())
        },
        avatar_url: actor.icon_url.clone(),
        is_locked: actor.manually_approves_followers,
        followers_count,
        following_count,
        notes_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sakurasato_core::model::ActorRow;
    use sqlx::types::Json as SqlxJson;

    fn fake_actor(is_local: bool, host: &str, locked: bool) -> ActorRow {
        ActorRow {
            id: 42,
            ap_id: format!("https://{host}/users/me"),
            preferred_username: "me".into(),
            host: host.into(),
            display_name: Some("Alice".into()),
            summary: None,
            icon_url: Some("https://cdn.test/avatar.webp".into()),
            image_url: None,
            inbox_url: format!("https://{host}/users/me/inbox"),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: "k".into(),
            public_key_pem: "p".into(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: SqlxJson(vec![]),
            moved_to_ap_id: None,
            is_local,
            actor_type: "Person".into(),
            manually_approves_followers: locked,
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// local actor の `host` は **null** で返る (Misskey `UserLite` 慣行)。
    #[test]
    fn local_actor_host_is_null() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 3, 5, 7);
        assert_eq!(miss.id, "42");
        assert_eq!(miss.username, "me");
        assert_eq!(miss.name.as_deref(), Some("Alice"));
        assert_eq!(miss.host, None, "local actor must serialize host as null");
        assert_eq!(miss.followers_count, 3);
        assert_eq!(miss.following_count, 5);
        assert_eq!(miss.notes_count, 7);
        assert!(!miss.is_locked);
    }

    /// remote actor の `host` は `Some(host)` で返る (将来 #159 で使う経路)。
    #[test]
    fn remote_actor_host_is_some() {
        let actor = fake_actor(false, "remote.test", false);
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        assert_eq!(miss.host.as_deref(), Some("remote.test"));
    }

    /// 鍵アカウント (`manually_approves_followers = true`) は `isLocked: true`。
    #[test]
    fn locked_actor_serializes_as_locked() {
        let actor = fake_actor(true, "sakurasato", true);
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        assert!(miss.is_locked);
    }

    /// camelCase + null 表現が Misskey wire spec と一致することを serde で確認。
    #[test]
    fn json_shape_matches_misskey_userlite_minimum() {
        let actor = fake_actor(true, "sakurasato", false);
        let miss = from_actor_and_counts(&actor, 11, 12, 13);
        let json = serde_json::to_value(&miss).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["username"], "me");
        assert_eq!(json["name"], "Alice");
        assert!(json["host"].is_null(), "host must be JSON null, not omitted");
        assert_eq!(json["avatarUrl"], "https://cdn.test/avatar.webp");
        assert_eq!(json["isLocked"], false);
        assert_eq!(json["followersCount"], 11);
        assert_eq!(json["followingCount"], 12);
        assert_eq!(json["notesCount"], 13);
    }

    /// `display_name` / `icon_url` が未設定なら **null** で出力される
    /// (omitted ではなく)。Milktea は `name === null` でフォールバック表示
    /// する。
    #[test]
    fn optional_fields_serialize_as_null_when_missing() {
        let mut actor = fake_actor(true, "sakurasato", false);
        actor.display_name = None;
        actor.icon_url = None;
        let miss = from_actor_and_counts(&actor, 0, 0, 0);
        let json = serde_json::to_value(&miss).unwrap();
        assert!(json["name"].is_null());
        assert!(json["avatarUrl"].is_null());
    }
}
