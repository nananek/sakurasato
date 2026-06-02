-- Sakurasato — Discord 互換 webhook 通知チャンネルから master `enabled` 列を撤去。
--
-- 当初 `enabled` (master) と `notify_<event>` (個別) を AND する 2 段スイッチで
-- 設計したが、CLI で `notification-channel toggle --event all` が直感に反する挙動
-- (`all` が「全 event を一斉に反転」ではなく「master を反転」) になり、
-- かつ `toggle` は冪等でないため運用しづらかった。
--
-- 新設計は master を撤去して 7 個の `notify_<event>` だけにする。CLI は
-- `enable` / `disable` のみで idempotent。`--event all` は 7 列を一斉セット
-- する直感どおりの意味になる。
--
-- 既存行は 0 件想定 (本機能は 0f3e646 で導入されて以降 prod に登録なし)
-- だが、test DB や cherry-pick 環境では `enabled = FALSE` の master OFF 行
-- が存在し得る。`enabled` を撤去するだけだと「master OFF だったので静か
-- だったチャンネル」が `notify_*` 全 TRUE のまま再有効化されて通知爆発
-- する事故になる。これを防ぐ corrective UPDATE を DROP の前に流す:
-- `enabled = FALSE` の行は 7 個の `notify_*` を一斉 FALSE に倒し、「黙ら
-- せたい」というユーザ意図を保つ。
--
-- 影響なしのケース (= prod) では UPDATE は 0 行 affected で no-op。
UPDATE notification_channel
SET
    notify_mention        = FALSE,
    notify_direct         = FALSE,
    notify_quote          = FALSE,
    notify_reaction       = FALSE,
    notify_renote         = FALSE,
    notify_follow         = FALSE,
    notify_follow_request = FALSE,
    updated_at            = now()
WHERE enabled = FALSE;

ALTER TABLE notification_channel DROP COLUMN enabled;

-- master 用 partial index も同時撤去。`list_enabled_for_event` の WHERE 句は
-- `enabled = TRUE AND notify_<event> = TRUE` から `notify_<event> = TRUE` に
-- 簡略化される。
DROP INDEX IF EXISTS idx_notification_channel_enabled;
