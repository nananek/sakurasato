-- Sakurasato M14 (MiAuth `i/update`) — actor の誕生日 (birthday)。
--
-- Misskey wire は `birthday` を `"YYYY-MM-DD"` 形式の文字列として扱う
-- (時刻・タイムゾーンを持たない単純な日付)。DATE 型ではなく TEXT で
-- そのまま保持し、往復変換 (DATE -> ISO 文字列) の手間とタイムゾーンずれの
-- 懸念を無くす。フォーマット検証は書き込み経路 (`miauth::i::update`) の
-- アプリケーション層で行う。
--
-- remote actor もこの列を持ちうるが (actor JSON に birthday 相当の property
-- は無いため実質使わない)、local actor 専用というわけではなく単純な
-- nullable カラムとして追加する。

ALTER TABLE actor
    ADD COLUMN birthday TEXT;
