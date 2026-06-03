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
            // 落とせるようにする。`--out <PATH>` を渡した場合は stdout に
            // は出さず、指定ファイルだけに書く (compose の名前付きボリューム
            // 経由でテストランナに共有する用途)。既存ファイルは上書きせず
            // エラーにする ── 古いトークンの存在を黙って奪わないため。
            eprintln!("issued token id={} name={:?}", row.id, row.name);
            eprintln!("(this is the only time the raw token is displayed)");
            if let Some(path) = issue.out.as_deref() {
                write_token_file(path, &raw)
                    .with_context(|| format!("write raw token to {}", path.display()))?;
                eprintln!("wrote raw token to {}", path.display());
            } else {
                println!("{raw}");
            }
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

/// 生トークンをファイルに書き出す (mode 0o600)。
///
/// `OpenOptions::create_new(true)` で **既存ファイルがあれば失敗** させる ──
/// 古いトークンが置かれた状態で黙って上書きすると、テストランナ等が「黙って
/// 入れ替わったトークン」を読み続ける危険があるため。
/// 上位は `--out` を毎回新しいパスに向けるか、既存ファイルを事前に削除する。
///
/// `pub(crate)`: M14 #157 (= 親 issue #150) の `MiAuth` CLI も同じ書き出し方を
/// 共有する (= `miauth approve --out <path>` 経路)。ロジックを 2 重持ちにすると
/// 「片方だけ EACCES 対応漏れ」等の事故が起きるため crate 内 1 本に統一。
pub(crate) fn write_token_file(path: &std::path::Path, raw_token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // 末尾に LF を付ける ── `cat` や POSIX text-file 期待のツールで「No
    // newline at end of file」になるのを避ける。Bearer ヘッダに乗せる側は
    // 必ず `trim()` する想定。
    writeln!(file, "{raw_token}")?;
    Ok(())
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
    fn write_token_file_creates_with_mode_0600_and_trailing_lf() {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token.txt");
        let raw = generate_raw();
        write_token_file(&path, &raw).expect("write_token_file ok");

        let meta = std::fs::metadata(&path).expect("stat");
        // mode は file-type + perms。下位 9 bit だけ見る。
        assert_eq!(meta.mode() & 0o777, 0o600, "expected 0o600");

        let mut buf = String::new();
        std::fs::File::open(&path)
            .expect("open")
            .read_to_string(&mut buf)
            .expect("read");
        // trailing LF が付くこと。
        assert_eq!(buf, format!("{raw}\n"));
    }

    #[test]
    fn write_token_file_refuses_to_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token.txt");
        write_token_file(&path, "old").expect("write initial");
        let err = write_token_file(&path, "new")
            .expect_err("second write must fail (would clobber existing token)");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // 元の中身が保護されていること。
        let buf = std::fs::read_to_string(&path).expect("read");
        assert_eq!(buf, "old\n");
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
