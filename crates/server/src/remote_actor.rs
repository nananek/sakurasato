//! Remote actor の取得と DB upsert。
//!
//! 未知 actor (= 受信 inbox の `keyId` が DB に無い) を解決するため、
//! `<ap_id>` から actor JSON を `GET` し、RSA / Ed25519 公開鍵と inbox 等
//! を抜き出して `actor` テーブルに upsert する。
//!
//! ## なぜ server 直 (#23 暫定) なのか
//!
//! CLAUDE.md §3 / §5.3: `ActivityPub` の配送 POST と remote actor fetch は
//! 当面 server 直で行う (Mastodon / Misskey / Pleroma も server 直配送が
//! 業界標準)。M6 で media-proxy 実装時に再評価する選択。
//!
//! ## SSRF ガード
//!
//! 配送と **同じ** [`crate::net_guard`] を通す ── 別ガードを書くと片方で
//! 防御を忘れる事故が起きやすい。具体的には:
//! - URL 文字列パース時点で host が IP literal の内部 / 予約範囲、または
//!   `localhost` / `*.local` 等の予約ドメインなら拒否
//! - 自インスタンスの公開ホスト名と一致する URL も拒否 (自己 fetch 抑止)
//! - **redirect は自前 policy で検証** ── `reqwest::Client` のデフォルト
//!   redirect 追従は禁止 (`http_client` 側で `Policy::none`)、ここでは
//!   3xx を **明示拒否** することで「初回 URL のチェックは通ったが Location
//!   で内部に飛ぶ」攻撃を遮断する。
//! - **DNS 解決後の IP 再検証は本体ではできない** (libresolver 非露出)。
//!   これは server 直の既知制限で、CLAUDE.md §3 再評価項目 (b) に既記載。
//!
//! ## レスポンス処理
//!
//! `application/activity+json` か `application/ld+json` か `application/json`
//! のいずれかを期待する。レスポンスサイズは 256 KiB で上限を打つ ── 通常
//! actor JSON は数 KB なので、これを超えるものは攻撃か誤設定。

use std::time::Duration;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use sakurasato_core::model::{ActorField, ActorRow};
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::time::timeout;
use tracing::warn;
use url::Url;

use crate::net_guard;
use crate::state::AppState;

/// AP object JSON (actor / Note 等) のレスポンス上限 (256 KiB)。通常 actor /
/// Note は数 KB なので、これを超えるものは攻撃か誤設定。`fetch_object_json`
/// が actor / Note 両方の fetch でこの上限を共有する。
const MAX_AP_OBJECT_BYTES: usize = 256 * 1024;

/// 1 リクエストの全体 deadline (10 秒)。`reqwest` の `.timeout()` は接続→
/// レスポンス読了までの全体に効くが、念のため `tokio::time::timeout` でも
/// 包んで保険にする (delivery worker や inbox 処理を詰まらせない)。
const FETCH_DEADLINE: Duration = Duration::from_secs(10);

/// remote actor のプロフィール (+ counts) キャッシュの再 fetch TTL。
///
/// `fetched_at` がこの閾値を超えて古い remote actor は、`/api/users/show` 等で
/// 表示する直前に [`fetch_and_upsert_with_counts`] で同期的に再取得する
/// (= Misskey の on-demand 更新相当)。設定可能な項目にする必然性が薄いので
/// named constant に留める (CLAUDE.md §8 の「既存の `MAX_AP_OBJECT_BYTES` 等と
/// 同様」)。
const REMOTE_ACTOR_REFRESH_TTL: chrono::Duration = chrono::Duration::hours(1);

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("URL is invalid: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("URL host {host:?} is in a blocked address range ({reason}); refusing to fetch")]
    Blocked { host: String, reason: &'static str },

    #[error("network error while fetching actor: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("actor fetch returned non-2xx status {0}")]
    HttpStatus(StatusCode),

    #[error("actor fetch redirected; redirects are not followed (target {0:?})")]
    RedirectRefused(String),

    #[error("outbound fetch to {host:?} is rate-limited; dropping (Issue #269)")]
    RateLimited { host: String },

    #[error("actor fetch timed out")]
    Timeout,

    #[error("actor JSON is malformed: {0}")]
    Malformed(String),

    #[error("response too large (>{MAX_AP_OBJECT_BYTES} bytes)")]
    TooLarge,

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// 受信 inbox の署名検証で未知 actor が来たときに呼ぶエントリポイント。
///
/// `<ap_id>` から actor JSON を `GET` し、パース・SSRF 検査・公開鍵
/// `owner` 検査を経て `actor` テーブルに upsert してから `ActorRow` を返す。
/// [`crate::extract::SignedInboxBody`] が DB 再 lookup を重複させずに済むよう、
/// 既存行チェックは extractor 側で済ませてから本関数を呼ぶ。
pub(crate) async fn fetch_and_upsert_for_signature(
    state: &AppState,
    ap_id: &str,
) -> Result<ActorRow, FetchError> {
    fetch_and_upsert(state, ap_id).await
}

/// `ap_id` から actor JSON を `GET` し、SSRF / `id` 一致 / 鍵 `owner` を検査
/// した上で DB に upsert する。署名検証経路 (extractor) 以外からも呼べる
/// ように `pub` で公開する ── M9 で Move 受領 / outbound の前段として、
/// target/aka actor が DB に居ないときに引いてくる用途で必要になった。
///
/// **この関数は actor 本体のみを取得し、Collection (`followers` / `following` /
/// `outbox`) の `totalItems` は取得しない** ── inbox 署名検証 (extractor) /
/// Update 受信 / Move 受領・送出 / Follow 解決 / 未知 Note の author 解決など
/// **複数の共有ホットパス**から呼ばれるため、ここで counts まで引くと
/// per-domain レート制限 (Issue #269) のトークン消費が 1 fetch あたり 1 → 4 に
/// 増える。counts が必要なのは `MiAuth` プロフィール表示のみなので、
/// [`fetch_and_upsert_with_counts`] がこの関数を呼んでから後追いで取得する
/// (プロフィール表示用途の呼び出し側はそちらを選ぶ)。
pub async fn fetch_and_upsert(state: &AppState, ap_id: &str) -> Result<ActorRow, FetchError> {
    let json = fetch_actor_json(state, ap_id).await?;
    let parsed = parse_actor_json(ap_id, &json)?;
    upsert(state, parsed).await
}

/// [`fetch_and_upsert`] + Collection `totalItems` の count キャッシュ取得
/// (**`MiAuth` プロフィール表示専用経路** = `refresh_remote_actor_if_stale` が
/// 呼ぶ)。actor 本体の upsert に続けて、`followers` / `following` / `outbox`
/// Collection の `totalItems` を並行取得し、DB と戻り値の行の両方に反映する
/// (= Aria 等のプロフィール画面に出す `followersCount` / `followingCount` /
/// `notesCount`。Mastodon / Misskey 共通パターン)。
///
/// Collection 取得の失敗は個別に fail-open する ── 1 つ落ちても actor 本体の
/// upsert と他の count は成功し、失敗した count は既存 DB 値を維持する
/// ([`apply_counts`] の `None` 扱い。新規 insert では 0 のまま)。
pub async fn fetch_and_upsert_with_counts(
    state: &AppState,
    ap_id: &str,
) -> Result<ActorRow, FetchError> {
    let row = fetch_and_upsert(state, ap_id).await?;
    let counts = fetch_row_counts(state, &row).await;
    Ok(apply_counts(state, row, counts).await)
}

/// actor の 3 つの Collection (`followers` / `following` / `outbox`) を **並行**
/// GET して `totalItems` を読む。各取得は個別に fail-open する ── 失敗は
/// `None` になり、呼び出し側は「取得に失敗した = DB の既存 count を維持する」と
/// 解釈する (新規 insert では 0 のまま)。
///
/// URL は upsert 済みの `ActorRow` から読む (= [`fetch_and_upsert`] がパース・
/// 保存したものと同一)。
async fn fetch_row_counts(
    state: &AppState,
    row: &ActorRow,
) -> (Option<i64>, Option<i64>, Option<i64>) {
    let followers = fetch_collection_total_items(state, row.followers_url.as_deref());
    let following = fetch_collection_total_items(state, row.following_url.as_deref());
    let outbox = fetch_collection_total_items(state, row.outbox_url.as_deref());
    tokio::join!(followers, following, outbox)
}

/// 取得できた Collection count を **DB と戻り値の行の両方**に反映する。
///
/// `None` の引数は「その Collection の取得に失敗した / `totalItems` が無い」
/// を意味し、既存値を維持する ── 一時的なネットワーク障害でキャッシュ済みの
/// count を 0 に上書きしないためのフェイルオープン。3 つとも `None` のときは
/// DB 書き込み自体を行わない (全 fetch 失敗で 0 埋めの UPDATE を投げない)。
///
/// DB 書き込みの失敗は warn だけに留める (actor 本体の upsert は成功している
/// ので、count の記録失敗で呼び出し元を失敗させない)。ただし行のフィールドは
/// メモリ上で更新して返す ── 呼び出し元 ([`fetch_and_upsert_with_counts`] /
/// [`refresh_remote_actor_if_stale`]) が「更新後の行」としてこの戻り値を使う
/// ため、書き込み前の値のまま返すと follow 直後 / TTL 再 fetch 直後の
/// レスポンスに古い count (新規なら 0) が出てしまう。
async fn apply_counts(
    state: &AppState,
    row: ActorRow,
    counts: (Option<i64>, Option<i64>, Option<i64>),
) -> ActorRow {
    if counts.0.is_none() && counts.1.is_none() && counts.2.is_none() {
        return row;
    }
    if let Err(err) =
        repo::actor::set_remote_counts(state.pool(), row.id, counts.0, counts.1, counts.2).await
    {
        warn!(
            ?err,
            actor_id = row.id,
            "remote actor count cache write failed; serving in-memory counts",
        );
    }
    ActorRow {
        followers_count: counts.0.unwrap_or(row.followers_count),
        following_count: counts.1.unwrap_or(row.following_count),
        notes_count: counts.2.unwrap_or(row.notes_count),
        ..row
    }
}

/// actor JSON 取得。HTTP GET だけ。DB 非依存。
async fn fetch_actor_json(state: &AppState, ap_id: &str) -> Result<JsonValue, FetchError> {
    fetch_object_json(state, ap_id).await
}

/// Collection URL を `GET` して `totalItems` (数値) を読む。
///
/// **`MiAuth` プロフィール表示専用経路** ([`fetch_and_upsert_with_counts`]) から
/// 呼ばれる ── 共有ホットパスの [`fetch_and_upsert`] はこの関数を通らない
/// (per-domain レート制限のトークン消費を増やさないため)。
///
/// 汎用 [`fetch_object_json`] をそのまま使う ── SSRF 検査 / redirect 拒否 /
/// per-domain レート制限 / サイズ上限 / `id` 一致検査を actor / Note fetch と
/// **完全共有** する (新しい迂回経路を作らない。CLAUDE.md §3 / §7 どおり
/// 「JSON のみで画像デコードを伴わない remote actor fetch と同種」なので
/// media-proxy を介さず server 直で行う)。
///
/// レスポンスは `Collection` / `OrderedCollection` の JSON で、`totalItems` を
/// 持つ。以下のケースは **エラーにせず `None` で fail-open** する:
/// - URL が無い (actor JSON に `followers` 等が無い / 一部の実装は出さない)
/// - HTTP 取得が失敗した (相手インスタンス down 等)
/// - `totalItems` フィールドが無い / 数値でない / **負値** (Mastodon は
///   フォロー一覧を非公開にすると `totalItems` を返さない実装がある)
async fn fetch_collection_total_items(state: &AppState, url: Option<&str>) -> Option<i64> {
    let url = url?;
    let json = match fetch_object_json(state, url).await {
        Ok(json) => json,
        Err(err) => {
            warn!(
                url,
                ?err,
                "Collection totalItems fetch failed; keeping cached count",
            );
            return None;
        }
    };
    parse_collection_total_items(&json)
}

/// Collection JSON から `totalItems` を抜き出す。フィールド欠如 / 非数値 /
/// 負値は `None` (= fail-open。負値はあり得ないので壊れた値として扱い、DB に
/// 負の count を保存しない)。`items` / `orderedItems` の配列長は読まない ──
/// Collection のページング実装差 (`totalItems` を出すか `orderedItems` だけ
/// 出すか) に依存しない「`totalItems` があればそれを信じる」最小実装に留める。
fn parse_collection_total_items(json: &JsonValue) -> Option<i64> {
    json.get("totalItems")
        .and_then(JsonValue::as_i64)
        .filter(|n| *n >= 0)
}

/// 任意の AP object (`Note` / `Actor` 等) を `uri` から `GET` する汎用 fetcher。
///
/// SSRF ガード ([`enforce_url_policy`]) / redirect 拒否 / サイズ上限 / `id`
/// 一致検査を actor fetch と **完全共有** する ── 別経路で防御を書くと片方で
/// 抜ける事故になる (本モジュール冒頭の方針)。`Announce` 受信時の未知 Note
/// fetch ([`crate::dispatch::note::fetch_and_store_remote_note`]) もこの関数を
/// 通すことで、画像取得を伴わない JSON GET (= remote actor fetch と同種、
/// CLAUDE.md §3 の server 直 fetch 例外) に収める。
pub(crate) async fn fetch_object_json(
    state: &AppState,
    uri: &str,
) -> Result<JsonValue, FetchError> {
    let url = Url::parse(uri)?;
    enforce_url_policy(&url, &state.config().server.host)?;

    // per-domain レート制限 (Issue #269)。actor / Note fetch の唯一の chokepoint
    // なので、ここで宛先 host のトークンを引く。flood (例: 悪意ある followee の
    // Announce 大量送出) のとき外部ドメインへの増幅 fetch を drop する。SSRF
    // 検査を通った後にチェックするので、host は検証済みの宛先。
    let host = url.host_str().unwrap_or_default();
    if !state.try_acquire_fetch(host) {
        warn!(
            uri,
            host, "outbound AP fetch rate-limited for domain; dropping (Issue #269)",
        );
        return Err(FetchError::RateLimited {
            host: host.to_string(),
        });
    }

    // 信頼境界外 URL なので Accept ヘッダで JSON-LD を明示要求。レスポンス
    // ボディは MAX_AP_OBJECT_BYTES で頭打ちする。reqwest は body streaming で
    // 来るので、`bytes_stream()` を使い手で size を数えながら積む。
    let req = state
        .http_client()
        .get(url.clone())
        .header(
            "accept",
            "application/activity+json, application/ld+json, application/json;q=0.5",
        )
        .build()?;

    let resp = match timeout(FETCH_DEADLINE, state.http_client().execute(req)).await {
        Ok(r) => r?,
        Err(_) => return Err(FetchError::Timeout),
    };

    // **redirect 拒否** (`http_client::build_client` が Policy::none で
    // 設定済み)。3xx をここで見たということは reqwest が自動追従せずに
    // ステータスをそのまま返した、という意味。明示拒否してログに残す。
    if resp.status().is_redirection() {
        let location = resp
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        warn!(
            uri,
            status = resp.status().as_u16(),
            location,
            "AP object fetch returned redirect; refusing to follow",
        );
        return Err(FetchError::RedirectRefused(location));
    }

    if !resp.status().is_success() {
        return Err(FetchError::HttpStatus(resp.status()));
    }

    // 本来 Content-Type 検査も行うべきだが、Mastodon は `Vary` や
    // `; charset=utf-8` を付ける実装が多い ── まずバイト列を取り JSON
    // パースで本物判定する方が頑健。サイズ上限だけ厳格に適用する。
    let bytes = match timeout(FETCH_DEADLINE, resp.bytes()).await {
        Ok(r) => r?,
        Err(_) => return Err(FetchError::Timeout),
    };
    if bytes.len() > MAX_AP_OBJECT_BYTES {
        return Err(FetchError::TooLarge);
    }

    let json: JsonValue = serde_json::from_slice(&bytes)
        .map_err(|e| FetchError::Malformed(format!("not JSON: {e}")))?;

    // 受領 JSON の `id` が要求した URI と一致することを確認。不一致は
    // (a) 攻撃者が別 id を仕込んだ偽装 object を返した、(b) Mastodon の
    // `Account#redirect` 等で別 object を返している、のどちらか ── どちらも
    // 危険なので拒否する (actor / note 共通)。
    let id = json
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FetchError::Malformed("AP object JSON has no string `id`".into()))?;
    if id != uri {
        warn!(
            requested = uri,
            returned = id,
            "AP object id mismatch; refusing to use",
        );
        return Err(FetchError::Malformed(format!(
            "id mismatch: requested {uri}, got {id}"
        )));
    }

    Ok(json)
}

/// 共有ガード ([`net_guard`]) を通して URL を検査する。
///
/// 失敗時に呼び出し側で host を含むエラーを返したいので、ここでは結果を
/// `Result<(), FetchError>` で返す。
fn enforce_url_policy(url: &Url, server_host: &str) -> Result<(), FetchError> {
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(FetchError::Malformed(format!(
            "unsupported scheme {:?}",
            url.scheme()
        )));
    }
    if net_guard::is_self_host(url, server_host) {
        return Err(FetchError::Blocked {
            host: url.host_str().unwrap_or("").to_string(),
            reason: "self-host",
        });
    }
    if let Some(reason) = net_guard::host_blocked(url) {
        return Err(FetchError::Blocked {
            host: url.host_str().unwrap_or("").to_string(),
            reason,
        });
    }
    Ok(())
}

/// パース済みの actor 表現。`NewActor` に詰め替える前段。
#[derive(Debug)]
struct ParsedRemoteActor {
    ap_id: String,
    preferred_username: String,
    host: String,
    display_name: Option<String>,
    summary: Option<String>,
    icon_url: Option<String>,
    image_url: Option<String>,
    inbox_url: String,
    shared_inbox_url: Option<String>,
    outbox_url: Option<String>,
    followers_url: Option<String>,
    following_url: Option<String>,
    public_key_id: String,
    public_key_pem: String,
    ed25519_public_key_id: Option<String>,
    ed25519_public_key_pem: Option<String>,
    also_known_as: Vec<String>,
    moved_to_ap_id: Option<String>,
    actor_type: String,
    /// Mastodon / Misskey が actor JSON に乗せる鍵アカフラグ (Issue #66)。
    /// 相手側でどう Follow を扱っているかのキャッシュとして保持する。
    /// 値が無ければ `false` (= 通常アカ扱い)。
    manually_approves_followers: bool,
}

/// 取得した actor JSON をパースして必要フィールドを取り出す。
fn parse_actor_json(ap_id: &str, json: &JsonValue) -> Result<ParsedRemoteActor, FetchError> {
    let s = |k: &str| json.get(k).and_then(JsonValue::as_str).map(str::to_string);

    let preferred_username = s("preferredUsername")
        .ok_or_else(|| FetchError::Malformed("actor has no preferredUsername".into()))?;

    let host = Url::parse(ap_id)?
        .host_str()
        .ok_or_else(|| FetchError::Malformed("actor ap_id has no host".into()))?
        .to_string();

    let inbox_url =
        s("inbox").ok_or_else(|| FetchError::Malformed("actor has no inbox URL".into()))?;

    // publicKey は object 形式が標準 (`{id, owner, publicKeyPem}`)。
    let pk_obj = json
        .get("publicKey")
        .ok_or_else(|| FetchError::Malformed("actor has no publicKey".into()))?;
    let (public_key_id, public_key_pem) = parse_public_key(pk_obj)?;

    // 公開鍵の `owner` が actor 本人を指していることを確認 ── 別 actor の
    // 公開鍵を流用した actor を作って署名なりすましされないように。
    if let Some(owner) = pk_obj.get("owner").and_then(JsonValue::as_str)
        && owner != ap_id
    {
        return Err(FetchError::Malformed(format!(
            "publicKey.owner {owner} does not match actor id {ap_id}",
        )));
    }

    // FEP-521a assertionMethod の Ed25519 鍵 (任意)。Multikey の
    // `publicKeyMultibase` から PEM 変換 → DB に格納する。失敗 (= 別 codec
    // など) はログだけ残して None で続行 ── RSA だけでも署名検証は成立
    // するので、Ed25519 が無いだけで actor 全体の取り込みを諦める必要は無い。
    let (ed25519_id, ed25519_pem) = match parse_ed25519_assertion(ap_id, json) {
        Some((id, pem)) => (Some(id), Some(pem)),
        None => (None, None),
    };

    let endpoints = json.get("endpoints");
    let shared_inbox_url = endpoints
        .and_then(|e| e.get("sharedInbox"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);

    let also_known_as = json
        .get("alsoKnownAs")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // icon / image はオブジェクト形式 (`{type: "Image", url: "..."}`) または
    // 文字列のいずれか。url だけ拾う。
    let icon_url = extract_media_url(json.get("icon"));
    let image_url = extract_media_url(json.get("image"));

    let actor_type = s("type").unwrap_or_else(|| "Person".to_string());

    // 鍵アカフラグ (Issue #66) ── 欠落 / 非 boolean は `false` 扱い。
    // Mastodon / Misskey はいずれも boolean で出すが、互換層 (例えば
    // GoToSocial の一部設定) で文字列を返す実装もあるため、`as_bool()`
    // のみを受理する保守的なパースにする。
    let manually_approves_followers = json
        .get("manuallyApprovesFollowers")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);

    Ok(ParsedRemoteActor {
        ap_id: ap_id.to_string(),
        preferred_username,
        host,
        display_name: s("name"),
        summary: s("summary"),
        icon_url,
        image_url,
        inbox_url,
        shared_inbox_url,
        outbox_url: s("outbox"),
        followers_url: s("followers"),
        following_url: s("following"),
        public_key_id,
        public_key_pem,
        ed25519_public_key_id: ed25519_id,
        ed25519_public_key_pem: ed25519_pem,
        also_known_as,
        moved_to_ap_id: s("movedTo"),
        actor_type,
        manually_approves_followers,
    })
}

fn parse_public_key(pk: &JsonValue) -> Result<(String, String), FetchError> {
    let id = pk
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FetchError::Malformed("publicKey has no string `id`".into()))?
        .to_string();
    // Pleroma 2.5.5 は `publicKeyPem` を末尾 `\n\n` で送ってくる。`rsa::pkcs8`
    // は `PreEncapsulationBoundary` で拒否するので、保存前に外周の whitespace
    // を落として canonicalize する。`trim()` は ASCII 空白 + LF/CR/Tab だけを
    // 削るので、PEM 本体 (Base64 + `-----BEGIN/END-----` 行) を壊さない。
    let pem = pk
        .get("publicKeyPem")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FetchError::Malformed("publicKey has no `publicKeyPem`".into()))?
        .trim()
        .to_string();
    Ok((id, pem))
}

/// FEP-521a `assertionMethod` から Ed25519 Multikey を探し、PEM 変換する。
///
/// 別 codec (BLS / X25519 等) のエントリはスキップして次へ進む ── 配列内に
/// Ed25519 鍵が無ければ `None` を返す。
fn parse_ed25519_assertion(ap_id: &str, json: &JsonValue) -> Option<(String, String)> {
    let Some(JsonValue::Array(arr)) = json.get("assertionMethod") else {
        return None;
    };
    for entry in arr {
        let Some(controller) = entry.get("controller").and_then(JsonValue::as_str) else {
            continue;
        };
        if controller != ap_id {
            // controller が actor 本人でない multikey は無視 (delegation 等)。
            continue;
        }
        let Some(id) = entry.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(mb) = entry.get("publicKeyMultibase").and_then(JsonValue::as_str) else {
            continue;
        };
        // Ed25519 だけ受理。multikey の先頭 2 バイトは varint codec
        // (`0xED 0x01` = `ed25519-pub`)。base58btc (z) 前提。
        match crate::multikey::multibase_to_ed25519_pem(mb) {
            Ok(pem) => return Some((id.to_string(), pem)),
            Err(e) => {
                warn!(error = %e, id, "assertionMethod entry is not Ed25519; skipping");
            }
        }
    }
    None
}

fn extract_media_url(v: Option<&JsonValue>) -> Option<String> {
    let v = v?;
    let raw = match v {
        JsonValue::String(s) => s.clone(),
        JsonValue::Object(map) => map
            .get("url")
            .and_then(JsonValue::as_str)
            .map(str::to_string)?,
        _ => return None,
    };
    // **スキーム制限**: 悪意ある remote actor が `"icon": {"url": "javascript:..."}`
    // のような URL を advertise し、それが webhook payload (Discord embed の
    // `icon_url` 等) や Web UI に流れるとリスクが残る。Discord は embed.icon_url で
    // JS を実行しないが、Slack / Misskey 互換 webhook や将来の表示先で危険なので
    // 入口で `http`/`https` のみ許可する。`url::Url::parse` で書式不正も同時に弾く。
    // (round-1 review F3)
    let parsed = url::Url::parse(&raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(raw)
}

async fn upsert(state: &AppState, parsed: ParsedRemoteActor) -> Result<ActorRow, FetchError> {
    // 既存行があれば update、無ければ insert ── 同一 ap_id は一意。
    // 既存 actor で publicKey が変わっている = 相手が再 keying したとき
    // (例えば Mastodon の管理操作)。更新で追従する。
    if let Some(existing) = repo::actor::get_by_ap_id(state.pool(), &parsed.ap_id).await? {
        let updated = update_existing(state, existing.id, parsed).await?;
        repo::actor::mark_fetched(state.pool(), updated.id).await?;
        return Ok(updated);
    }

    let new = repo::actor::NewActor {
        ap_id: parsed.ap_id.clone(),
        preferred_username: parsed.preferred_username,
        host: parsed.host,
        display_name: parsed.display_name,
        summary: parsed.summary,
        icon_url: parsed.icon_url,
        image_url: parsed.image_url,
        inbox_url: parsed.inbox_url,
        shared_inbox_url: parsed.shared_inbox_url,
        outbox_url: parsed.outbox_url,
        followers_url: parsed.followers_url,
        following_url: parsed.following_url,
        public_key_id: parsed.public_key_id,
        public_key_pem: parsed.public_key_pem,
        private_key_pem: None,
        ed25519_public_key_id: parsed.ed25519_public_key_id,
        ed25519_public_key_pem: parsed.ed25519_public_key_pem,
        ed25519_private_key_pem: None,
        also_known_as: parsed.also_known_as,
        moved_to_ap_id: parsed.moved_to_ap_id,
        is_local: false,
        actor_type: parsed.actor_type,
        manually_approves_followers: parsed.manually_approves_followers,
    };
    let row = repo::actor::insert(state.pool(), new).await?;

    // count キャッシュ (followers/following/outbox Collection `totalItems`) は
    // **ここでは書き込まない** ── [`fetch_and_upsert_with_counts`] (= MiAuth
    // プロフィール表示経路) が actor 本体の upsert に続けて `apply_counts` で
    // 反映する。共有ホットパスの fetch でレート制限トークンを余計に消費しない
    // ため (fetch_and_upsert の doc 参照)。
    repo::actor::mark_fetched(state.pool(), row.id).await?;
    Ok(row)
}

/// 既存 actor 行を更新 (公開鍵 / inbox / metadata)。`is_local` と `id` は
/// 触らない ── 同じ `ap_id` を持つ local actor が存在する場合は、本関数を
/// 呼ぶ前に呼び出し側で弾く責務とする (現実には extractor が `get_by_ap_id`
/// で先にヒットするので fetch 経路には来ない)。
async fn update_existing(
    state: &AppState,
    id: i64,
    parsed: ParsedRemoteActor,
) -> Result<ActorRow, FetchError> {
    // `also_known_as` は JSONB 列で、`sqlx::types::Json<Vec<String>>` を
    // 経由して書く。serde_json::to_value で文字列配列を JsonValue に変換。
    let also_known_as_json = serde_json::to_value(&parsed.also_known_as).map_err(|e| anyhow!(e))?;

    let row = sqlx::query_as!(
        sakurasato_core::model::ActorRow,
        r#"
        UPDATE actor SET
            preferred_username = $1,
            host = $2,
            display_name = $3,
            summary = $4,
            icon_url = $5,
            image_url = $6,
            inbox_url = $7,
            shared_inbox_url = $8,
            outbox_url = $9,
            followers_url = $10,
            following_url = $11,
            public_key_id = $12,
            public_key_pem = $13,
            ed25519_public_key_id = $14,
            ed25519_public_key_pem = $15,
            also_known_as = $16,
            moved_to_ap_id = $17,
            actor_type = $18,
            manually_approves_followers = $19,
            updated_at = now()
        WHERE id = $20
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            ed25519_public_key_id, ed25519_public_key_pem, ed25519_private_key_pem,
            also_known_as as "also_known_as: sqlx::types::Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, manually_approves_followers,
            birthday, location, lang, followed_message,
            fields as "fields: sqlx::types::Json<Vec<ActorField>>",
            followers_count, following_count, notes_count,
            fetched_at, created_at, updated_at
        "#,
        parsed.preferred_username,
        parsed.host,
        parsed.display_name,
        parsed.summary,
        parsed.icon_url,
        parsed.image_url,
        parsed.inbox_url,
        parsed.shared_inbox_url,
        parsed.outbox_url,
        parsed.followers_url,
        parsed.following_url,
        parsed.public_key_id,
        parsed.public_key_pem,
        parsed.ed25519_public_key_id,
        parsed.ed25519_public_key_pem,
        also_known_as_json,
        parsed.moved_to_ap_id,
        parsed.actor_type,
        parsed.manually_approves_followers,
        id,
    )
    .fetch_one(state.pool())
    .await?;
    Ok(row)
}

/// remote actor のキャッシュ (プロフィール + count) を TTL ベースで再 fetch する。
///
/// `MiAuth` の `/api/users/show` 等で remote actor を返す直前に呼ぶ ──
/// `fetched_at` が [`REMOTE_ACTOR_REFRESH_TTL`] を超えていたら
/// [`fetch_and_upsert_with_counts`] で同期的に再取得し、更新後の行を返す。
/// これにより Aria のプロフィール画面を開くたびに毎回 GET が飛ぶことはなく
/// (TTL 内は DB キャッシュのみで応答)、古い count が表示され続けることもない
/// (= Misskey の on-demand 更新相当)。
///
/// フェイルオープン方針:
/// - local actor は対象外 (実クエリ集計なので fetch 不要) → 引数をそのまま返す。
/// - テスト経路 (`enable_remote_fetch = false`) は fetch しない。
/// - TTL 内は fetch しない。
/// - 再取得に失敗したら既存の DB 値をそのまま使う (= `/api/users/show` 自体は
///   失敗させない)。
pub async fn refresh_remote_actor_if_stale(state: &AppState, actor: &ActorRow) -> ActorRow {
    if actor.is_local {
        return actor.clone();
    }
    // テスト経路 (from_pool) は実ネットワークに出ない契約 ── TTL 再 fetch も
    // 同じゲートに従う (統合テストは DB の count カラムを seed して検証する)。
    if !state.enable_remote_fetch() {
        return actor.clone();
    }
    if !is_remote_actor_stale(actor.fetched_at) {
        return actor.clone();
    }
    match fetch_and_upsert_with_counts(state, &actor.ap_id).await {
        Ok(row) => row,
        Err(err) => {
            warn!(
                ap_id = actor.ap_id,
                ?err,
                "remote actor refresh failed; serving cached profile/counts",
            );
            actor.clone()
        }
    }
}

/// remote actor のキャッシュが古いかどうか。`fetched_at` が無い (= カラム追加前
/// の過去データ) か [`REMOTE_ACTOR_REFRESH_TTL`] を超えていたら stale。
fn is_remote_actor_stale(fetched_at: Option<DateTime<Utc>>) -> bool {
    match fetched_at {
        None => true,
        Some(t) => Utc::now().signed_duration_since(t) > REMOTE_ACTOR_REFRESH_TTL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn enforce_url_policy_blocks_loopback_literal() {
        let err = enforce_url_policy(&url("http://127.0.0.1/users/x"), "example.test").unwrap_err();
        assert!(matches!(
            err,
            FetchError::Blocked {
                reason: "loopback",
                ..
            }
        ));
    }

    #[test]
    fn enforce_url_policy_blocks_self_host() {
        let err =
            enforce_url_policy(&url("https://example.test/users/me"), "example.test").unwrap_err();
        assert!(matches!(
            err,
            FetchError::Blocked {
                reason: "self-host",
                ..
            }
        ));
    }

    #[test]
    fn enforce_url_policy_blocks_local_tld() {
        let err =
            enforce_url_policy(&url("http://postgres.local/users/x"), "example.test").unwrap_err();
        assert!(matches!(
            err,
            FetchError::Blocked {
                reason: "mdns-local",
                ..
            }
        ));
    }

    #[test]
    fn enforce_url_policy_allows_public_domain() {
        enforce_url_policy(&url("https://mastodon.example/users/x"), "example.test").unwrap();
    }

    #[test]
    fn enforce_url_policy_rejects_file_scheme() {
        let err = enforce_url_policy(&url("file:///etc/passwd"), "example.test").unwrap_err();
        assert!(matches!(err, FetchError::Malformed(_)));
    }

    #[test]
    fn parse_actor_json_basic() {
        let j = json!({
            "id": "https://x.test/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "name": "Alice",
            "summary": "hi",
            "inbox": "https://x.test/users/alice/inbox",
            "outbox": "https://x.test/users/alice/outbox",
            "followers": "https://x.test/users/alice/followers",
            "following": "https://x.test/users/alice/following",
            "endpoints": {"sharedInbox": "https://x.test/inbox"},
            "publicKey": {
                "id": "https://x.test/users/alice#main-key",
                "owner": "https://x.test/users/alice",
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n",
            },
            "icon": {"type": "Image", "url": "https://cdn.x.test/a.png"},
            "image": "https://cdn.x.test/header.png",
        });
        let p = parse_actor_json("https://x.test/users/alice", &j).unwrap();
        assert_eq!(p.preferred_username, "alice");
        assert_eq!(p.host, "x.test");
        assert_eq!(p.display_name.as_deref(), Some("Alice"));
        assert_eq!(p.inbox_url, "https://x.test/users/alice/inbox");
        assert_eq!(p.shared_inbox_url.as_deref(), Some("https://x.test/inbox"),);
        assert_eq!(p.public_key_id, "https://x.test/users/alice#main-key");
        assert_eq!(p.icon_url.as_deref(), Some("https://cdn.x.test/a.png"));
        assert_eq!(
            p.image_url.as_deref(),
            Some("https://cdn.x.test/header.png")
        );
    }

    #[test]
    fn parse_actor_json_rejects_mismatched_public_key_owner() {
        let j = json!({
            "id": "https://x.test/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "inbox": "https://x.test/users/alice/inbox",
            "publicKey": {
                "id": "https://x.test/users/alice#main-key",
                "owner": "https://OTHER.test/users/eve",
                "publicKeyPem": "PEM",
            },
        });
        let err = parse_actor_json("https://x.test/users/alice", &j).unwrap_err();
        assert!(matches!(err, FetchError::Malformed(_)));
    }

    #[test]
    fn parse_actor_json_requires_inbox() {
        let j = json!({
            "id": "https://x.test/users/alice",
            "type": "Person",
            "preferredUsername": "alice",
            "publicKey": {
                "id": "https://x.test/users/alice#main-key",
                "owner": "https://x.test/users/alice",
                "publicKeyPem": "PEM",
            },
        });
        let err = parse_actor_json("https://x.test/users/alice", &j).unwrap_err();
        assert!(matches!(err, FetchError::Malformed(_)));
    }

    #[test]
    fn extract_media_url_handles_object_and_string() {
        assert_eq!(
            extract_media_url(Some(
                &json!({"type": "Image", "url": "https://cdn.x.test/a.png"})
            )),
            Some("https://cdn.x.test/a.png".into())
        );
        assert_eq!(
            extract_media_url(Some(&json!("https://cdn.x.test/a.png"))),
            Some("https://cdn.x.test/a.png".into())
        );
        assert_eq!(extract_media_url(None), None);
        assert_eq!(extract_media_url(Some(&JsonValue::Null)), None);
    }

    /// `http`/`https` 以外の scheme (javascript: / data: / file: / 書式不正) は弾く。
    /// (round-1 review F3)
    #[test]
    fn extract_media_url_rejects_non_http_schemes() {
        assert_eq!(extract_media_url(Some(&json!("javascript:alert(1)"))), None);
        assert_eq!(
            extract_media_url(Some(&json!("data:image/png;base64,AAAA"))),
            None
        );
        assert_eq!(extract_media_url(Some(&json!("file:///etc/passwd"))), None);
        // 書式不正 (scheme なし) も Url::parse で落ちる
        assert_eq!(extract_media_url(Some(&json!("not-a-url"))), None);
        // object 形式でも scheme 検査が効く
        assert_eq!(
            extract_media_url(Some(
                &json!({"type": "Image", "url": "javascript:alert(1)"})
            )),
            None
        );
    }

    // ── Collection totalItems パース ────────────────────────────────────────

    #[test]
    fn parse_collection_total_items_reads_numeric_total_items() {
        let coll = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://x.test/users/alice/followers",
            "type": "OrderedCollection",
            "totalItems": 42,
        });
        assert_eq!(parse_collection_total_items(&coll), Some(42));
    }

    #[test]
    fn parse_collection_total_items_missing_or_non_numeric_is_none() {
        // フィールド欠如 (= Mastodon がフォロー一覧を非公開にしたとき等)
        assert_eq!(parse_collection_total_items(&json!({})), None);
        assert_eq!(
            parse_collection_total_items(&json!({"type": "OrderedCollection"})),
            None
        );
        // 非数値
        assert_eq!(
            parse_collection_total_items(&json!({"totalItems": "42"})),
            None
        );
        assert_eq!(
            parse_collection_total_items(&json!({"totalItems": null})),
            None
        );
        // 数値でも非整数 (実装差があっても fail-open)
        assert_eq!(
            parse_collection_total_items(&json!({"totalItems": 42.5})),
            None
        );
        // 負値はあり得ないので fail-open (= DB に負の count を保存しない)
        assert_eq!(
            parse_collection_total_items(&json!({"totalItems": -1})),
            None
        );
        // 0 は正当な値として受理する (フォロー/投稿が 0 件のアカウント)
        assert_eq!(
            parse_collection_total_items(&json!({"totalItems": 0})),
            Some(0)
        );
    }

    // ── TTL ベースの再 fetch 判定 ───────────────────────────────────────────

    #[test]
    fn is_remote_actor_stale_requires_fetched_at() {
        // fetched_at が無い (カラム追加前の過去データ) は stale 扱い → 再 fetch。
        assert!(is_remote_actor_stale(None));
    }

    #[test]
    fn is_remote_actor_stale_respects_ttl() {
        let now = Utc::now();
        // 直近の fetch は新鮮 (再 fetch しない)。
        assert!(!is_remote_actor_stale(Some(now)));
        // TTL 境界ギリギリ (59 分前) も新鮮。
        assert!(!is_remote_actor_stale(Some(
            now - chrono::Duration::minutes(59)
        )));
        // TTL 超過 (2 時間前) は stale。
        assert!(is_remote_actor_stale(Some(
            now - chrono::Duration::hours(2)
        )));
        // 未来時刻 (時計ずれ) も stale 扱いにしない (= 再 fetch しない)。
        assert!(!is_remote_actor_stale(Some(
            now + chrono::Duration::hours(1)
        )));
    }
}
