-- Sakurasato — in-app 通知フィード。
--
-- お一人様サーバの TUI / MiAuth (Aria 等 Misskey 互換クライアント) 双方で
-- 「自分宛のメンション / DM / 引用 / リアクション / リノート / フォロー (鍵アカ
-- 運用なら follow-request も)」を一覧表示するための通知本体テーブル。
--
-- 既存の `notification_channel` (0013) とは **別物**:
-- - `notification_channel` = 外部 Discord 等 webhook への push 宛先設定
-- - `notification` (本表) = サーバ内に貯める通知フィードそのもの
--
-- 生成は同じフック (`crate::notification::dispatch::notify`) に相乗りする ──
-- dispatch handler がイベント検知時に webhook enqueue と並行して本表へ 1 行
-- insert する。`NotificationContext` (notifier actor / 対象 note / reaction) を
-- そのまま列に落とす。recipient はお一人様なので常に local actor だが、actor
-- FK + 将来拡張のため明示的に持つ。

CREATE TABLE notification (
    id                  BIGSERIAL    PRIMARY KEY,
    -- 受信者 (= local user)。お一人様サーバでは常に同一だが actor FK で持つ。
    recipient_actor_id  BIGINT       NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- 通知種別。`NotificationEvent::as_str()` (snake_case) と一致:
    -- mention / direct / quote / reaction / renote / follow / follow_request
    event_type          TEXT         NOT NULL,
    -- 通知を起こした相手 (mention 送信者 / reactor / boost した人 / follower)。
    -- actor が purge されたら通知ごと消す (orphan を避ける)。
    notifier_actor_id   BIGINT       REFERENCES actor(id) ON DELETE CASCADE,
    -- 関連 note (reaction/renote/quote/mention/direct の対象)。follow 系は NULL。
    -- note 削除で通知ごと消す (= 参照先が無い通知は moot)。
    note_id             BIGINT       REFERENCES note(id) ON DELETE CASCADE,
    -- reaction の内容 (Unicode emoji or `:shortcode:` / `:shortcode@host:`)。
    -- reaction event のみ非 NULL。
    reaction            TEXT,
    is_read             BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- フィード一覧 (recipient ごとに id DESC で新しい順、sinceId/untilId ページング)。
CREATE INDEX idx_notification_recipient
    ON notification (recipient_actor_id, id DESC);

-- 未読数カウント / 未読のみ取得を高速化する partial index。
CREATE INDEX idx_notification_unread
    ON notification (recipient_actor_id)
    WHERE is_read = FALSE;
