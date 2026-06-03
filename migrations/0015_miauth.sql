-- Sakurasato M14 #157 — Misskey MiAuth 互換 API endpoint の基盤テーブル。
--
-- 親 Issue #150 (= 「Misskey クライアント (Milktea / MissRirica 等) から
-- リアクション / 投稿が叩けるようにする」) の foundation PR で、認証フロー
-- (= #158) / read endpoints (= #159) / write endpoints (= #160) が乗る前に、
-- DB スキーマ + config + listener + auth middleware を一括で整備する。
--
-- ## AGPL discipline
--
-- Misskey 本体は AGPL-3.0 (§13 network copyleft)、Sakurasato は MIT。本表
-- スキーマは **misskey-hub.net + api-doc.misskey.io の公開 API 仕様** のみを
-- 一次資料として **clean-room 設計** している (= Misskey の TypeScript source
-- を翻訳していない)。API 仕様は interface = 著作権対象外 (Oracle v Google)。
--
-- ## 2 テーブル構成
--
-- `miauth_token` ── Misskey 互換クライアントが認可後に受け取る Bearer 相当の
-- トークン。既存 `api_token` (= TUI 用、permission 概念なし、M4 PR1) と
-- **完全分離** ── TUI 用 token に MiAuth permission の概念が混ざらない、
-- DB 監査でも区別が明確、認証 middleware が異なる経路を歩く。
--
-- `miauth_session` ── MiAuth 認証フローの中間状態。クライアントが UUID を
-- 生成して `GET /miauth/{uuid}?name=&permission=` を叩いた時点で pending 行
-- を作り、ユーザが CLI で approve すると `approved` になり、クライアントが
-- `POST /api/miauth/{uuid}/check` で token を取得すると `consumed` になる。
-- ── これらの endpoint 自体の実装は #158 で行うが、スキーマは本 PR で先に
-- 整えておく (= マイグレーション順序を後ろから前にしない、`#157` の責務)。
--
-- ## 状態機械 (miauth_session.state)
--
-- ```
--   register (GET /miauth/{uuid})
--           ▼
--       pending  ──── CLI reject ────► rejected (終端)
--           │
--           │ CLI approve
--           ▼
--       approved
--           │
--           │ POST /api/miauth/{uuid}/check (= token を発行)
--           ▼
--       consumed (token_id を保持、再 check は同 token を返す = 冪等)
--
--   (pending のまま expires_at を過ぎたら expire_old_sessions が expired へ)
-- ```
--
-- 重要な不変条件:
-- - `consumed` 状態の行は `issued_token_id IS NOT NULL` (= token が必ず付随)
-- - `pending` / `approved` の行は `issued_token_id IS NULL`
-- - `rejected` / `expired` は終端、`issued_token_id IS NULL`
--
-- CHECK 制約で機械的に保証する。
--
-- ## permission scope (JSONB の string array)
--
-- Misskey は 70+ の細粒度 scope (`read:account` / `write:notes` /
-- `write:reactions` / `write:following` / ...) を持つ。本 PR では token に
-- snapshot した permissions を JSONB string array で保管し、エンドポイント
-- (= #158 以降) が「特定 scope を要求」するときに contains 判定する。
--
-- スキーマレベルで scope の妥当性は検証しない (= 未知 scope を CHECK で
-- 弾くと Misskey 側で新規 scope が追加されたとき互換性が壊れる)。endpoint
-- 側で要求 scope を hard-coded list と照合する設計。

-- ── miauth_token ─────────────────────────────────────────────────────

CREATE TABLE miauth_token (
    id            BIGSERIAL    PRIMARY KEY,
    -- 人間可読ラベル (= 認可時の `name` query param、例 "Milktea-iPhone")。
    -- 監査・revoke 時の手がかり。重複可。
    name          TEXT         NOT NULL,
    -- 生トークンの SHA-256 を Base64URL (no-pad) エンコードしたもの。
    -- 実装は server::miauth::token::hash 参照 (M4 `api_token` と同じ形式 +
    -- 同じ rationale ── 256-bit エントロピーなので salt 不要、UNIQUE で
    -- 二重発行を DB レイヤで弾ける)。
    token_hash    TEXT         NOT NULL UNIQUE,
    -- approve 時に snapshot した permission scope の JSONB array。
    -- 形式: `["read:account", "write:reactions"]`。
    -- 空配列 (= `[]`) は「scope ゼロ」= 何の endpoint も叩けないので、
    -- 実用上は 1 個以上入る (CLI で `--permission` 必須にする方針は #158 で)。
    permissions   JSONB        NOT NULL DEFAULT '[]'::jsonb
                                CHECK (jsonb_typeof(permissions) = 'array'),
    last_used_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- 監査用: 直近使用順で並べる照会のため。`api_token` には付けていないが、
-- MiAuth は Misskey クライアント由来で「どの token がアクティブか」を
-- たまに確認したい需要があるため index しておく。
CREATE INDEX idx_miauth_token_last_used
    ON miauth_token (last_used_at DESC NULLS LAST);

-- ── miauth_session ───────────────────────────────────────────────────

CREATE TABLE miauth_session (
    -- UUID はクライアント生成 (= Misskey MiAuth 仕様準拠)。サーバが払い出
    -- さない設計なので、PK としてそのまま使う (BIGSERIAL は持たない)。
    uuid              UUID         PRIMARY KEY,
    -- 認可リクエストの `name` query param (= アプリ表示名、例 "Milktea")。
    -- CLI で「どのアプリの session か」を区別するために必要。
    app_name          TEXT         NOT NULL,
    -- `callback` query param (= 認可後にクライアントにリダイレクトする URL)。
    -- Sakurasato は browser approve 経路を採らない (CLI approve のみ) ので、
    -- 本フィールドは informational ── 保管はするが redirect は発火しない。
    -- `text/html` landing page にも「callback: <url>」を表示しておくと、
    -- ユーザが「正しいアプリの session か」を目視確認できる。
    callback_url      TEXT,
    -- 認可リクエストの `permission` query param (= CSV 形式) を分解して
    -- JSONB array に正規化したもの。
    permissions       JSONB        NOT NULL DEFAULT '[]'::jsonb
                                    CHECK (jsonb_typeof(permissions) = 'array'),
    -- 状態機械 (上のコメント参照)。CHECK で 5 値以外を弾く。
    state             TEXT         NOT NULL DEFAULT 'pending'
                                    CHECK (state IN ('pending', 'approved',
                                                     'rejected', 'consumed',
                                                     'expired')),
    -- consumed 状態のときに発行済み token を指す FK。それ以外の state では
    -- 必ず NULL ── CHECK 制約で不変条件を保証する。token が revoke で消えた
    -- ら ON DELETE SET NULL で session の参照だけ落とす (session 行自体は
    -- 保持して監査トレイルを残す)。
    issued_token_id   BIGINT       REFERENCES miauth_token(id) ON DELETE SET NULL,
    requested_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    approved_at       TIMESTAMPTZ,
    -- 認可待ちの期限 (= config.miauth.session_ttl_secs)。expire_old_sessions
    -- が `state = 'pending' AND now() > expires_at` の行を `expired` に倒す。
    expires_at        TIMESTAMPTZ  NOT NULL,
    -- ── 不変条件 ───────────────────────────────────────────────────
    -- 非 consumed 状態 (= pending / approved / rejected / expired) は必ず
    -- token なし ── browser landing / CLI approve / CLI reject / TTL sweep の
    -- いずれでも token を発行しない経路だから。
    --
    -- consumed 状態は **基本的に token 付き** だが、`ON DELETE SET NULL` で
    -- token 行が revoke された後は **NULL に倒れる** ことを許容する (= 監査
    -- トレイル維持のため session 行は残るが、参照していた token はもう存在
    -- しない)。完全な「consumed AND NULL」状態を弾くと revoke 時に CHECK
    -- 違反になり token 行を消せなくなるため、consumed は両方許可する。
    CONSTRAINT miauth_session_state_token_consistency CHECK (
        state = 'consumed'
        OR issued_token_id IS NULL
    ),
    -- approved 以降の state は approved_at が必ず付く。
    CONSTRAINT miauth_session_approved_at_consistency CHECK (
        (state IN ('approved', 'consumed') AND approved_at IS NOT NULL)
        OR (state IN ('pending', 'rejected', 'expired') AND approved_at IS NULL)
    )
);

-- CLI の `miauth list` (= pending 一覧) で高頻度に使う partial index。
CREATE INDEX idx_miauth_session_pending
    ON miauth_session (requested_at)
    WHERE state = 'pending';

-- expire_old_sessions の sweep で使う。pending かつ expires_at 経過の行を
-- 引くため、state + expires_at の複合 partial index にする。
CREATE INDEX idx_miauth_session_expiry
    ON miauth_session (expires_at)
    WHERE state = 'pending';
