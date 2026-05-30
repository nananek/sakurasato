//! `GET /media/{*key}` — versitygw に格納された画像を配信する。
//!
//! versitygw は CLAUDE.md §3 で「非公開・内部ネット限定」と定めているため、
//! 公開クライアント (連合 / ブラウザ) は本ルート経由でのみオブジェクトを
//! 取得できる。署名 URL や ACL は使わず、本サーバが代理で GET する。
//!
//! ## URL 設計
//!
//! axum 0.8 のワイルドカード `{*key}` を使い、`/media/path/to/object.png`
//! のようなスラッシュ入りキーを単一の `String` でキャプチャする。
//!
//! ## 本文の扱い
//!
//! M4 PR1 では **`GetObject` のレスポンスボディを一括バッファリング** して
//! `Body::from(bytes)` で返す。max オブジェクトサイズは `media_proxy.max_bytes`
//! (既定 25 MiB) で、アップロードサニタイザ側で上限を強制している前提。
//! 本来はストリーミング (`Body::from_stream` + `ByteStream`) が望ましいが、
//! `ByteStream` の `Stream` 実装を axum の `Body::from_stream` シグネチャに
//! 適合させる際に型のマッサージが要るため、PR1 ではシンプルに collect する。
//! 巨大オブジェクトを扱うようになった段階で stream 化する (今後の milestone)。
//!
//! ## エラー
//!
//! - `NoSuchKey` (オブジェクト不在) → 404。
//! - その他の S3 エラー → 500 (詳細はログ出力のみ、本文に出さない)。
//!
//! ## パスサニタイズ (PR #31 round-1 🔴 対応)
//!
//! axum の `{*key}` は URL パスを正規化せずキャプチャするため、`/media/../secret`
//! のような相対 traversal が `key = "../secret"` のまま versitygw に渡る恐れが
//! ある。versitygw は POSIX volume バックエンドを使う想定 (CLAUDE.md §6) で、
//! S3 key を相対パスとして解決する driver なら traversal が成立し得る。
//!
//! 深層防御として、本ハンドラ自身が key の各セグメントを検証する:
//! - 空セグメント (`a//b`) を拒否
//! - `.` / `..` セグメントを拒否
//! - NUL (`\0`) を拒否 (古い POSIX driver で path 区切り扱いされる例あり)
//! - `\` を拒否 (Windows 区切りを期待する driver 対策、本プロジェクトは Linux
//!   だが将来 versitygw が driver 変更したときの保険)
//!
//! S3 仕様上 key は UTF-8 で 1024 バイトまで任意の文字が使えるが、本サーバが
//! 生成する key は `<sha256>.<ext>` 形式の予定 (M6/M7) で上記制約に収まる。
//! 仕様逸脱 key は他経路から書き込まれた攻撃用オブジェクトの可能性が高いので
//! 一律 400 で拒否する。

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn handle(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    if !is_safe_key(&key) {
        tracing::warn!(key = %key, "media GET: rejected unsafe key");
        return StatusCode::BAD_REQUEST.into_response();
    }
    let bucket = state.config().storage.bucket.clone();
    let resp = match state
        .s3_client()
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(SdkError::ServiceError(svc)) if matches!(svc.err(), GetObjectError::NoSuchKey(_)) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(err) => {
            tracing::error!(?err, key = %key, "media GET failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let content_type_header = resp
        .content_type()
        .and_then(|s| HeaderValue::from_str(s).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    let content_length = resp.content_length();

    // body は ByteStream。`collect()` で全部バッファリングし `Body::from`。
    let bytes = match resp.body.collect().await {
        Ok(agg) => agg.into_bytes(),
        Err(err) => {
            tracing::error!(?err, key = %key, "media GET: collect body failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut response = (StatusCode::OK, Body::from(bytes)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type_header);
    // PR #31 round-1 🟢 対応: media-proxy 経由のサニタイズが M7 まで入らない
    // ため、ブラウザにスクリプト実行させない最小 CSP と sniff 抑止を載せて
    // 「アップロードされた HTML が image を主張して実行される」攻撃に蓋を
    // しておく。media-proxy 導入後も外しても問題ないが、外す利益が無いので
    // そのまま残す方針。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(len) = content_length
        && let Ok(hv) = HeaderValue::from_str(&len.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, hv);
    }
    response
}

/// versitygw に渡す前に key の妥当性を検証する。
///
/// 拒否ルール:
/// - 空文字列 (`/media/` 単体アクセス)
/// - 空セグメント (`a//b` — `///` の連続)
/// - `.` / `..` セグメント (相対パス traversal)
/// - NUL バイト (`\0` — 一部 POSIX driver で path 区切り扱い)
/// - バックスラッシュ (`\\` — Windows 区切り、driver 差吸収の保険)
///
/// 制御文字 (0x00-0x1f, 0x7f) も拒否する: S3 key として spec 上は使えるが、
/// 我々が生成する key は `<sha256>.<ext>` のような ASCII 印字可能のみで、
/// 制御文字が混じった key は攻撃由来の可能性が高い。
fn is_safe_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    if key
        .bytes()
        .any(|b| b == 0 || b == b'\\' || b.is_ascii_control())
    {
        return false;
    }
    for seg in key.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::is_safe_key;

    #[test]
    fn safe_keys_pass() {
        assert!(is_safe_key("a.png"));
        assert!(is_safe_key("images/abc.png"));
        assert!(is_safe_key("a/b/c/d/e.webp"));
        assert!(is_safe_key("ab12-_3.png"));
        // ファイル名先頭のドットは OK (隠しファイルではなく key 名)。
        assert!(is_safe_key(".dotfile"));
        // 中間ドットは OK (`a.b.c` は単一セグメント)。
        assert!(is_safe_key("a.b.c"));
    }

    #[test]
    fn rejects_traversal_segments() {
        assert!(!is_safe_key("../secret"));
        assert!(!is_safe_key("a/../b"));
        assert!(!is_safe_key("./x"));
        assert!(!is_safe_key("a/./b"));
        // 末尾 `/..` も。
        assert!(!is_safe_key("a/.."));
    }

    #[test]
    fn rejects_empty_and_double_slash() {
        assert!(!is_safe_key(""));
        assert!(!is_safe_key("/"));
        assert!(!is_safe_key("a//b"));
        // 先頭 `/` は空セグメントになる ── reject。
        assert!(!is_safe_key("/a"));
        // 末尾 `/` も同様。
        assert!(!is_safe_key("a/"));
    }

    #[test]
    fn rejects_control_and_separator_chars() {
        assert!(!is_safe_key("a\0b"));
        assert!(!is_safe_key("a\\b"));
        assert!(!is_safe_key("a\nb"));
        assert!(!is_safe_key("a\rb"));
        assert!(!is_safe_key("a\tb"));
    }
}
