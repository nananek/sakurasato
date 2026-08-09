-- Sakurasato: remote actor の count キャッシュ (followers / following / outbox
-- Collection の `totalItems`)。
--
-- MiAuth (`/api/users/show` 等) で Aria 等のプロフィール画面に出す
-- `followersCount` / `followingCount` / `notesCount` は、remote actor に対しては
-- 相手インスタンスの `followers` / `following` / `outbox` Collection を `GET` し
-- て `totalItems` を読んだ値をキャッシュする (Mastodon / Misskey 共通の実装
-- パターン。`fetch_and_upsert` が取得時に並行して埋める)。
--
-- local actor (= 自分自身) は既存どおり `follow` テーブル / `note` テーブルの
-- 実クエリで集計するため、このカラムは remote actor 専用 (local は 0 のまま)。
-- `NOT NULL DEFAULT 0` ── カラム追加時の既存行も 0 で埋まる (= 古い remote
-- actor は次回の fetch で正しい値に更新される)。

ALTER TABLE actor
    ADD COLUMN followers_count BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN following_count BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN notes_count BIGINT NOT NULL DEFAULT 0;
