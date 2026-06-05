-- 0021: 既存 reaction 行の content から @host suffix を剥がして `:shortcode:` に
-- 正規化する一回限りの backfill。
--
-- PR #183 (outbound) / #187 (inbound) で全 write 経路が `:foo@host:` → `:foo:` に
-- 正規化するようになった (host 非依存に `@` 以降を落とす =
-- normalize_inbound_reaction_content / parse_local_emoji_shortcode)。しかしそれ以前
-- に書かれた行は `:foo@host:` / `:foo@.:` のまま残り、新しい `:foo:` と別キー扱いに
-- なって Aria 等のクライアントで「同じ絵文字が増殖」して見える (= reaction shortcode
-- mismatch)。本 migration でその stale 行を一括正規化する。
--
-- 正規化式: 先頭 ':' + ('@'/':' を含まない shortcode) + '@' + 任意 (host:port 含む)
-- + 末尾 ':' を `:shortcode:` に畳む。Unicode (`👍`) や `@` を持たない行 (`:foo:`) は
-- 不変。空 shortcode (`:@host:`) は `[^:@]+` が 1 文字以上を要求するので対象外
-- (= コード側 extract_shortcode が None を返すケースと一致)。

-- 1. (note_id, actor_id, 正規化後 content) が同一のグループは「同じ人が同じ note に
--    同じ絵文字で 1 回リアクションした」= 1 行に畳む。emoji_id を持つ行を優先し、
--    次いで低い id を残して (= reactionEmojis の画像参照を保つ) 残りを削除する。
--    UNIQUE (note_id, actor_id, content) 衝突を後段の UPDATE 前に解消するため。
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY
                note_id,
                actor_id,
                regexp_replace(content, '^(:[^:@]+)@.*:$', '\1:')
            ORDER BY (emoji_id IS NOT NULL) DESC, id ASC
        ) AS rn
    FROM reaction
)
DELETE FROM reaction
WHERE id IN (SELECT id FROM ranked WHERE rn > 1);

-- 2. 残った suffix 付き行を in-place で正規化する。1 で dedup 済みなので
--    UNIQUE 制約に衝突しない。
UPDATE reaction
SET content = regexp_replace(content, '^(:[^:@]+)@.*:$', '\1:')
WHERE content ~ '^:[^:@]+@.*:$';
