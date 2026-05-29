//! Multikey (W3C VC Data Integrity / FEP-521a) helpers.
//!
//! Ed25519 公開鍵を FEP-521a の `assertionMethod` で公開するために、PKCS#8
//! SPKI PEM を multibase + multicodec エンコードされた `publicKeyMultibase`
//! 文字列に変換する。
//!
//! 参考:
//! - <https://codeberg.org/fediverse/fep/src/branch/main/fep/521a/fep-521a.md>
//! - <https://w3c-ccg.github.io/multikey/>
//! - multicodec ed25519-pub の prefix: 0xED → varint で `[0xED, 0x01]`
//!
//! 出力例: `"z6MkpTHR8VNsBxYAAWHut2Geadd9jSruqfoEYxXRdtmU8nWB"` (32-byte 公開鍵
//! + 2-byte prefix を base58btc, multibase prefix `z`)。

use anyhow::Context;
#[cfg(test)]
use anyhow::anyhow;
use ed25519_dalek::VerifyingKey;
use ed25519_dalek::pkcs8::DecodePublicKey;

/// multicodec の "ed25519-pub" コード `0xED` を unsigned varint で表現したもの。
const ED25519_MULTICODEC_PREFIX: [u8; 2] = [0xed, 0x01];

/// Ed25519 公開鍵 (PKCS#8 SPKI PEM) を Multikey の `publicKeyMultibase` 文字列
/// に変換する。
pub fn ed25519_pem_to_multibase(pem: &str) -> anyhow::Result<String> {
    let verifying =
        VerifyingKey::from_public_key_pem(pem).context("parse Ed25519 public key PEM")?;
    Ok(ed25519_bytes_to_multibase(verifying.as_bytes()))
}

/// Raw 32-byte Ed25519 公開鍵を `publicKeyMultibase` に整形する。
fn ed25519_bytes_to_multibase(pubkey: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(ED25519_MULTICODEC_PREFIX.len() + pubkey.len());
    payload.extend_from_slice(&ED25519_MULTICODEC_PREFIX);
    payload.extend_from_slice(pubkey);
    multibase::encode(multibase::Base::Base58Btc, &payload)
}

/// Reverse of [`ed25519_pem_to_multibase`] — テストヘルパ。本体コードからは
/// 使わないが、生成した multibase 値が同じ鍵を表現していることを単体テスト
/// で検証するために置いておく。
#[cfg(test)]
fn multibase_to_ed25519_bytes(s: &str) -> anyhow::Result<[u8; 32]> {
    let (base, payload) = multibase::decode(s).context("decode multibase")?;
    if base != multibase::Base::Base58Btc {
        return Err(anyhow!("expected base58btc multibase, got {base:?}"));
    }
    if payload.len() != ED25519_MULTICODEC_PREFIX.len() + 32 {
        return Err(anyhow!(
            "unexpected multikey payload length {} (want {})",
            payload.len(),
            ED25519_MULTICODEC_PREFIX.len() + 32
        ));
    }
    let (prefix, key) = payload.split_at(ED25519_MULTICODEC_PREFIX.len());
    if prefix != ED25519_MULTICODEC_PREFIX {
        return Err(anyhow!(
            "wrong multicodec prefix {prefix:02x?} (want ed25519-pub = ed 01)"
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(key);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use rsa::rand_core::OsRng;

    #[test]
    fn multibase_starts_with_z_and_roundtrips() {
        let signing = SigningKey::generate(&mut OsRng);
        let verifying = signing.verifying_key();
        let pem = verifying.to_public_key_pem(LineEnding::LF).unwrap();

        let encoded = ed25519_pem_to_multibase(&pem).unwrap();
        assert!(
            encoded.starts_with('z'),
            "multibase prefix must be base58btc 'z': got {encoded}",
        );
        // base58btc(0xed 0x01 || 32-byte) は概ね 48 文字 + 'z' = 49 文字に収まる。
        assert!((48..=52).contains(&encoded.len()), "len={}", encoded.len());

        let decoded = multibase_to_ed25519_bytes(&encoded).unwrap();
        assert_eq!(&decoded, verifying.as_bytes());
    }

    #[test]
    fn rejects_non_ed25519_pem() {
        let garbage = "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n";
        assert!(ed25519_pem_to_multibase(garbage).is_err());
    }

    #[test]
    fn known_vector_is_stable() {
        // 回帰テスト: RFC 8032 §7.1 Test 1 の Ed25519 公開鍵に対する Multikey
        // 表現を固定する。値の正しさは
        //   - multibase prefix が 'z' (base58btc) であること
        //   - decode して [0xED, 0x01] (multicodec ed25519-pub) で始まること
        //   - 残り 32 バイトが元の pubkey と一致すること
        // を `multibase_starts_with_z_and_roundtrips` で別途検証する。ここでは
        // multibase ライブラリのバージョン更新やバイト順入れ替え等の事故を
        // 検出するため、出力文字列そのものをピン留めする。
        let pubkey_hex = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let mut bytes = [0u8; 32];
        for (i, chunk) in pubkey_hex.as_bytes().chunks(2).enumerate() {
            bytes[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
        }
        let encoded = ed25519_bytes_to_multibase(&bytes);
        assert_eq!(encoded, "z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw");
        // 形式契約の二重チェック。
        let roundtrip = multibase_to_ed25519_bytes(&encoded).unwrap();
        assert_eq!(roundtrip, bytes);
    }
}
