-- Sakurasato — ホスト名の正規化 (末尾ドット / 大文字小文字)。
--
-- `url::Url::host_str()` は FQDN の末尾ドットを保持する (`evil.example.`)。
-- 既存行に末尾ドット付きの actor / domain_moderation が入っていると、
-- 完全一致で照合するモデレーション (silence / suspend) を別名で
-- すり抜けられる。アプリ側は `net_guard::canonical_host` を必須化したので、
-- 既存行も一度だけ正規化する。
--
-- domain_moderation は host UNIQUE のため、同一ドメインの別名行が既に
-- ある場合は新しい id を残して古い行を削除してから正規化する。

DELETE FROM domain_moderation d
 USING domain_moderation e
 WHERE d.id > e.id
   AND lower(rtrim(d.host, '.')) = lower(rtrim(e.host, '.'));

UPDATE domain_moderation
   SET host = lower(rtrim(host, '.'))
 WHERE host <> lower(rtrim(host, '.'));

UPDATE actor
   SET host = lower(rtrim(host, '.')),
       updated_at = now()
 WHERE host <> lower(rtrim(host, '.'));
