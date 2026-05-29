-- Sakurasato M2 — emoji テーブル。
-- カスタム絵文字 (local + remote)。Misskey 互換 zip インポート / EmojiReact /
-- AP tag 内 Emoji オブジェクトの保持に使う。

CREATE TABLE emoji (
    id           BIGSERIAL   PRIMARY KEY,
    -- shortcode (コロンなし、例: "blob_party")。
    -- zip-slip 相当の S3 キー注入を防ぐため、本体側 + DB の二重で文字種制限する。
    shortcode    TEXT        NOT NULL
                              CHECK (shortcode ~ '^[a-zA-Z0-9_-]{1,64}$'),
    -- host (NULL = 本サーバ所有)
    host         TEXT,
    -- カテゴリ (Misskey 互換)
    category     TEXT,
    -- 別名 (Misskey aliases)
    aliases      JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- versitygw 上のオブジェクトキー (例: "emoji/local/blob_party.png")
    image_key    TEXT        NOT NULL,
    -- 元画像の media type (例: "image/png", "image/webp")
    media_type   TEXT        NOT NULL,
    -- AP id URI (連合で取得した emoji 用)
    ap_id        TEXT        UNIQUE,
    is_local     BOOLEAN     NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- 同一 (shortcode, host) は同じ絵文字を指す。Misskey インポートで同名は上書きする。
    -- NULLS NOT DISTINCT (Postgres 15+) を指定しないと host=NULL 同士の重複が
    -- 検出されず、local 絵文字の上書きが新規行になってしまう。
    UNIQUE NULLS NOT DISTINCT (shortcode, host)
);

CREATE INDEX idx_emoji_local_shortcode ON emoji (shortcode) WHERE is_local;
