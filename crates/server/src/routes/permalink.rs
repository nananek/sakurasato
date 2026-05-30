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
//! `Accept: application/activity+json` (AP fetcher が要求するヘッダ) は
//! **PR1 では扱わず HTML を返す**。AP Note JSON は M4 PR2 (POST notes +
//! Create 配送) と合わせて入れる ── ローカル発信 Note を生成する経路が
//! 無い PR1 で AP JSON だけ提供しても整合性が取れないため。

use std::fmt::Write as _;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::NoteRow;
use sakurasato_core::repo;

use crate::state::AppState;

pub async fn handle(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
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
