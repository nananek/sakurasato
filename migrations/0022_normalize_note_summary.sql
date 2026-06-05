-- 0022: 既存 note の空 summary (`''` / 空白のみ) を NULL に畳む一回限りの backfill。
--
-- Pleroma は CW 無しのノートでも `summary: ""` を送ってくるが、PR #222 以前の
-- inbound Create/Update 受信はこれを `Some("")` のまま保存していた。Misskey 系
-- クライアント (Aria 等) は `cw` が非 null = 「CW あり」と解釈するため、Pleroma の
-- 全ノートが「警告文の無い CW」に見える症状になっていた。inbound を
-- normalize_summary で空 → None に正規化する修正と合わせ、本 migration で既に
-- 保存済みの stale 行を一掃する。
--
-- 空白のみ (`btrim(summary) = ''`) も対象 ── これも実質 CW なし。非空の summary
-- (実際の CW テキスト) は trim せず温存する (= inbound 正規化と同じ方針)。
UPDATE note
SET summary = NULL
WHERE summary IS NOT NULL
  AND btrim(summary) = '';
