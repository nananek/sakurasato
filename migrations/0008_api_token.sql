-- Sakurasato M4 PR1 — api_token テーブル。
-- ローカル API (Unix socket REST + SSE) の Bearer 認証用トークン。
--
-- CLAUDE.md §5.1: 「ソケットのファイルパーミッションが認証境界」 + CLI
-- でのトークン発行。ソケットの mode 0600 で第一の壁を作りつつ、複数の
-- TUI クライアント (ホスト端末 / 別マシンからの SSH 転送等) を発行単位で
-- 識別・取り消し可能にするため Bearer トークンを併用する。
--
-- 設計メモ:
-- - `token_hash` は **生トークンの SHA-256 (hex)** を格納。生トークンは
--   発行時に 1 回だけ stdout に出し、以降は再表示できない。DB 漏洩しても
--   生トークンは復元できない。
-- - `name` は人間可読ラベル (例: "tui-laptop")。同名の重複は許可する
--   ── 同じ TUI を再発行する際に古い行を残せるようにしておく。`name` の
--   一意制約は付けない。
-- - 失効 (revoke) は当面 `DELETE FROM api_token WHERE id = $1` で済ます。
--   `revoked_at` を持って soft-delete にするのは monitoring 要件が出てから。
--   M4 ではトークン数が片手で数えられる前提なので、ハード削除でよい。
-- - `last_used_at` は auth middleware が成功時に best-effort で更新する。
--   失敗してもリクエスト本体は通すので、UPDATE エラーで握り潰されないよう
--   呼び出し側でログだけ残す方針 (実装は `auth.rs` 参照)。

CREATE TABLE api_token (
    id            BIGSERIAL   PRIMARY KEY,
    -- 人間可読ラベル (例: "tui-laptop", "tui-mobile")。重複可。
    name          TEXT        NOT NULL,
    -- 生トークンの SHA-256 hex。一意制約で「同じ生トークンを 2 度発行
    -- してしまった」状態を DB レイヤで弾く。
    token_hash    TEXT        NOT NULL UNIQUE,
    -- 最終利用時刻。auth middleware が成功時に best-effort で更新。
    last_used_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
