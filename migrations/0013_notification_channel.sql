-- Sakurasato — Discord 互換 webhook 通知チャンネル。
--
-- お一人様サーバから自分用 Discord (および Slack / Misskey 互換 webhook) に
-- push 通知するための宛先テーブル。Web UI を持たないサーバなので、外出中に
-- 「自分宛のメンション / DM / 引用 / リアクション / リノート / フォロー (鍵
-- アカ運用なら follow-request も)」を受け取ったことを知る経路として用意する。
--
-- 配送は既存 delivery_queue を流用する設計:
-- - delivery_queue.activity (JSONB) の "type" 値を "Webhook:Discord" /
--   "Webhook:Plain" に倒して書き込む
-- - delivery worker は "Webhook:" prefix を見たら ActivityPub の HTTP 署名を
--   skip し、`payload` サブツリーを `application/json` で POST する
-- - net_guard::host_blocked は通常経路と同じく適用 (= TOCTOU 防御の本丸)
--
-- これにより新規 queue 表を作らず、retry / backoff / dead 状態機械もすべて
-- 既存の delivery_queue / worker と共有できる。
--
-- 列設計:
-- - `enabled` は master switch。チャンネルごとに全イベントを一括 on/off する
--   ため (例: webhook 先 Discord が一時 down のとき)。個別 `notify_*` も残す
--   ことで「mention だけ落として静かにする」運用も可能。
-- - `format = 'embed'` は Discord 互換 embed (色 / author / description /
--   footer)。`format = 'plain'` は `{"content": "..."}` のみで Slack の `text`
--   フィールドとも互換に倒せる fallback。
-- - 個別 `notify_*` は default TRUE で「とりあえず全イベント通知」をベースに
--   する。個別 off は `notification-channel toggle --event ...` で。

CREATE TABLE notification_channel (
    id                       BIGSERIAL    PRIMARY KEY,
    -- CLI / 表示用ラベル (例: "discord-personal")。UNIQUE で同名重複を弾く。
    name                     TEXT         NOT NULL UNIQUE,
    -- Webhook URL。public IP / 公開ドメインのみ。private / loopback /
    -- link-local / reserved は net_guard::host_blocked で配送時に遮断される。
    url                      TEXT         NOT NULL,
    -- 'embed' (Discord) または 'plain' (Slack / Misskey 互換 fallback)。
    format                   TEXT         NOT NULL DEFAULT 'embed'
                                          CHECK (format IN ('embed', 'plain')),
    -- master switch。FALSE のときは個別 notify_* に関わらず通知を発火しない。
    enabled                  BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_mention           BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_direct            BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_quote             BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_reaction          BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_renote            BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_follow            BOOLEAN      NOT NULL DEFAULT TRUE,
    notify_follow_request    BOOLEAN      NOT NULL DEFAULT TRUE,
    created_at               TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- enabled = TRUE な行だけ取り出す高頻度 path (notify が dispatch ごとに走る)
-- を高速化する partial index。
CREATE INDEX idx_notification_channel_enabled
    ON notification_channel (enabled)
    WHERE enabled = TRUE;
