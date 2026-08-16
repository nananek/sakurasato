-- MFM ソース (Misskey 互換) を保持するための列。
-- ローカル投稿は `source = 生の投稿本文` (plain text)、remote note は現状 NULL。
-- AP `Note.source` / `_misskey_content` として配送する (= Misskey 系が MFM として
-- レンダリングできる)。NULL = MFM ソース無し。
ALTER TABLE note ADD COLUMN source TEXT;
