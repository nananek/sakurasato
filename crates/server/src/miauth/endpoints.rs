//! `POST /api/endpoints` ── Misskey 互換のサポート endpoint 名一覧
//! (= M14 #176 / 親 #150)。
//!
//! ## なぜ必要か
//!
//! [poppingmoon/aria](https://github.com/poppingmoon/aria) の
//! `lib/provider/emojis_notifier_provider.dart` は、emoji 取得の前に
//! `endpoints.contains('emojis')` を判定する経路を取る:
//!
//! ```text
//! Aria → POST /api/endpoints (= サーバが対応する endpoint 名一覧を取得)
//!      → contains("emojis") なら POST /api/emojis を呼ぶ
//!      → contains しないなら fallback で POST /api/meta の `emojis` field を読む
//! ```
//!
//! Sakurasato は `/api/endpoints` を実装していなかったため Aria が fallback に
//! 倒れ、`/api/meta.emojis` (= 我々が emit していない field) が空配列扱いされて
//! emoji picker が空のまま、という症状になっていた。
//!
//! ## レスポンス shape
//!
//! [misskey-dart](https://github.com/shiosyakeyakini-info/misskey_dart) の
//! `Misskey.endpoints()` 実装より:
//!
//! ```dart
//! Future<List<String>> endpoints() async {
//!   final response = await apiService.post<List>("endpoints", {});
//!   return response.cast<String>();
//! }
//! ```
//!
//! = **JSON array of string**。`{ endpoints: [...] }` のような envelope は **無い**
//! (= 直接 array を返す)。
//!
//! ## endpoints の中身
//!
//! Misskey 本家は handler ディレクトリ走査で動的に生成しているが、Sakurasato は
//! 静的に `MiAuth` listener が登録している endpoint 名を **hardcode** する
//! (= scope の絞り込みを明示するため)。
//!
//! 認証 / browser landing 系 (= `miauth/{uuid}` / `api/miauth/{uuid}/check` /
//! healthz / nodeinfo) は public API の慣行で **emit しない** ── Misskey 本家も
//! 認証系を `endpoints` に出していない。
//!
//! ## AGPL discipline
//!
//! [misskey-dart](https://github.com/shiosyakeyakini-info/misskey_dart) (= MIT) +
//! [api-doc.misskey.io](https://api-doc.misskey.io/) の公開仕様から書き起こした。
//! Misskey 本体 (AGPL-3.0) の TypeScript handler は未参照 ──
//! `[[agpl-discipline-miauth]]` 準拠。

use axum::Json;
use axum::response::{IntoResponse, Response};

/// `MiAuth` listener がサポートする endpoint 名 (`/api/` プレフィックス無し)。
///
/// **本配列は [`crate::miauth::router`] で実際に mount されている route 集合と
/// 完全に同期** する必要がある ── 偽申告すると Aria が「対応しているはずなのに
/// 404」を踏み、未申告だと「対応しているのに fallback」を踏む。新 endpoint を
/// router に足したら本配列にも追加すること。
const SUPPORTED_ENDPOINTS: &[&str] = &[
    // self-listing (= Misskey 本家の慣行で、`endpoints` 自身も含めて返す)。
    "endpoints",
    // instance probe (= Aria が login 直後に叩く一群、#168 / #170)。
    "meta",
    "stats",
    // session whoami (= #158)。
    "i",
    // in-app 通知フィード (= #206 PR2、Aria 通知タブ)。
    "i/notifications",
    "notifications/mark-all-as-read",
    // read endpoints (= #159)。
    "notes/show",
    "notes/timeline",
    "users/show",
    // **emojis** ── 本 issue (#176) の直接の動機。Aria がこの key を見て
    // `/api/emojis` を使うか fallback (= `/api/meta.emojis`) かを判定する。
    "emojis",
    // write endpoints (= #160)。
    "notes/create",
    "notes/delete",
    "notes/renote",
    "notes/reactions/create",
    "notes/reactions/delete",
    "following/create",
    "following/delete",
    "drive/files/create",
    "drive/files",
    "drive/files/show",
];

/// `POST /api/endpoints` handler。**認証不要** (= Misskey 本家も anonymous で
/// 返す慣行、サーバが何に対応しているかは公開情報)。
pub async fn handle() -> Response {
    Json(SUPPORTED_ENDPOINTS).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn endpoints_list_includes_emojis() {
        // 本 PR の直接の動機 ── Aria が `.contains("emojis")` で判定する。
        assert!(
            SUPPORTED_ENDPOINTS.contains(&"emojis"),
            "endpoints list must contain 'emojis' to make Aria use /api/emojis"
        );
    }

    #[test]
    fn endpoints_list_includes_core() {
        // Misskey クライアント (= Aria / Milktea / MissRirica) が必ず叩く
        // 主要 endpoint も列挙されていること。
        for required in &["endpoints", "meta", "i", "notes/timeline", "users/show"] {
            assert!(
                SUPPORTED_ENDPOINTS.contains(required),
                "endpoints list must contain '{required}'"
            );
        }
    }

    #[test]
    fn endpoints_list_no_leading_slash() {
        // Misskey 慣行: `/api/` プレフィックスは無し、`notes/timeline` の形式。
        // 先頭 slash や `api/` プレフィックスが混ざると client 側 contains 判定が
        // 失敗するので、本テストで形式を担保。
        for ep in SUPPORTED_ENDPOINTS {
            assert!(
                !ep.starts_with('/') && !ep.starts_with("api/"),
                "endpoint '{ep}' must not start with '/' or 'api/'"
            );
        }
    }

    #[test]
    fn endpoints_list_serializes_as_top_level_array() {
        // wire shape (= top-level JSON array、envelope なし)。
        // `{endpoints: [...]}` だと Aria の `.cast<String>()` が失敗する。
        let v: Value = serde_json::to_value(SUPPORTED_ENDPOINTS).unwrap();
        assert!(
            v.is_array(),
            "endpoints response must be a top-level JSON array"
        );
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty(), "endpoints list must not be empty");
        for item in arr {
            assert!(
                item.is_string(),
                "each endpoint entry must be a string; got {item:?}"
            );
        }
    }
}
