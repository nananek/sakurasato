-- 0024 emoji_license_sensitive
-- 絵文字メタデータの round-trip export 対応 (= 他サーバが import するときに
-- license / sensitive まで拾えるように)。
--   license      = AP `_misskey_license.freeText` / Misskey zip meta.json の
--                  emojis[].emoji.license。NULL 可 (= 明示ライセンス無し)。
--   is_sensitive = Misskey `isSensitive`。NOT NULL DEFAULT FALSE。
-- 既存行は license=NULL / is_sensitive=FALSE で backfill。同名 zip の再 import
-- (= 上書き) で値が入る (Issue #134 の variant 再生成と同じ運用)。
-- Postgres 11+ は nullable / DEFAULT 付き ADD COLUMN を table rewrite 無しで行う。
ALTER TABLE emoji ADD COLUMN license TEXT;
ALTER TABLE emoji ADD COLUMN is_sensitive BOOLEAN NOT NULL DEFAULT FALSE;
