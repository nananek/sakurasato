//! 受領 `Create` ハンドラ (M11)。
//!
//! ## スコープ
//!
//! - 対象: inline `Note` を持つ Create のみ (`Article` 等は no-op)。
//! - 受け入れ条件:
//!   - **signer が我々の followee** (= local actor が signer を `accepted`
//!     で follow している) のとき。タイムラインに流す前提。
//!   - **または** activity / object の `to` / `cc` に我々の local actor が
//!     明示されている (= mention / reply / DM)。
//! - 上記いずれも満たさない post は debug ログのみで no-op ── public TL に
//!   流れる無関係 post を引き込むと DB が肥大するため (CLAUDE.md §5.1 / #55)。
//! - 重複 `ap_id` (= retry / duplicate delivery) は info で no-op。
//!
//! ## 信頼境界
//!
//! - F3 (body actor == signer) と nested object `attributedTo` 検査は
//!   [`super::dispatch`] で通過済み。ここでは signer が note の作者本人と
//!   みなしてよい。
//! - `Note.id` のホストが signer のホストと一致することは追加で検証する
//!   (= ホスト混入 spoofing 防御)。

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use sakurasato_core::model::{ActorRow, NoteRow, Visibility};
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::{debug, info};
use url::Url;

use super::DispatchError;
use crate::notification;
use crate::state::AppState;

/// AS2 の "public" 配送先 magic URI。
const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";

/// Note の content / summary の保護的上限 (DB 肥大対策)。local 投稿側の
/// 上限 (`local_api::notes`) と同じ値。リモートが滅多に超えないが、
/// `aaaa...` を流し込まれて DB を膨らませない保険。
const CONTENT_MAX: usize = 5000;
const SUMMARY_MAX: usize = 200;

pub(crate) async fn handle_create(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> Result<(), DispatchError> {
    let Some(JsonValue::Object(obj)) = activity.get("object") else {
        return Err(DispatchError::Malformed(
            "Create.object must be an inline object".into(),
        ));
    };

    let object_type = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if !object_type.eq_ignore_ascii_case("Note") {
        debug!(
            object_type,
            signer = %signer.ap_id,
            "Create: object is not a Note; ignoring",
        );
        return Ok(());
    }

    let note_ap_id = obj
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DispatchError::Malformed("Create.object.id missing".into()))?
        .to_string();

    same_host(&note_ap_id, &signer.ap_id, "Note id")
        .map_err(|e| DispatchError::Malformed(format!("{e:#}")))?;

    // 重複 (= retry / forwarding) は何も触らずに 202。
    if repo::note::get_by_ap_id(state.pool(), &note_ap_id)
        .await
        .with_context(|| format!("lookup note {note_ap_id}"))
        .map_err(DispatchError::Internal)?
        .is_some()
    {
        debug!(
            note_ap_id = %note_ap_id,
            signer = %signer.ap_id,
            "Create: note already stored; ignoring duplicate",
        );
        return Ok(());
    }

    let recipients = collect_recipients(obj, activity);
    let local_ap_id = format!(
        "https://{host}/users/{user}",
        host = state.config().server.host,
        user = state.config().server.user,
    );
    let addresses_us = recipients.iter_all().any(|r| r == &local_ap_id);
    let followed = !repo::follow::list_local_following(state.pool(), signer.id)
        .await
        .context("list_local_following for Create filter")
        .map_err(DispatchError::Internal)?
        .is_empty();

    if !addresses_us && !followed {
        debug!(
            note_ap_id = %note_ap_id,
            signer = %signer.ap_id,
            "Create: not from a followee and we are not addressed; ignoring",
        );
        return Ok(());
    }

    let new = build_remote_note(state, signer, &note_ap_id, obj, &recipients).await?;
    let inserted = repo::note::insert(state.pool(), new)
        .await
        .with_context(|| format!("insert remote note {note_ap_id}"))
        .map_err(DispatchError::Internal)?;

    let (quote_target, is_local_quote) = resolve_quote_target(state, obj).await;

    info!(
        note_id = inserted.id,
        note_ap_id = %note_ap_id,
        signer = %signer.ap_id,
        visibility = %inserted.visibility,
        addresses_us,
        followed,
        has_quote = quote_target.is_some(),
        is_local_quote,
        "remote note stored",
    );

    // Webhook 通知発火 (fire-and-forget)。失敗は内部 warn! のみで本筋に
    // 伝播しない。`is_local_quote` のときだけ quote target を渡す ── 第三者
    // の note を引用しているケースで誤って通知を撃たないため。
    let quote_for_notify = if is_local_quote {
        quote_target.as_ref()
    } else {
        None
    };
    notification::dispatch::notify_inbound_note(
        state,
        signer,
        &inserted,
        addresses_us,
        quote_for_notify,
    )
    .await;

    Ok(())
}

/// followee の `Announce` が参照する **未知 Note** を origin から fetch して
/// 取り込む (Issue #266)。取り込んだ [`NoteRow`] を返す。
///
/// M11 (#55) では「Announce 受信時の未知 Note 自動 fetch」を意図的にスコープ外
/// にしていた (他人の boost で見知らぬ note を引き込まないため) が、結果として
/// **フォローしていないアカウントの投稿への被リノートが表示されない** という
/// 報告につながった。`@nekono` 等フォロー済み著者の投稿だけ既知 = 表示される、
/// という散発挙動の正体。Mastodon / Misskey と同様に followee の Announce に
/// 限って fetch を許可してこれを解消する。呼び出し元
/// ([`super::announce::handle_announce`]) が signer = followee を保証する。
///
/// ## 防御
///
/// - fetch は [`crate::remote_actor::fetch_object_json`] 経由で SSRF / redirect
///   拒否 / サイズ上限 / `id` 一致を **actor fetch と共有**。画像デコードを伴わ
///   ない JSON GET なので CLAUDE.md §3 の server 直 fetch 例外に収まる。
/// - `attributedTo` のホストが Note の `id` ホストと一致することを検証
///   ([`same_host`]) ── 別ドメイン著者を騙る note の取り込み (なりすまし) を防ぐ。
/// - 著者 actor は DB → 無ければ [`crate::remote_actor::fetch_and_upsert`] で
///   解決 (鍵 / inbox / `id` 一致検査つき)。
/// - **public / unlisted のみ**取り込む。boost は公開コンテンツ前提であり、
///   万一 followers / direct な note を fetch できても第三者の非公開投稿を
///   表示しないようにする (privacy)。
pub(crate) async fn fetch_and_store_remote_note(
    state: &AppState,
    note_ap_id: &str,
) -> Result<NoteRow, DispatchError> {
    let json = crate::remote_actor::fetch_object_json(state, note_ap_id)
        .await
        .map_err(|e| DispatchError::Malformed(format!("fetch announced note: {e}")))?;
    let JsonValue::Object(obj) = json else {
        return Err(DispatchError::Malformed(
            "fetched announced object is not a JSON object".into(),
        ));
    };

    let object_type = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if !object_type.eq_ignore_ascii_case("Note") {
        return Err(DispatchError::Malformed(format!(
            "announced object is not a Note (type={object_type:?})"
        )));
    }

    // `id == note_ap_id` は fetch_object_json が保証済み。著者を解決する。
    let author_uri = extract_attributed_to(&obj)
        .ok_or_else(|| DispatchError::Malformed("announced note has no attributedTo".into()))?;
    // 別ドメイン著者を騙る note を弾く (= note の id ホスト == 著者ホスト)。
    same_host(note_ap_id, author_uri, "announced Note attributedTo")
        .map_err(|e| DispatchError::Malformed(format!("{e:#}")))?;

    let author = match repo::actor::get_by_ap_id(state.pool(), author_uri)
        .await
        .with_context(|| format!("lookup announced note author {author_uri}"))
        .map_err(DispatchError::Internal)?
    {
        Some(a) => a,
        None => crate::remote_actor::fetch_and_upsert(state, author_uri)
            .await
            .map_err(|e| DispatchError::Malformed(format!("fetch announced note author: {e}")))?,
    };

    let recipients = collect_recipients(&obj, &JsonValue::Null);
    let new = build_remote_note(state, &author, note_ap_id, &obj, &recipients).await?;
    if !matches!(new.visibility, Visibility::Public | Visibility::Unlisted) {
        return Err(DispatchError::Malformed(format!(
            "announced note is not public/unlisted (visibility={:?}); refusing to fetch-store",
            new.visibility,
        )));
    }
    let inserted = repo::note::insert(state.pool(), new)
        .await
        .with_context(|| format!("insert announced note {note_ap_id}"))
        .map_err(DispatchError::Internal)?;
    info!(
        note_id = inserted.id,
        note_ap_id,
        author = %author.ap_id,
        "announced note fetched and stored",
    );
    Ok(inserted)
}

/// `attributedTo` を string / `{id}` / 配列の先頭から URI として取り出す。
/// Mastodon は string、Misskey 等は object や配列で出すことがある。
fn extract_attributed_to(obj: &serde_json::Map<String, JsonValue>) -> Option<&str> {
    fn one(v: &JsonValue) -> Option<&str> {
        match v {
            JsonValue::String(s) => Some(s.as_str()),
            JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str),
            _ => None,
        }
    }
    match obj.get("attributedTo")? {
        JsonValue::Array(arr) => arr.iter().find_map(one),
        other => one(other),
    }
}

/// 引用先 note を解決し、それが我々 local actor の local note を指しているか
/// (= `is_local_quote`) を判定して返す。DB エラーは debug ログだけ残して
/// `None` 扱いで進む ── quote 通知は本筋の Create 受領を巻き込まない。
async fn resolve_quote_target(
    state: &AppState,
    obj: &serde_json::Map<String, JsonValue>,
) -> (Option<NoteRow>, bool) {
    let quote_target = match extract_quote_target(state, obj).await {
        Ok(q) => q,
        Err(err) => {
            debug!(
                ?err,
                "Create: quote target lookup failed; continuing without quote notification"
            );
            None
        }
    };
    let is_local_quote = match quote_target.as_ref() {
        Some(q) if q.is_local => local_actor_id_opt(state).await == Some(q.actor_id),
        _ => false,
    };
    (quote_target, is_local_quote)
}

/// `quoteUrl` / `quoteUri` / `_misskey_quote` のうち最初に見つかった URI で
/// `note` テーブルを検索する。これら全ては「もう一つの note を引用している」
/// AP 拡張で、Mastodon の最新 draft / FEP-e232 / FEP-044f / Misskey 慣習を
/// カバーする。
///
/// 永続化 (= note テーブルに `quote_target_note_id` 列追加) は別 issue 扱い。
/// 本関数は「引用先 note を引き当てる」だけ。
async fn extract_quote_target(
    state: &AppState,
    obj: &serde_json::Map<String, JsonValue>,
) -> Result<Option<NoteRow>, anyhow::Error> {
    let uri = obj
        .get("quoteUrl")
        .or_else(|| obj.get("quoteUri"))
        .or_else(|| obj.get("_misskey_quote"))
        .and_then(JsonValue::as_str);
    let Some(uri) = uri else {
        return Ok(None);
    };
    let row = repo::note::get_by_ap_id(state.pool(), uri)
        .await
        .with_context(|| format!("lookup quote target {uri}"))?;
    Ok(row)
}

/// `is_local_quote` 判定用に local actor の id を引く best-effort lookup。
/// 失敗は `None` → 全 note 一致しないので quote 通知が発火しない安全側挙動。
async fn local_actor_id_opt(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .ok()
        .flatten()
        .filter(|a| a.is_local)
        .map(|a| a.id)
}

/// `to` / `cc` の集合。activity 階層と object 階層の両方を保持して、
/// visibility 推定にも mention 判定にも使い回す。
struct Recipients {
    object_to: Vec<String>,
    object_cc: Vec<String>,
    activity_to: Vec<String>,
    activity_cc: Vec<String>,
}

impl Recipients {
    fn iter_all(&self) -> impl Iterator<Item = &String> {
        self.object_to
            .iter()
            .chain(self.object_cc.iter())
            .chain(self.activity_to.iter())
            .chain(self.activity_cc.iter())
    }
}

fn collect_recipients(
    obj: &serde_json::Map<String, JsonValue>,
    activity: &JsonValue,
) -> Recipients {
    Recipients {
        object_to: extract_string_array(obj.get("to")),
        object_cc: extract_string_array(obj.get("cc")),
        activity_to: extract_string_array(activity.get("to")),
        activity_cc: extract_string_array(activity.get("cc")),
    }
}

/// `Create.object` (= inline Note) を [`repo::note::NewNote`] に詰める。
/// 上限 / `inReplyTo` 解決 / visibility 推定をここでまとめて行う。
async fn build_remote_note(
    state: &AppState,
    signer: &ActorRow,
    note_ap_id: &str,
    obj: &serde_json::Map<String, JsonValue>,
    recipients: &Recipients,
) -> Result<repo::note::NewNote, DispatchError> {
    let content = obj
        .get("content")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    if content.chars().count() > CONTENT_MAX {
        return Err(DispatchError::Malformed(
            "Create.Note content exceeds the 5000-character limit".into(),
        ));
    }

    let summary = normalize_summary(obj.get("summary").and_then(JsonValue::as_str));
    if let Some(s) = summary.as_deref()
        && s.chars().count() > SUMMARY_MAX
    {
        return Err(DispatchError::Malformed(
            "Create.Note summary exceeds the 200-character limit".into(),
        ));
    }

    let language = obj
        .get("language")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let in_reply_to_ap_id = obj
        .get("inReplyTo")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let in_reply_to_note_id = if let Some(uri) = in_reply_to_ap_id.as_deref() {
        // DB 接続障害 (= Internal) と「親 note が未知」(= Ok(None)) を区別する。
        // `.ok()` で平滑化すると接続障害時に reply 無し扱いで insert が成立し、
        // インフラ問題が静かにすり抜ける ([[m11-pr-review]] minor 1)。
        repo::note::get_by_ap_id(state.pool(), uri)
            .await
            .with_context(|| format!("lookup reply parent {uri}"))
            .map_err(DispatchError::Internal)?
            .map(|n| n.id)
    } else {
        None
    };
    let visibility = derive_visibility(
        &recipients.object_to,
        &recipients.object_cc,
        &recipients.activity_to,
        &recipients.activity_cc,
    );
    let sensitive = obj
        .get("sensitive")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let url = obj.get("url").and_then(extract_url_string);
    let published_at = obj
        .get("published")
        .and_then(JsonValue::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc));
    let attachments = obj
        .get("attachment")
        .cloned()
        .unwrap_or_else(|| JsonValue::Array(vec![]));
    let tags = obj
        .get("tag")
        .cloned()
        .unwrap_or_else(|| JsonValue::Array(vec![]));

    Ok(repo::note::NewNote {
        ap_id: note_ap_id.to_string(),
        actor_id: signer.id,
        content,
        language,
        in_reply_to_ap_id,
        in_reply_to_note_id,
        summary,
        visibility,
        sensitive,
        to_recipients: recipients.object_to.clone(),
        cc_recipients: recipients.object_cc.clone(),
        attachments,
        tags,
        is_local: false,
        url,
        published_at,
    })
}

/// AP Note の `summary` (= CW / spoiler) を正規化する。空文字や空白のみは
/// `None` (= CW なし) に倒し、非空はそのまま返す (remote 著者の CW テキストは
/// 勝手に trim しない)。
///
/// Pleroma は CW 無しのノートでも `summary: ""` を送ってくるため、これを通さないと
/// `Some("")` が DB に入り、Misskey 系クライアント (Aria 等) が「`cw` が非 null =
/// CW あり」と解釈して全ノートが「警告文の無い CW」に見える。local 投稿側
/// (`local_api::notes`) の空 summary → `None` 正規化と対称。`Update` 受信も共有する。
pub(crate) fn normalize_summary(raw: Option<&str>) -> Option<String> {
    raw.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

/// `to` / `cc` などの string array 抽出。文字列以外の要素は無視。
fn extract_string_array(v: Option<&JsonValue>) -> Vec<String> {
    let Some(JsonValue::Array(arr)) = v else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(JsonValue::as_str)
        .map(str::to_string)
        .collect()
}

/// `url` フィールドは文字列単体、または `{type: "Link", href: "..."}` 形式、
/// または `[{...}, "string"]` の配列がある。最初に取れた URI を返す。
fn extract_url_string(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Object(map) => map
            .get("href")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        JsonValue::Array(arr) => arr.iter().find_map(extract_url_string),
        _ => None,
    }
}

/// AP の `to`/`cc` 配列から DB の `visibility` 列の値を推測する。
///
/// CLAUDE.md M4 PR2 仕様の逆方向:
/// - to に Public      → public
/// - cc に Public      → unlisted
/// - そのどちらでもないが to/cc に followers 系 URL がある → followers
/// - それ以外          → direct (= 我々宛のみの DM 等)
///
/// 厳密判定 (= 何が followers URL か signer 側で確定) は handler 段では
/// 難しいので、`public` / `unlisted` / `followers` / `direct` への ざっくり
/// 分類で十分。後段の表示で困らない粒度。
fn derive_visibility(
    object_to: &[String],
    object_cc: &[String],
    activity_to: &[String],
    activity_cc: &[String],
) -> Visibility {
    let any_to = object_to.iter().chain(activity_to.iter());
    let any_cc = object_cc.iter().chain(activity_cc.iter());

    if any_to.clone().any(|r| r == PUBLIC_URI) {
        Visibility::Public
    } else if any_cc.clone().any(|r| r == PUBLIC_URI) {
        Visibility::Unlisted
    } else if any_to.chain(any_cc).any(|r| r.ends_with("/followers")) {
        Visibility::Followers
    } else {
        Visibility::Direct
    }
}

/// `Note.id` のホストが `signer` のホストと一致することを確認する。
/// `reaction::process_inbound_reaction` の `ensure_same_host` と同種の防御。
fn same_host(other_uri: &str, signer_ap_id: &str, kind: &str) -> anyhow::Result<()> {
    let other = Url::parse(other_uri)
        .with_context(|| format!("{kind} {other_uri:?} is not a valid URL"))?;
    let signer = Url::parse(signer_ap_id)
        .with_context(|| format!("signer ap_id {signer_ap_id:?} is not a valid URL"))?;
    let other_host = other.host_str().unwrap_or_default();
    let signer_host = signer.host_str().unwrap_or_default();
    if !other_host.eq_ignore_ascii_case(signer_host) {
        return Err(anyhow!(
            "{kind} host {other_host:?} does not match signer host {signer_host:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visibility_public_to_strict() {
        let v = derive_visibility(&[PUBLIC_URI.into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Public);
    }

    #[test]
    fn visibility_unlisted_when_public_in_cc() {
        let v = derive_visibility(
            &["https://x.test/users/me".into()],
            &[PUBLIC_URI.into()],
            &[],
            &[],
        );
        assert_eq!(v, Visibility::Unlisted);
    }

    #[test]
    fn visibility_followers_when_only_followers_url() {
        let v = derive_visibility(&["https://x.test/users/a/followers".into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Followers);
    }

    #[test]
    fn visibility_direct_when_only_user_uri() {
        let v = derive_visibility(&["https://x.test/users/me".into()], &[], &[], &[]);
        assert_eq!(v, Visibility::Direct);
    }

    #[test]
    fn extract_string_array_filters_non_strings() {
        let v = json!(["a", 42, {"x": 1}, "b"]);
        assert_eq!(extract_string_array(Some(&v)), vec!["a", "b"]);
    }

    #[test]
    fn extract_string_array_handles_missing() {
        assert!(extract_string_array(None).is_empty());
        assert!(extract_string_array(Some(&JsonValue::Null)).is_empty());
    }

    #[test]
    fn normalize_summary_drops_empty_and_whitespace() {
        // Pleroma は CW 無しでも `summary: ""` を送る → None (= CW なし) に倒す。
        assert_eq!(normalize_summary(None), None);
        assert_eq!(normalize_summary(Some("")), None);
        assert_eq!(normalize_summary(Some("   ")), None);
        assert_eq!(normalize_summary(Some("\t\n")), None);
        // 非空はそのまま (remote の CW テキストは trim しない)。
        assert_eq!(normalize_summary(Some("spoiler")), Some("spoiler".into()));
        assert_eq!(
            normalize_summary(Some("  keep inner  ")),
            Some("  keep inner  ".into())
        );
    }

    #[test]
    fn extract_url_handles_string_object_array() {
        assert_eq!(
            extract_url_string(&json!("https://x")),
            Some("https://x".into())
        );
        assert_eq!(
            extract_url_string(&json!({"type": "Link", "href": "https://y"})),
            Some("https://y".into())
        );
        assert_eq!(
            extract_url_string(&json!([{"href": "https://z"}, "ignored"])),
            Some("https://z".into())
        );
    }

    #[test]
    fn same_host_accepts_matching() {
        same_host(
            "https://x.test/notes/1",
            "https://x.test/users/a",
            "Note id",
        )
        .unwrap();
    }

    #[test]
    fn same_host_rejects_different_host() {
        assert!(
            same_host(
                "https://evil.test/notes/1",
                "https://x.test/users/a",
                "Note id",
            )
            .is_err()
        );
    }

    #[test]
    fn extract_attributed_to_handles_string_object_array() {
        // string 形式 (Mastodon)
        let o = json!({"attributedTo": "https://x.test/users/a"});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), Some("https://x.test/users/a"));

        // object 形式 (`{id}`)
        let o = json!({"attributedTo": {"id": "https://x.test/users/b", "type": "Person"}});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), Some("https://x.test/users/b"));

        // 配列形式 → 最初に取れた URI
        let o = json!({"attributedTo": [{"type": "Mention"}, "https://x.test/users/c"]});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), Some("https://x.test/users/c"));
    }

    #[test]
    fn extract_attributed_to_missing_or_invalid_is_none() {
        let o = json!({"type": "Note"});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), None);

        // 数値 / null / id 無し object は無効
        let o = json!({"attributedTo": 42});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), None);

        let o = json!({"attributedTo": {"type": "Person"}});
        let JsonValue::Object(map) = o else {
            unreachable!()
        };
        assert_eq!(extract_attributed_to(&map), None);
    }
}
