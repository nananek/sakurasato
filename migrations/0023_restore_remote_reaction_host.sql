-- 0023: remote custom emoji の reaction content に `@host` suffix を復元する
-- 一回限りの backfill (Issue #242)。
--
-- PR #186 (`normalize_inbound_reaction_content`) と migration 0021 は、inbound
-- reaction の `@host` を host が何であろうと無条件で剥がして `:foo:` に畳んでいた。
-- これはローカル絵文字の往復 (#182) を畳む狙いだったが、**真リモートの custom
-- emoji まで巻き込んで** `:blob@misskey.io:` → `:blob:` にしていた。
--
-- Misskey 系クライアント (Aria 等) は reaction key の形で解決先を変える:
--   - `:foo@host:` (= `@` あり) → `note.reactionEmojis["foo@host"]` の URL を使う
--   - `:foo:`      (= `@` なし) → reactionEmojis を見ず、その鯖 (= sakurasato) の
--      ローカル絵文字ストアから `foo` を引く
-- host を剥がすとリモート絵文字が「自鯖ローカル絵文字」と誤認され、ローカルに
-- 同名 shortcode が無いと描画できなくなる (= versitygw にキャッシュ画像はあるのに
-- 見えない)。本 migration で stale 行に `@host` を復元し、以降は
-- `dispatch::reaction::process_inbound_reaction` が remote 絵文字に対し
-- `:shortcode@signer_host:` を書く。
--
-- 復元対象は **emoji_id が remote emoji 行 (is_local = FALSE) を指す** content
-- だけ。host は emoji.host (= 学習時の signer host、lowercase) から取る。shortcode
-- も emoji.shortcode (= canonical) を使い、古い content 文字列には依存しない。
-- ローカル絵文字往復 (#182) / Unicode / 学習不能 (emoji_id NULL) の行は :foo: のまま。
--
-- UNIQUE (note_id, actor_id, content) 衝突は起きない: 0021 が
-- (note_id, actor_id, 正規化後 content) 単位で dedup 済みのため、各 (note, actor)
-- に `:foo:` 行は高々 1 本。`@host` を付け直しても一意性は保たれる。

UPDATE reaction r
SET content = ':' || e.shortcode || '@' || e.host || ':'
FROM emoji e
WHERE r.emoji_id = e.id
  AND e.is_local = FALSE
  AND e.host IS NOT NULL
  AND e.host <> ''
  AND r.content LIKE ':%:'      -- custom emoji 形のみ (Unicode を除外)
  AND r.content NOT LIKE '%@%'; -- 既に host 付きの行 (= 将来の再適用) は触らない
