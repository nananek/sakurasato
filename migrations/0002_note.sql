-- Sakurasato M2 — note テーブル。
-- ActivityPub Note (投稿) を表す。本人の投稿と、フォロー先 actor から受信した
-- 投稿の両方を保持する。

CREATE TABLE note (
    id                  BIGSERIAL   PRIMARY KEY,
    ap_id               TEXT        NOT NULL UNIQUE,
    actor_id            BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- 本文 (HTML, sanitize 済み想定)
    content             TEXT        NOT NULL,
    -- 言語タグ (BCP 47, 例: "ja", "en")
    language            TEXT,
    -- 返信先 — AP URI と (取り込み済みなら) ローカル FK の両方を保持。
    in_reply_to_ap_id   TEXT,
    in_reply_to_note_id BIGINT      REFERENCES note(id) ON DELETE SET NULL,
    -- CW / spoiler / summary
    summary             TEXT,
    -- 公開範囲: 'public' / 'unlisted' / 'followers' / 'direct'
    visibility          TEXT        NOT NULL DEFAULT 'public',
    -- NSFW / Sensitive
    sensitive           BOOLEAN     NOT NULL DEFAULT FALSE,
    -- to / cc (URI 配列)。配送・受信フィルタに使う。
    to_recipients       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    cc_recipients       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- 添付 (画像/動画/音声などのメタデータ配列)
    attachments         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- mentions / hashtags / custom emojis (AP "tag")
    tags                JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- 本サーバ発信か
    is_local            BOOLEAN     NOT NULL,
    -- ブラウザ閲覧用 URL (パーマリンク)
    url                 TEXT,
    -- AP `published` (発信時刻)
    published_at        TIMESTAMPTZ NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_note_actor_published    ON note (actor_id, published_at DESC);
CREATE INDEX idx_note_in_reply_to_note   ON note (in_reply_to_note_id);
CREATE INDEX idx_note_published_at       ON note (published_at DESC);
