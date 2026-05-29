-- Sakurasato M2 — actor テーブル。
-- ActivityPub actor を表す。お一人様サーバの本人 (is_local = TRUE) と
-- instance.actor (application actor)、および外部の remote actor を全て保持する。

CREATE TABLE actor (
    id                 BIGSERIAL PRIMARY KEY,
    -- ActivityPub `id` URI (canonical identifier)。
    ap_id              TEXT        NOT NULL UNIQUE,
    -- preferredUsername。host との組み合わせで acct: として一意。
    preferred_username TEXT        NOT NULL,
    -- ホスト (例: "example.com")。local actor は本サーバの host を入れる。
    host               TEXT        NOT NULL,
    -- 表示名 (`name`)
    display_name       TEXT,
    -- 自己紹介 (HTML or plain)
    summary            TEXT,
    -- アイコン (avatar) URL — 表示時は media-proxy 経由でフェッチする想定。
    icon_url           TEXT,
    -- ヘッダ画像 URL
    image_url          TEXT,
    -- 配送・受信エンドポイント
    inbox_url          TEXT        NOT NULL,
    shared_inbox_url   TEXT,
    outbox_url         TEXT,
    followers_url      TEXT,
    following_url      TEXT,
    -- 公開鍵 (PEM)。HTTP 署名検証用。
    public_key_id      TEXT        NOT NULL UNIQUE,
    public_key_pem     TEXT        NOT NULL,
    -- 秘密鍵 (PEM)。local actor のみ非 NULL。CLI で生成・厳重管理。
    private_key_pem    TEXT,
    -- alsoKnownAs (引っ越し元 actor の URI 配列)
    also_known_as      JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- movedTo — 引っ越し先 actor URI (引っ越し済みの場合)
    moved_to_ap_id     TEXT,
    -- 本サーバ所属か (TRUE: 本人 / instance.actor, FALSE: remote)
    is_local           BOOLEAN     NOT NULL,
    -- AP type: "Person" / "Service" / "Application" など
    actor_type         TEXT        NOT NULL DEFAULT 'Person',
    -- 最終フェッチ時刻 (remote actor 用)
    fetched_at         TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX idx_actor_username_host ON actor (preferred_username, host);
CREATE INDEX idx_actor_is_local            ON actor (is_local) WHERE is_local;
