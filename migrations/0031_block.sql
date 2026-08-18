-- Sakurasato — block テーブル。
-- ユーザーブロック関係を表す。follow テーブルと対称の設計。
-- blocker が blocked をブロックしている、という有向関係。
-- お一人様サーバでは実運用上ほぼ (local, remote) か (remote, local) のどちらか。

CREATE TABLE block (
    id                BIGSERIAL   PRIMARY KEY,
    -- Block activity の AP id URI。Undo Block の対象指定に使う。
    -- outbound (自分が送った Block) は自前で決定論的 id を振る。
    -- inbound (相手から届いた Block) は相手が振った id をそのまま保存する。
    ap_id             TEXT        NOT NULL UNIQUE,
    blocker_actor_id  BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    blocked_actor_id  BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (blocker_actor_id, blocked_actor_id)
);

CREATE INDEX idx_block_blocker ON block (blocker_actor_id);
CREATE INDEX idx_block_blocked ON block (blocked_actor_id);
