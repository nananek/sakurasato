//! `MissUser` の count 3 点 (followers / following / notes) の読み分け helper。
//!
//! `followersCount` / `followingCount` / `notesCount` は **actor の種別で出所が
//! 変わる**:
//!
//! - **local actor** (= 自分自身): `follow` テーブル / `note` テーブルの実クエリ
//!   (`repo::follow::count_followers` / `count_following` /
//!   `repo::note::count_local`) で正確な値を集計する。
//! - **remote actor**: `actor` テーブルの count キャッシュ
//!   (`followers_count` / `following_count` / `notes_count`) を素通しで使う。
//!   お一人様サーバはローカルの `follow` テーブルに remote actor の「真の
//!   フォロー関係」を持っていない (あるのは「自分がその remote をフォローして
//!   いるか」の 0 or 1 だけ) ので、`count_followers` 等で集計すると意味が違う
//!   値になってしまう。キャッシュは `crate::remote_actor::fetch_and_upsert_with_counts`
//!   (= `refresh_remote_actor_if_stale` 経由、MiAuth プロフィール表示専用) が
//!   相手インスタンスの `followers` / `following` / `outbox` Collection の
//!   `totalItems` から埋める (Mastodon / Misskey 共通パターン、自己申告値を
//!   信じる設計)。
//!
//! 表示の新鮮化 (= TTL ベースの on-demand 再 fetch) は呼び出し側で
//! `crate::remote_actor::refresh_remote_actor_if_stale` を先に走らせてから本
//! helper を呼ぶ設計。

use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;

use crate::state::AppState;

/// actor の count 3 点を `(followers, following, notes)` で返す。
///
/// `actor.is_local` で分岐する (module doc 参照)。DB 障害時は従来どおり
/// `.unwrap_or(0)` でフェイルオープンする。
pub async fn counts_for_actor(state: &AppState, actor: &ActorRow) -> (i64, i64, i64) {
    if actor.is_local {
        let followers = repo::follow::count_followers(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let following = repo::follow::count_following(state.pool(), actor.id)
            .await
            .unwrap_or(0);
        let notes = repo::note::count_local(state.pool()).await.unwrap_or(0);
        (followers, following, notes)
    } else {
        // remote: 相手インスタンスの自己申告値のキャッシュ。TTL 内なら fetch
        // は発生しない (呼び出し側が refresh_remote_actor_if_stale を先に呼ぶ)。
        // 取得に失敗して古い値のままでも fail-open で DB 値を返す。
        (
            actor.followers_count,
            actor.following_count,
            actor.notes_count,
        )
    }
}
