-- Sakurasato M11 — announce テーブル。
-- リモートからの Boost (`Announce`) を受け取って、誰がいつどの Note を
-- boost したかを記録する。
--
-- ## Design notes
--
-- - reaction テーブル拡張ではなく独立テーブルにする ── boost は「賛意」と
--   「タイムライン上の再放送」の二側面があり、TUI の表現も別個になる予定
--   (= reaction の集計 UI とは別レーンで表示する)。
-- - `(note_id, actor_id)` UNIQUE: 同じ actor が同じ note を二度 boost
--   できないように。Misskey / Mastodon も idempotent 設計。
-- - `note_id` は既知ローカルレコードへの FK。未知 Note の Announce は
--   handler 層で no-op し、ここには insert しない (= 「相手の Boost で
--   見知らぬ note を引き込まない」M11 設計判断)。Note を引き取るかは
--   将来の独立 issue で再評価する。
-- - `ap_id` UNIQUE: Undo Announce で対象を引くための識別子。
CREATE TABLE announce (
    id           BIGSERIAL   PRIMARY KEY,
    ap_id        TEXT        NOT NULL UNIQUE,
    note_id      BIGINT      NOT NULL REFERENCES note(id)  ON DELETE CASCADE,
    actor_id     BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    published_at TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (note_id, actor_id)
);

CREATE INDEX idx_announce_note  ON announce (note_id);
CREATE INDEX idx_announce_actor ON announce (actor_id);
