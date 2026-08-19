-- Sakurasato — domain_moderation テーブル。
-- ドメイン (host) 単位のモデレーション状態。行が存在しない = 通常運用。
-- 'silence' = 配信停止相当 (新規 inbound インタラクションを拒否、既存関係は維持)。
-- 'suspend' = 完全ブロック (inbox 受信拒否 + 配送停止 + 既存フォロー関係を強制解除)。

CREATE TABLE domain_moderation (
    id           BIGSERIAL   PRIMARY KEY,
    host         TEXT        NOT NULL UNIQUE,
    severity     TEXT        NOT NULL
                              CHECK (severity IN ('silence', 'suspend')),
    reason       TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_domain_moderation_severity ON domain_moderation (severity);
