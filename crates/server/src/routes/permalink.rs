//! `GET /notes/{id}` — ローカル Note のパーマリンク (HTML)。
//!
//! AP `id` URI (= `https://<host>/notes/{id}`) で参照可能な人間可読 HTML を
//! 返す。`{id}` は `note.id` (i64) をそのまま使う ── Mastodon が `/@user/{id}`
//! に sequential id を載せる慣習に倣う。完全に opaque なスラグが必要に
//! なったら `note` に `slug` カラムを足して切り替える (M? 以降)。
//!
//! ## セキュリティ
//!
//! M4 PR1 時点で **inbound Note の content sanitization は未実装** のため、
//! 安全側に倒して content / summary / display name 全てを HTML escape する。
//! 結果として `<p>hello</p>` 形式の本文が `&lt;p&gt;hello&lt;/p&gt;` と
//! 見える ── 仕様未満だが、XSS よりはマシ。サニタイザを入れた段階で
//! content だけは raw 出力に戻す予定 (将来 milestone)。
//!
//! ## Content negotiation
//!
//! `Accept: application/activity+json` (および `application/ld+json`) を
//! 受けたら AP Note JSON を返し、それ以外 (typical browser) には HTML を
//! 返す。AP fetcher が `note.ap_id` を解決したときに 404 を返すと連合相手
//! が `Create` の object を引けず、再送ループが止まらない原因になるため、
//! PR2 で AP JSON 兼用化を入れる。

use std::fmt::Write as _;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::{ActorRow, NoteRow};
use sakurasato_core::repo;
use serde_json::{Value as JsonValue, json};

use crate::state::AppState;

pub async fn handle(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let note = match repo::note::get_by_id(state.pool(), id).await {
        Ok(Some(row)) if row.is_local => row,
        // 未知 / remote note は 404。remote のパーマリンクは別ドメインに
        // あるので、本サーバが代理で出すべきではない。
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, note_id = id, "permalink note lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // **SECURITY (緊急 fix)**: visibility filter ── permalink は
    // unauthenticated な公開 endpoint なので、AS2 audience に Public が
    // 含まれない note (`followers` / `direct`) は **URL を知っていても
    // 漏らさない**。404 で返して存在自体を秘匿する (Mastodon の慣習と同じ)。
    //
    // - `public`: to に `as:Public` → 公開
    // - `unlisted`: cc に `as:Public` → 公開 (= 連合相手の fetch にも応答する慣習)
    // - `followers`: to に followers-only → permalink では 404
    // - `direct`: to に mentioned actor のみ → permalink では 404
    //
    // Sakurasato はお一人様サーバなので「ログイン済み follower かを判定する
    // permalink 認証」は提供しない (= TUI 経由で読む)。`note.visibility` は
    // `crates/core/src/model.rs::Visibility::as_str` と同形の小文字 enum 文字列。
    if !matches!(note.visibility.as_str(), "public" | "unlisted") {
        tracing::debug!(
            note_id = id,
            visibility = note.visibility.as_str(),
            "permalink: refusing to serve non-public note",
        );
        return StatusCode::NOT_FOUND.into_response();
    }

    let actor = match repo::actor::get_by_id(state.pool(), note.actor_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::error!(
                note_id = id,
                actor_id = note.actor_id,
                "permalink: note refers to non-existent actor"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(err) => {
            tracing::error!(?err, "permalink actor lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if wants_activity_json(&headers) {
        let body = render_ap_note(&note, &actor);
        let mut response = (StatusCode::OK, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/activity+json"),
        );
        return response;
    }

    let html = render_html(
        &note,
        &actor.preferred_username,
        actor.display_name.as_deref(),
    );
    let mut response = (StatusCode::OK, html).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// `Accept` ヘッダが AP JSON 系を要求しているか判定する。
///
/// AP fetcher (Mastodon / Misskey 等) は `application/activity+json` または
/// `application/ld+json; profile="https://www.w3.org/ns/activitystreams"` を
/// 送ってくる。ブラウザは `text/html` (+ `*/*`) なので AP 判定しない。
///
/// 全部の Accept ヘッダ要素を見て、AP 系の media-type が含まれていれば
/// JSON を返す。`*/*` 単独や `text/html` 含みは HTML 扱い。
fn wants_activity_json(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    for part in accept.split(',') {
        let media = part
            .split(';')
            .next()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if media == "application/activity+json" || media == "application/ld+json" {
            return true;
        }
    }
    false
}

/// `Note` を AP JSON-LD として組み立てる。
///
/// `to` / `cc` は DB に永続化済みの `to_recipients` / `cc_recipients` を
/// そのまま使う ── POST `/api/v1/notes` で組み立てた値と完全に同じ。
/// 受信した remote note の場合も DB の to/cc がそのまま流れる。
fn render_ap_note(note: &NoteRow, actor: &ActorRow) -> JsonValue {
    let published = note
        .published_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut body = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Note",
        "id": note.ap_id,
        "attributedTo": actor.ap_id,
        "content": note.content,
        "to": note.to_recipients.0,
        "cc": note.cc_recipients.0,
        "published": published,
        "sensitive": note.sensitive,
        "url": note.url.clone().unwrap_or_else(|| note.ap_id.clone()),
    });
    if let Some(s) = note.summary.as_deref()
        && !s.is_empty()
    {
        body["summary"] = JsonValue::String(s.into());
    }
    if let Some(lang) = note.language.as_deref()
        && !lang.is_empty()
    {
        body["contentMap"] = json!({ lang: note.content });
    }
    if let Some(reply) = note.in_reply_to_ap_id.as_deref() {
        body["inReplyTo"] = JsonValue::String(reply.into());
    }
    body
}

fn render_html(note: &NoteRow, username: &str, display_name: Option<&str>) -> Body {
    let display = display_name.unwrap_or(username);
    let title = format!("Note by @{username}");
    let mut buf = String::with_capacity(512 + note.content.len());
    let _ = writeln!(buf, "<!doctype html>");
    let _ = writeln!(
        buf,
        r#"<html lang="{}">"#,
        escape_attr(note.language.as_deref().unwrap_or(""))
    );
    let _ = writeln!(buf, "<head>");
    let _ = writeln!(buf, r#"<meta charset="utf-8">"#);
    let _ = writeln!(
        buf,
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#
    );
    let _ = writeln!(buf, "<title>{}</title>", escape_text(&title));
    let _ = writeln!(buf, "</head>");
    let _ = writeln!(buf, "<body>");
    let _ = writeln!(buf, "<article>");
    let _ = writeln!(buf, "<header>");
    let _ = writeln!(
        buf,
        r#"<a href="/users/{}">@{}</a>"#,
        escape_attr(username),
        escape_text(display)
    );
    let _ = writeln!(
        buf,
        r#"<time datetime="{}">{}</time>"#,
        escape_attr(&note.published_at.to_rfc3339()),
        escape_text(&note.published_at.to_rfc3339()),
    );
    let _ = writeln!(buf, "</header>");
    if let Some(summary) = note.summary.as_deref()
        && !summary.is_empty()
    {
        let _ = writeln!(buf, "<details><summary>{}</summary>", escape_text(summary));
    }
    let _ = writeln!(
        buf,
        r#"<div class="content">{}</div>"#,
        escape_text(&note.content),
    );
    if note.summary.as_deref().is_some_and(|s| !s.is_empty()) {
        let _ = writeln!(buf, "</details>");
    }
    let _ = writeln!(buf, "</article>");
    let _ = writeln!(buf, "</body></html>");
    Body::from(buf)
}

/// テキストノードに入れるための最小 HTML escape。`&`/`<`/`>`/`"`/`'` を
/// 実体参照に変換する。属性値とテキスト両方を 1 関数でカバーする最小集合
/// (OWASP の Output Encoding 規約に従う)。
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// 属性値用 escape。テキスト escape と同じ集合で十分 (`"` を実体化する
/// ため属性区切りが壊れない)。別関数にしているのは将来 URL escape を別に
/// するときに差し替えやすくするため。
fn escape_attr(s: &str) -> String {
    escape_text(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn wants_activity_json_true_for_ap_accept() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/activity+json"),
        );
        assert!(wants_activity_json(&h));

        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static(
                r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
            ),
        );
        assert!(wants_activity_json(&h));

        // Mastodon が複数 media-type を並べた場合も拾えること。
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html, application/activity+json;q=0.9"),
        );
        assert!(wants_activity_json(&h));
    }

    #[test]
    fn wants_activity_json_false_for_browser() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
        );
        assert!(!wants_activity_json(&h));

        // Accept ヘッダ無しはブラウザの素っ気ない GET 相当 → HTML 扱い。
        let empty = HeaderMap::new();
        assert!(!wants_activity_json(&empty));

        // `*/*` だけは HTML 側に振る (連合 fetcher は明示 AP 系を載せる慣習)。
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!wants_activity_json(&h));
    }

    #[test]
    fn escape_text_handles_html_specials() {
        assert_eq!(
            escape_text("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&#x27;f"
        );
    }

    #[test]
    fn escape_text_passes_through_plain() {
        assert_eq!(escape_text("hello world"), "hello world");
        assert_eq!(escape_text("日本語"), "日本語");
    }
}
