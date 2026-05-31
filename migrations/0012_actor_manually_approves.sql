-- Sakurasato M12 / Issue #66 — Follow 承認制 (manuallyApprovesFollowers)。
--
-- 鍵アカ運用: フラグが TRUE の local actor 宛の inbound Follow は
-- auto-Accept されず `follow.state = pending` のまま据え置かれる。
-- 管理者が `sakurasato-server follow-request approve --id N` で明示
-- 承認すると Accept activity が delivery_queue に積まれ、state が
-- `accepted` に遷移する (reject も同様)。
--
-- default FALSE = 後方互換: 既存の auto-Accept 挙動を維持する。
-- お一人様 + 自動承認設計の従来仕様を壊さないため、新フィールドは
-- 必ず明示的に opt-in する形にする。
--
-- remote actor もこの列を持つが、本サーバから見て意味があるのは
-- local actor (= 我々自身) のみ。remote 側の値はキャッシュとして
-- 取り込んでおくが、inbound dispatcher の分岐は使わない (相手側
-- インスタンスがどう Follow を扱うかは我々の管轄外)。
--
-- Mastodon / Misskey ともに actor JSON の `manuallyApprovesFollowers`
-- (boolean) を解釈し、UI に「鍵アカ」表示を出す。本サーバも同じ key
-- 名で actor JSON に乗せる。

ALTER TABLE actor
    ADD COLUMN manually_approves_followers BOOLEAN NOT NULL DEFAULT FALSE;
