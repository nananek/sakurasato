//! ローカル API (M4) 用 Bearer トークンの生成・ハッシュ・検証 + CLI。
//!
//! ## トークン形式
//!
//! 生トークンは `OsRng` から 32 バイト引き、Base64URL (no pad) で
//! 文字列化した 43 文字。空白・`+`/`/` を含まないので HTTP ヘッダにそのまま
//! 載せられる。
//!
//! ## ストレージ
//!
//! DB には **SHA-256(raw) を `Base64URL` no-pad** したものだけを保存する。
//! 256 bit の高エントロピー入力に対して SHA-256 を直接適用しており、
//! salt は不要 (ブルートフォース不可、レインボーテーブル無関係)。salt を
//! 入れると `find_by_hash` の単純な等値検索が成立せず、行全件スキャン+定数
//! 時間比較が必要になり、トークン数が増えたとき (運用上は数本だが) のコスト
//! が無駄に上がる。
//!
//! ## CLI
//!
//! `sakurasato token issue --name tui-laptop` — 新規発行。**生トークンは
//! このときだけ stdout に出る**。
//! `sakurasato token list` — 一覧 (生トークンは出さない)。
//! `sakurasato token revoke --id N` — 失効 (ハード削除)。

use anyhow::{Context, bail};
use base64::engine::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::rand_core::{OsRng, RngCore};
use sakurasato_core::Config;
use sakurasato_core::repo;
use sha2::{Digest, Sha256};

use crate::cli::{TokenArgs, TokenCommand};
use crate::state::AppState;

/// 生トークンのバイト長 (= 256 bit)。
const RAW_TOKEN_BYTES: usize = 32;

/// 新しい生トークン文字列 (`Base64URL` no-pad の 43 文字) を返す。
///
/// `OsRng` は ed25519-dalek / rsa が `&mut OsRng` で受けているのと同じ
/// `rand_core` 0.6 系の `OsRng`。`fill_bytes` は OS の CSPRNG (getrandom) を
/// そのまま読むため、暗号用途として十分強い。
pub fn generate_raw() -> String {
    let mut bytes = [0u8; RAW_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 生トークンを DB 格納用のハッシュ文字列に変換する。
pub fn hash(raw_token: &str) -> String {
    let digest = Sha256::digest(raw_token.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// `Authorization: Bearer <token>` の値から `<token>` 部分を取り出す。
/// 前後の空白を吸収し、prefix が違うときは `None`。case-insensitive 比較
/// (RFC 7235 §2.1 の auth-scheme は ASCII case-insensitive)。
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    let v = header_value.trim();
    // ASCII case-insensitive で "bearer " を剥がす。
    let (scheme, rest) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = rest.trim_start();
    if token.is_empty() {
        return None;
    }
    Some(token)
}

/// `sakurasato token …` のエントリポイント。
///
/// `Serve` と違って migrations は走らせない (`init` 済み前提)。
pub async fn run(config: Config, args: TokenArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        TokenCommand::Issue(issue) => {
            if issue.name.trim().is_empty() {
                bail!("--name must not be empty");
            }
            let raw = generate_raw();
            let token_hash = hash(&raw);
            let row = repo::api_token::insert(
                state.pool(),
                repo::api_token::NewApiToken {
                    name: issue.name.clone(),
                    token_hash,
                },
            )
            .await
            .context("insert api_token row")?;
            // 生トークンはここでだけ出す。以降 DB には hash しか残らない。
            // stderr に注意書きを出し、stdout は生トークンだけにして、
            // `sakurasato token issue --name x > token.txt` でファイルに
            // 落とせるようにする。
            eprintln!("issued token id={} name={:?}", row.id, row.name);
            eprintln!("(this is the only time the raw token is displayed)");
            println!("{raw}");
            Ok(())
        }
        TokenCommand::List => {
            let rows = repo::api_token::list_all(state.pool())
                .await
                .context("list api_token rows")?;
            if rows.is_empty() {
                println!("(no tokens issued)");
                return Ok(());
            }
            for row in rows {
                let last = row
                    .last_used_at
                    .map_or_else(|| "never".to_string(), |t| t.to_rfc3339());
                println!(
                    "id={} name={:?} created={} last_used={}",
                    row.id,
                    row.name,
                    row.created_at.to_rfc3339(),
                    last,
                );
            }
            Ok(())
        }
        TokenCommand::Revoke(revoke) => {
            let deleted = repo::api_token::delete_by_id(state.pool(), revoke.id)
                .await
                .with_context(|| format!("revoke api_token id={}", revoke.id))?;
            if deleted {
                println!("revoked token id={}", revoke.id);
                Ok(())
            } else {
                bail!("no token with id={}", revoke.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_unique_and_url_safe() {
        let a = generate_raw();
        let b = generate_raw();
        assert_ne!(a, b);
        // Base64URL no-pad は [A-Za-z0-9_-] のみ。
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        // 32 byte → 43 char (no pad)。
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn hash_is_deterministic_and_constant_length() {
        let raw = generate_raw();
        let h1 = hash(&raw);
        let h2 = hash(&raw);
        assert_eq!(h1, h2);
        // SHA-256 = 32 byte → Base64URL no-pad で 43 char。
        assert_eq!(h1.len(), 43);
        // 別トークンの hash は当然違う。
        let other = hash(&generate_raw());
        assert_ne!(h1, other);
    }

    #[test]
    fn parse_bearer_strips_prefix_case_insensitive() {
        assert_eq!(parse_bearer("Bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer("bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer("BEARER  xyz"), Some("xyz"));
        // header_value 全体に `.trim()` をかけてから scheme をはがすため、
        // 前後の余白は両方除去される。これは意図した挙動 (Bearer トークン
        // 自体に空白は含まれない)。
        assert_eq!(parse_bearer("  Bearer  xyz  "), Some("xyz"));
    }

    #[test]
    fn parse_bearer_rejects_malformed() {
        assert_eq!(parse_bearer(""), None);
        assert_eq!(parse_bearer("xyz"), None);
        assert_eq!(parse_bearer("Basic abc:def"), None);
        // Bearer scheme with no token.
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("Bearer"), None);
    }
}
