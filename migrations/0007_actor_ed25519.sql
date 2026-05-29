-- Sakurasato M3b — actor に Ed25519 鍵カラムを追加。
--
-- 既存の public_key_pem / private_key_pem は RSA 鍵を保持し続け、
-- ここで追加するカラムは「同じ actor が併載する Ed25519 鍵」を表す。
--
-- 連合の主流 (Mastodon 系) は依然として cavage HTTP signatures + RSA-SHA256
-- を要求するため、RSA を捨てることはできない。一方で Misskey 系 (Iceshrimp,
-- Sharkey 等) や RFC 9421 HTTP Message Signatures を採用する実装は Ed25519
-- も受け付ける。両方を発行・公開して、相手の対応状況に合わせて使い分けるた
-- めにカラムを並べて持つ。
--
-- すべて NULL 許容:
--   - 既存の M3a の local actor は Ed25519 鍵を持たないため (init --force で
--     後から再生成する)。
--   - remote actor は Ed25519 鍵を公開していないインスタンスがあるため
--     (publicKey に RSA だけしかない)。
--
-- ed25519_public_key_id は ActivityPub の publicKey.id に対応する URI
-- (典型的には "<ap_id>#ed25519-key")。RSA 側 public_key_id とは別物。

ALTER TABLE actor
    ADD COLUMN ed25519_public_key_id  TEXT,
    ADD COLUMN ed25519_public_key_pem TEXT,
    ADD COLUMN ed25519_private_key_pem TEXT;

-- key id は公開鍵を引く index。NULL 同士は UNIQUE 制約上重複扱いされないので
-- 部分 index 不要だが、明示しておく。
CREATE UNIQUE INDEX idx_actor_ed25519_public_key_id
    ON actor (ed25519_public_key_id)
    WHERE ed25519_public_key_id IS NOT NULL;
