-- Sakurasato M14 (MiAuth `i/update`) — actor の location/lang/followedMessage/fields。
--
-- Misskey wire の `UserDetailed` 共有フィールド (location/lang) と Me 専用
-- フィールド (followedMessage)、プロフィールのカスタム項目 (fields) を
-- 追加する。いずれも `crates/server/src/miauth/i.rs` の `i/update` から
-- 書き込む。
--
-- `fields` は `[{"name": "...", "value": "..."}]` 形式の JSONB 配列。
-- Misskey wire は常に配列を返す (nullable ではない) ため、NOT NULL DEFAULT
-- '[]' で「項目無し」を空配列として表現する。

ALTER TABLE actor
    ADD COLUMN location TEXT,
    ADD COLUMN lang TEXT,
    ADD COLUMN followed_message TEXT,
    ADD COLUMN fields JSONB NOT NULL DEFAULT '[]'::jsonb;
