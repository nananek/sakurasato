-- Sakurasato Issue #188 — emoji shortcode の長さ上限を Misskey に揃える (64 → 128)。
--
-- 旧 migration 0005_emoji.sql は `^[a-zA-Z0-9_-]{1,64}$` だったが、Misskey の
-- emoji shortcode は実装上 128 文字まで許容されるため、Misskey 互換 zip pack や
-- リモート EmojiReact の `tag.Emoji.name` が 65〜128 文字長の shortcode を
-- 持っている場合に拒否してしまう (= import 失敗 / emoji 学習失敗)。
--
-- 文字種は変更せず `[a-zA-Z0-9_-]` のまま維持する ── Misskey は `-` を含まない
-- 仕様 (`^[a-zA-Z0-9_]+$`) だが、既存ローカル絵文字 (例: `blob-smile`) や
-- 既存 row が CHECK 違反になるため、互換性のため `-` 許可は意図的に残す。
--
-- 既存 row はすべて新制約 `{1,128}` の真部分集合 (= 1..=64 ⊂ 1..=128) なので
-- データ修復は不要。CHECK 制約の DROP → ADD だけで足りる。
--
-- 旧 CHECK 制約名は Postgres が `<table>_<column>_check` 形 (= `emoji_shortcode_check`)
-- で自動命名する想定だが、`pg_constraint` を引いて動的に検出する ── 過去
-- migration で別名が振られている / 将来 rename された場合のためのフェイル
-- セーフ。`shortcode` 列に乗っている CHECK 制約は同列の正規表現マッチ 1 つ
-- だけなので、`conkey = ARRAY[<shortcode 列 attnum>]` で一意に絞れる。

DO $$
DECLARE
    shortcode_attnum SMALLINT;
    existing_check_name TEXT;
BEGIN
    SELECT a.attnum
    INTO shortcode_attnum
    FROM pg_attribute a
    WHERE a.attrelid = 'emoji'::regclass
      AND a.attname = 'shortcode';

    SELECT c.conname
    INTO existing_check_name
    FROM pg_constraint c
    WHERE c.conrelid = 'emoji'::regclass
      AND c.contype = 'c'
      AND c.conkey = ARRAY[shortcode_attnum]
    LIMIT 1;

    IF existing_check_name IS NULL THEN
        RAISE EXCEPTION 'expected an existing CHECK constraint on emoji.shortcode (from 0005_emoji.sql) but found none';
    END IF;

    EXECUTE format('ALTER TABLE emoji DROP CONSTRAINT %I', existing_check_name);
END
$$;

ALTER TABLE emoji
    ADD CONSTRAINT emoji_shortcode_check
    CHECK (shortcode ~ '^[a-zA-Z0-9_-]{1,128}$');
