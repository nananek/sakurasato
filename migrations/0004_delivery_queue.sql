-- Sakurasato M2 — delivery_queue テーブル。
-- 本サーバから外部 inbox へ送出する activity をキューする。指数バックオフで
-- リトライし、最大試行回数を超えたものは 'dead' に倒す。

CREATE TABLE delivery_queue (
    id              BIGSERIAL   PRIMARY KEY,
    -- 送出先 inbox URL
    inbox_url       TEXT        NOT NULL,
    -- 署名前の activity JSON。送出ワーカが HTTP 署名を載せる。
    activity        JSONB       NOT NULL,
    -- 署名鍵を持つ actor (本サーバの local actor)。
    sender_actor_id BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- 試行回数 (失敗ごとに increment)。暴走防止に絶対上限を貼っておく。
    attempts        INTEGER     NOT NULL DEFAULT 0
                                 CHECK (attempts >= 0 AND attempts <= 30),
    -- 次回試行可能になる時刻 (指数バックオフ計算結果)
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- 直近の失敗理由 (デバッグ用)
    last_error      TEXT,
    -- 'pending' / 'delivered' / 'failed' (一時失敗、再試行待ち) / 'dead' (最大試行超過)
    state           TEXT        NOT NULL DEFAULT 'pending'
                                 CHECK (state IN ('pending', 'delivered', 'failed', 'dead')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ワーカが「次に走らせるべき行」を 1 トランザクションで取得するための索引。
CREATE INDEX idx_delivery_queue_pickup
    ON delivery_queue (state, next_attempt_at)
    WHERE state IN ('pending', 'failed');
