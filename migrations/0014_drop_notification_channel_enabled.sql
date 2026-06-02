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
-- 既存行は 0 件 (本機能は 0f3e646 で導入されて以降 prod に登録なし) のため
-- データ救済不要。万一 `enabled = FALSE` で master OFF にしていた行があれば
-- `notify_*` がすべて TRUE のまま再有効化される (= 通知が出るようになる)
-- 副作用がある点だけ DOWN 不能のリスクとして記録しておく。

ALTER TABLE notification_channel DROP COLUMN enabled;

-- master 用 partial index も同時撤去。`list_enabled_for_event` の WHERE 句は
-- `enabled = TRUE AND notify_<event> = TRUE` から `notify_<event> = TRUE` に
-- 簡略化される。
DROP INDEX IF EXISTS idx_notification_channel_enabled;
