-- Sakurasato M7 — media テーブル。
-- TUI から POST /api/v1/media でアップロードされ、media-proxy でサニタイズ
-- (= 再エンコードで EXIF / 埋め込みペイロード除去) されたうえで versitygw に
-- 格納された画像のメタデータを保持する。
--
-- 後段の利用先:
--   * `actor.icon_url` / `actor.image_url` ─ アバター / ヘッダとして
--     Update Activity で連合送出する (M7 PR1)。
--   * `note.attachments` JSONB ─ 投稿の添付 (M7 PR1 で attachment 紐付け)。
--
-- ライフサイクル:
--   * `owner_actor_id` は所有 actor (= local actor)。複数 actor を抱えない
--     お一人様サーバだが、外部キーで整合性を担保しておく。
--   * `note_id` は添付として紐付いた Note (= 添付目的でアップロードされ、
--     POST /api/v1/notes に attachment_ids として渡された行)。NULL は
--     「未紐付け」(プレビュー専用、アイコン用、孤児)。
--   * アイコン / ヘッダ用は actor 側が `icon_url` / `image_url` を URL で
--     持つだけで、media 行とは直接結びつけない (差し替え時の trail を
--     残せるよう「使った後の media は孤児で残る」設計)。後の milestone で
--     `purge` CLI を用意する想定。

CREATE TABLE media (
    id              BIGSERIAL   PRIMARY KEY,
    -- versitygw 上のキー (例: `media/<sha256>.webp`)。GET /media/{key} で
    -- 配信する。UNIQUE: 同一バイト列の重複アップロードは新規行を作らず
    -- 既存行を返す設計 (ハンドラ側で実装)。
    storage_key     TEXT        NOT NULL UNIQUE,
    -- 常に `image/webp` (media-proxy が再エンコード後の値)。将来の動画
    -- 対応に備えて TEXT。
    media_type      TEXT        NOT NULL,
    -- 出力寸法 (= media-proxy が再エンコード後)。NULL になることは無いが、
    -- 念のため SIGNED INT のままにする。
    width           INTEGER     NOT NULL CHECK (width > 0),
    height          INTEGER     NOT NULL CHECK (height > 0),
    byte_size       BIGINT      NOT NULL CHECK (byte_size > 0),
    -- 用途タグ: 'avatar' / 'header' / 'attachment'。
    -- media-proxy の Variant とは独立。例: TUI からの添付は variant=preview
    -- (1280x1280) で sanitize するが、用途タグは 'attachment'。
    kind            TEXT        NOT NULL
                                CHECK (kind IN ('avatar', 'header', 'attachment')),
    -- 添付の代替テキスト (a11y)。AP `Document.name` に流す。
    alt_text        TEXT,
    -- 所有 actor。お一人様サーバなので事実上 1 値だが、外部キーで縛る。
    owner_actor_id  BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- 添付として紐付いた Note。NULL = 未紐付け。
    note_id         BIGINT      REFERENCES note(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_media_owner_created    ON media (owner_actor_id, created_at DESC);
CREATE INDEX idx_media_note             ON media (note_id) WHERE note_id IS NOT NULL;
