-- Sakurasato M2 — reaction テーブル。
-- Misskey 形式の絵文字リアクション (`EmojiReact`)。Mastodon の `Like` も
-- 内部的にはここに 'unicode = ♥' のようなレコードとして格納する想定。

CREATE TABLE reaction (
    id         BIGSERIAL   PRIMARY KEY,
    -- EmojiReact / Like activity の AP id URI。Undo 対象指定に使う。
    ap_id      TEXT        NOT NULL UNIQUE,
    note_id    BIGINT      NOT NULL REFERENCES note(id)  ON DELETE CASCADE,
    actor_id   BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    -- content: 生の Unicode (例: "👍") か、Misskey 形式 ":shortcode@host:" / ":shortcode:" 文字列。
    content    TEXT        NOT NULL,
    -- カスタム絵文字を本サーバが知っている場合、emoji への FK。NULL 可。
    emoji_id   BIGINT      REFERENCES emoji(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Misskey 仕様: 1 (note, actor) に対して content 違いで複数リアクション可。
    UNIQUE (note_id, actor_id, content)
);

CREATE INDEX idx_reaction_note  ON reaction (note_id);
CREATE INDEX idx_reaction_actor ON reaction (actor_id);
