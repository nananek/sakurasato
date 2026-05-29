-- Sakurasato M2 — follow テーブル。
-- 双方向 (本人 → remote、remote → 本人) の Follow 関係を表す。
-- AP の Follow activity 単位で 1 行入り、Accept/Reject/Undo で state を遷移する。

CREATE TABLE follow (
    id                BIGSERIAL   PRIMARY KEY,
    -- Follow activity の AP id URI。Undo Follow の対象指定に使う。
    ap_id             TEXT        NOT NULL UNIQUE,
    follower_actor_id BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    followed_actor_id BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- 'pending' (相手の Accept 待ち) / 'accepted' / 'rejected'
    state             TEXT        NOT NULL DEFAULT 'pending'
                                   CHECK (state IN ('pending', 'accepted', 'rejected')),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (follower_actor_id, followed_actor_id)
);

CREATE INDEX idx_follow_follower ON follow (follower_actor_id);
CREATE INDEX idx_follow_followed ON follow (followed_actor_id);
CREATE INDEX idx_follow_state    ON follow (state);
