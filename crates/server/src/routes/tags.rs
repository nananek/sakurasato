//! `GET /tags/{name}` — ハッシュタグ付きローカル投稿の最小 HTML 一覧ページ。
//!
//! `Note.tag` に載せた `Hashtag` エントリの `href` (= `{host}/tags/{name}`) が
//! リンク先として指すページ。お一人様サーバの「連合相手がタグリンクを踏んだら
//! 404 にならない」ための最小実装で、Mastodon / Misskey の `/tags/{tag}` と
//! 同じ URL 慣習に合わせている。API (Mastodon `/api/v1/timelines/tag/{tag}` 等)
//! はスコープ外。
//!
//! 可視性は permalink (`routes/permalink.rs`) と同じ境界 ── `public` /
//! `unlisted` のみ載せる。`followers` / `direct` は公開ページに並べない。

use std::fmt::Write as _;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;

use crate::state::AppState;

/// 1 ページに並べる最大件数。お一人様サーバ + ページャ無しの最小実装なので
/// 十分な大きさに切る。
const PAGE_LIMIT: i64 = 50;

pub async fn handle(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    // `#` 付き / 大文字 / 余白で来られても正規化して照合する。
    let tag = name.trim().trim_start_matches('#').to_lowercase();
    if tag.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty hashtag").into_response();
    }

    let rows = match repo::note::list_by_hashtag(state.pool(), &tag, PAGE_LIMIT).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(?err, %tag, "tags page: list_by_hashtag failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut buf = String::with_capacity(512 + rows.len() * 128);
    let _ = writeln!(buf, "<!doctype html>");
    let _ = writeln!(buf, r#"<html lang="ja">"#);
    let _ = writeln!(buf, "<head>");
    let _ = writeln!(buf, r#"<meta charset="utf-8">"#);
    let _ = writeln!(
        buf,
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#
    );
    let _ = writeln!(buf, "<title>#{}</title>", escape_text(&tag));
    let _ = writeln!(buf, "</head>");
    let _ = writeln!(buf, "<body>");
    let _ = writeln!(buf, "<h1>#{}</h1>", escape_text(&tag));
    if rows.is_empty() {
        let _ = writeln!(buf, "<p>No notes.</p>");
    }
    for note in &rows {
        let _ = writeln!(buf, "<article>");
        let _ = writeln!(
            buf,
            r#"<a href="/notes/{}">{}</a>"#,
            note.id,
            escape_text(&crate::miauth::text::html_to_plain_text(&note.content)),
        );
        let _ = writeln!(
            buf,
            r#"<time datetime="{}">{}</time>"#,
            escape_attr(&note.published_at.to_rfc3339()),
            escape_text(&note.published_at.to_rfc3339()),
        );
        let _ = writeln!(buf, "</article>");
    }
    let _ = writeln!(buf, "</body></html>");

    let mut response = (StatusCode::OK, Body::from(buf)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// テキストノード用の最小 HTML escape (permalink の `escape_text` と同じ集合)。
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

/// 属性値用 escape。テキスト escape と同じ集合で十分。
fn escape_attr(s: &str) -> String {
    escape_text(s)
}
