//! `POST /api/meta` ── Misskey 互換インスタンス情報 (= #168 / 親 #150)。
//!
//! Misskey クライアント (Milktea / `MissRirica` / Iceshrimp Web) は **server URL
//! を入力した瞬間** に最初に叩く endpoint。レスポンスから `features.miauth`
//! フラグ + `maxNoteTextLength` + `policies.*` 等を読んで login UI を構成する。
//! これが無いと "サーバ情報を取得できません" で login フローが止まる。
//!
//! ## AGPL discipline
//!
//! 一次資料は [misskey-hub.net](https://misskey-hub.net/) /
//! [api-doc.misskey.io](https://api-doc.misskey.io/) と
//! [4ster1sk/nkv-proxy](https://github.com/4ster1sk/nkv-proxy)
//! (`app/api/mk/meta.py`) のみ。Misskey 本体 (AGPL-3.0) の TypeScript handler
//! は参照していない (= `[[agpl-discipline-miauth]]`)。レスポンスフィールド名
//! は **interface** (= 著作権対象外、Oracle v Google) なので独自に書き起こす。
//!
//! ## スコープ
//!
//! Misskey の `/api/meta` は 60+ field あるが、本実装は **Sakurasato のお一人様
//! 構成で意味を持つ field のみ** を返す:
//!
//! - 基本情報: `name` / `version` / `uri` / `description` / `repositoryUrl`
//! - 投稿制約: `maxNoteTextLength` / `policies.maxFileSizeMb`
//! - 認証フラグ: `features.miauth: true` (= 必須)
//! - 登録/captcha: 全て無効 (= お一人様、登録不可)
//! - timeline: `features.localTimeline` / `globalTimeline` 共に false
//!   (= お一人様で意味が薄い、UI 表示しない指示)
//!
//! 詳細フィールド (`ads` / `clientOptions` / `defaultLightTheme` 等) は
//! `null` / 空配列で埋める ── Misskey クライアントは未知 / null field を
//! gracefully に扱う設計。

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::miauth::nodeinfo::build_base_url_pub;
use crate::state::AppState;

/// `POST /api/meta` の body。Misskey 公式は `detail: bool` を受け取り、
/// false のときに一部 field を hide する仕様だが、現状は **常に detail=true
/// 相当** を返す (= clients の大半は detail を渡さない / true を期待)。
/// `serde(default)` で空 body にも対応する。
#[derive(Debug, Default, Deserialize)]
pub struct MetaBody {
    #[serde(default)]
    pub detail: Option<bool>,
}

/// `POST /api/meta` handler。認証不要 (= login 前に叩かれる)。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MetaBody>>,
) -> Response {
    let _ = body; // detail は currently unused (常に detail=true 相当)
    let cfg = state.config();
    let info = &cfg.server.info;

    // URI は request の Host header から組み立てる (= nodeinfo discovery と
    // 同じ理由。tailscale 越し client が tailnet host を見るため)。
    let uri = build_base_url_pub(&headers, &cfg.server.host);

    // 表示名 (= name) は ServerInfo::name 優先、なければ host を使う。
    let name = info.name.clone().unwrap_or_else(|| cfg.server.host.clone());

    // 管理者連絡先は `mailto:` prefix を剥がして渡す (= Misskey 仕様は
    // メールアドレス単体、URI ではない)。
    let maintainer_email = info
        .admin_contact
        .as_deref()
        .and_then(|s| s.strip_prefix("mailto:"))
        .map(str::to_string);

    // upload 上限 (バイト) を MiB に丸めて返す。
    let max_file_size_mb = max_file_size_mb_from_bytes(cfg.media_proxy.max_bytes);

    let repository_url = info
        .repository_url
        .clone()
        .unwrap_or_else(|| "https://github.com/nananek/sakurasato".to_string());

    let body = build_meta(&MetaInputs {
        name,
        version: env!("CARGO_PKG_VERSION"),
        uri,
        description: info.description.clone(),
        maintainer_name: info.admin_name.clone(),
        maintainer_email,
        tos_url: info.terms_url.clone(),
        repository_url,
        theme_color: info.theme_color.clone(),
        banner_url: info.banner_url.clone(),
        max_file_size_mb,
    });
    Json(body).into_response()
}

/// 内部用 ── `build_meta` の引数を集約。test から独立に呼べるよう公開。
pub(crate) struct MetaInputs {
    pub name: String,
    pub version: &'static str,
    pub uri: String,
    pub description: Option<String>,
    pub maintainer_name: Option<String>,
    pub maintainer_email: Option<String>,
    pub tos_url: Option<String>,
    pub repository_url: String,
    pub theme_color: Option<String>,
    pub banner_url: Option<String>,
    pub max_file_size_mb: u64,
}

/// `bytes` → MiB。切り上げで `bytes` を **下回らない** MiB 値を返す
/// (= upload 上限の意味的安全側)。0 入力は 0 を返す。
fn max_file_size_mb_from_bytes(bytes: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    bytes.div_ceil(MIB)
}

/// レスポンス JSON を組み立てる。
///
/// Misskey 仕様の field 名と型 (= interface) を踏襲。Sakurasato の構成に応じた
/// 値で埋める ── 詳細は module-level doc の「スコープ」参照。
///
/// `json!{}` の `recursion_limit` を回避するため、top-level / `policies` /
/// `features` / `clientOptions` を **別関数に分割** して `serde_json::Value`
/// の `Object` として組み立て、最後に top-level に挿入する。
fn build_meta(i: &MetaInputs) -> Value {
    let policies = build_policies(i.max_file_size_mb);
    let features = build_features();
    let mut v = build_meta_top(i);
    if let Value::Object(ref mut map) = v {
        map.insert("policies".to_string(), policies);
        map.insert("features".to_string(), features);
        map.insert("clientOptions".to_string(), json!({}));
    }
    v
}

/// top-level field のみ。`policies` / `features` / `clientOptions` は別関数で
/// 組み立てた `Value` を後から挿入する (= `recursion_limit` 回避)。
fn build_meta_top(i: &MetaInputs) -> Value {
    json!({
        "maintainerName": i.maintainer_name,
        "maintainerEmail": i.maintainer_email,
        "version": i.version,
        "providesTarball": false,
        "name": i.name,
        "shortName": Value::Null,
        "uri": i.uri,
        "description": i.description,
        "langs": [],
        "tosUrl": i.tos_url,
        "repositoryUrl": i.repository_url,
        "feedbackUrl": "https://github.com/nananek/sakurasato/issues",
        "impressumUrl": Value::Null,
        "privacyPolicyUrl": Value::Null,
        "inquiryUrl": Value::Null,
        "disableRegistration": true,
        "emailRequiredForSignup": false,
        "enableHcaptcha": false,
        "hcaptchaSiteKey": Value::Null,
        "enableMcaptcha": false,
        "mcaptchaSiteKey": Value::Null,
        "mcaptchaInstanceUrl": Value::Null,
        "enableRecaptcha": false,
        "recaptchaSiteKey": Value::Null,
        "enableTurnstile": false,
        "turnstileSiteKey": Value::Null,
        "enableTestcaptcha": false,
        "googleAnalyticsMeasurementId": Value::Null,
        "swPublickey": Value::Null,
        "themeColor": i.theme_color,
        "mascotImageUrl": Value::Null,
        "bannerUrl": i.banner_url,
        "infoImageUrl": Value::Null,
        "serverErrorImageUrl": Value::Null,
        "notFoundImageUrl": Value::Null,
        "iconUrl": Value::Null,
        "backgroundImageUrl": Value::Null,
        "logoImageUrl": Value::Null,
        "maxNoteTextLength": 3000,
        "defaultLightTheme": Value::Null,
        "defaultDarkTheme": Value::Null,
        "ads": [],
        "notesPerOneAd": 0,
        "enableEmail": false,
        "enableServiceWorker": false,
        "translatorAvailable": false,
        "serverRules": [],
        "sentryForFrontend": Value::Null,
        "mediaProxy": Value::Null,
        "enableUrlPreview": false,
        "noteSearchableScope": "local",
        "federation": "all",
        "cacheRemoteFiles": true,
        "cacheRemoteSensitiveFiles": false,
        "requireSetup": false,
        "proxyAccountName": Value::Null,
    })
}

/// `policies` object (= お一人様前提のデフォルト)。
fn build_policies(max_file_size_mb: u64) -> Value {
    json!({
        "gtlAvailable": false,
        "ltlAvailable": false,
        "canPublicNote": true,
        "mentionLimit": 20,
        "canInvite": false,
        "inviteLimit": 0,
        "inviteLimitCycle": 10080,
        "inviteExpirationTime": 0,
        "canManageCustomEmojis": false,
        "canManageAvatarDecorations": false,
        "canSearchNotes": false,
        "canSearchUsers": false,
        "canUseTranslator": false,
        "canHideAds": true,
        "driveCapacityMb": 0,
        "maxFileSizeMb": max_file_size_mb,
        "alwaysMarkNsfw": false,
        "canUpdateBioMedia": true,
        "pinLimit": 5,
        "antennaLimit": 0,
        "antennaNotesLimit": 0,
        "wordMuteLimit": 0,
        "webhookLimit": 0,
        "clipLimit": 0,
        "noteEachClipsLimit": 0,
        "userListLimit": 0,
        "userEachUserListsLimit": 0,
        "rateLimitFactor": 1,
        "avatarDecorationLimit": 0,
        "canImportAntennas": false,
        "canImportBlocking": false,
        "canImportFollowing": false,
        "canImportMuting": false,
        "canImportUserLists": false,
        "chatAvailability": "unavailable",
        "uploadableFileTypes": [
            "image/jpeg", "image/png", "image/gif", "image/webp", "image/avif"
        ],
        "noteDraftLimit": 0,
        "scheduledNoteLimit": 0,
        "watermarkAvailable": false,
        "fileSizeLimit": max_file_size_mb,
    })
}

/// `features` object (= client が「MiAuth 経路を使うか」判定する flag 群)。
fn build_features() -> Value {
    json!({
        "localTimeline": false,
        "globalTimeline": false,
        "registration": false,
        "emailRequiredForSignup": false,
        "hcaptcha": false,
        "recaptcha": false,
        "turnstile": false,
        "objectStorage": true,
        "serviceWorker": false,
        "miauth": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inputs() -> MetaInputs {
        MetaInputs {
            name: "Sakurasato Test".into(),
            version: "test",
            uri: "https://foo.tailnet.ts.net:8443".into(),
            description: Some("お一人様".into()),
            maintainer_name: Some("neko".into()),
            maintainer_email: Some("neko@example.com".into()),
            tos_url: None,
            repository_url: "https://github.com/nananek/sakurasato".into(),
            theme_color: Some("#ffb7c5".into()),
            banner_url: None,
            max_file_size_mb: 25,
        }
    }

    #[test]
    fn build_meta_contains_required_top_level_fields() {
        let v = build_meta(&sample_inputs());
        // login probe で client が触る最低限の field を必ず存在検査。
        for key in &[
            "name",
            "version",
            "uri",
            "description",
            "maintainerName",
            "maintainerEmail",
            "repositoryUrl",
            "feedbackUrl",
            "disableRegistration",
            "emailRequiredForSignup",
            "enableHcaptcha",
            "enableRecaptcha",
            "enableTurnstile",
            "maxNoteTextLength",
            "serverRules",
            "policies",
            "features",
        ] {
            assert!(v.get(*key).is_some(), "missing required field: {key}");
        }
    }

    #[test]
    fn build_meta_features_miauth_is_true() {
        let v = build_meta(&sample_inputs());
        let miauth = v
            .pointer("/features/miauth")
            .expect("features.miauth must be present");
        assert_eq!(*miauth, Value::Bool(true));
    }

    #[test]
    fn build_meta_policies_can_public_note_true() {
        let v = build_meta(&sample_inputs());
        assert_eq!(
            v.pointer("/policies/canPublicNote"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn build_meta_disables_registration_and_captcha() {
        let v = build_meta(&sample_inputs());
        assert_eq!(v["disableRegistration"], Value::Bool(true));
        assert_eq!(v["enableHcaptcha"], Value::Bool(false));
        assert_eq!(v["enableRecaptcha"], Value::Bool(false));
        assert_eq!(v["enableTurnstile"], Value::Bool(false));
        assert_eq!(v["enableMcaptcha"], Value::Bool(false));
    }

    #[test]
    fn max_file_size_mb_rounds_up() {
        // 25 MiB ちょうど
        assert_eq!(max_file_size_mb_from_bytes(25 * 1024 * 1024), 25);
        // 25 MiB + 1 byte → 26 MiB (= 上限を下回らない)
        assert_eq!(max_file_size_mb_from_bytes(25 * 1024 * 1024 + 1), 26);
        // 0 → 0
        assert_eq!(max_file_size_mb_from_bytes(0), 0);
    }

    #[test]
    fn build_meta_uri_uses_input() {
        let v = build_meta(&sample_inputs());
        assert_eq!(v["uri"], "https://foo.tailnet.ts.net:8443");
    }
}
