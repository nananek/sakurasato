-- Sakurasato — ホスト名の正規化 (末尾ドット / 大文字小文字)。
--
-- `url::Url::host_str()` は FQDN の末尾ドットを保持する (`evil.example.`)。
-- 既存行に末尾ドット付きの actor / domain_moderation が入っていると、
-- 完全一致で照合するモデレーション (silence / suspend) を別名で
-- すり抜けられる。アプリ側は `net_guard::canonical_host` を必須化したので、
-- 既存行も一度だけ正規化する。
--
-- domain_moderation は host UNIQUE のため、同一ドメインへ縮退する別名重複
-- (`evil.example` / `evil.example.` 等) が既にある場合は 1 行に畳んでから
-- 正規化する。畳む基準は id の若さではなく **severity の強さ
-- ('suspend' > 'silence')** ── 正規化前は別名ごとに別々の措置が入っていた
-- ケースがありえるため、id 基準で機械的に片方を残すと suspend が silence に
-- 黙って格下げされる恐れがある (0021 の reaction dedup と同じ
-- `ROW_NUMBER() OVER (PARTITION BY ...)` パターンを severity 優先に変えて踏襲)。

WITH ranked AS (
    SELECT id,
           row_number() OVER (
               PARTITION BY lower(rtrim(host, '.'))
               ORDER BY (severity = 'suspend') DESC, id ASC
           ) AS rn
      FROM domain_moderation
)
DELETE FROM domain_moderation d
 USING ranked r
 WHERE d.id = r.id
   AND r.rn > 1;

UPDATE domain_moderation
   SET host = lower(rtrim(host, '.'))
 WHERE host <> lower(rtrim(host, '.'));

-- actor は (preferred_username, host) が UNIQUE (idx_actor_username_host)。
-- ただし actor 行の削除は note/follow/reaction/media/announce/notification/
-- block/delivery_queue 等を ON DELETE CASCADE で巻き込む (migrations/0002〜
-- 0031 参照) ため、domain_moderation のように機械的に merge/delete するのは
-- 危険が大きすぎる (どちらの行を残すべきかをこの migration は判断できない)。
-- 正規化後に (preferred_username, host) が衝突する行だけは **正規化せず
-- 元のホスト文字列を残し**、運用者が手動で調査できるよう警告を出す。
-- (実運用でこの衝突が起き得るのは、同一 preferredUsername の remote actor が
-- 大文字小文字/末尾ドット違いの ap_id で二重に fetch された場合のみで、
-- 極めて稀と見込む)。
DO $$
DECLARE
    conflict_count integer;
BEGIN
    SELECT count(*) INTO conflict_count
      FROM actor a
     WHERE a.host <> lower(rtrim(a.host, '.'))
       AND EXISTS (
           SELECT 1
             FROM actor b
            WHERE b.id <> a.id
              AND b.preferred_username = a.preferred_username
              AND lower(rtrim(b.host, '.')) = lower(rtrim(a.host, '.'))
       );
    IF conflict_count > 0 THEN
        RAISE WARNING
            'sakurasato: migration 0033 left % actor row(s) un-normalized '
            '(normalizing their host would collide with another actor row on '
            '(preferred_username, host)); resolve manually',
            conflict_count;
    END IF;
END $$;

UPDATE actor a
   SET host = lower(rtrim(a.host, '.')),
       updated_at = now()
 WHERE a.host <> lower(rtrim(a.host, '.'))
   AND NOT EXISTS (
       SELECT 1
         FROM actor b
        WHERE b.id <> a.id
          AND b.preferred_username = a.preferred_username
          AND lower(rtrim(b.host, '.')) = lower(rtrim(a.host, '.'))
   );
