-- Sakurasato — リスト機能 (Mastodon/Misskey 互換の user list)。
--
-- フォロー中ユーザーをグループ化し、専用タイムラインとして閲覧するための
-- 機能。Misskey MiAuth 経路 (`users/lists/*` + `notes/user-list-timeline`)
-- と TUI ローカル API 経路の両方から同じスキーマを参照する。
--
-- お一人様サーバなので `owner_actor_id` は持たない (= `api_token` /
-- `miauth_token` と同じ設計判断。所有者は常に唯一の local actor)。
--
-- メンバー追加は「`follow.state = 'accepted'` の相手のみ」という制約を
-- アプリ層 (`repo::user_list::add_member`) で保証する。DB CHECK にしない
-- 理由は `follow` 側の state 変化 (unfollow 等) を本テーブルの CHECK では
-- 追跡できないため。フォロー解除後も既存メンバーは残す (Mastodon 準拠)。

CREATE TABLE user_list (
    id         BIGSERIAL   PRIMARY KEY,
    title      TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_list_member (
    list_id         BIGINT      NOT NULL REFERENCES user_list(id) ON DELETE CASCADE,
    member_actor_id BIGINT      NOT NULL REFERENCES actor(id) ON DELETE CASCADE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (list_id, member_actor_id)
);

-- リストタイムラインのクエリ (`WHERE member_actor_id IN (SELECT list_id = $1
-- ...)`) は `list_id` (PK の先頭列) で足りるため、本 index は逆引き
-- (「この actor がどのリストに入っているか」) 用。
CREATE INDEX idx_user_list_member_actor ON user_list_member (member_actor_id);
