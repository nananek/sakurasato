//! `GET /miauth/{uuid}` ── Misskey `MiAuth` 仕様準拠の **browser landing**
//! (= M14 #158, 親 issue #150)。
//!
//! ## `MiAuth` 仕様の概略
//!
//! Misskey クライアント (Milktea / `MissRirica` 等) は次のフローで認証する:
//!
//! 1. クライアントが UUID v4 を生成
//! 2. クライアントがユーザのブラウザを `GET /miauth/{uuid}?name=<app>&permission=<csv>&callback=<url>`
//!    に開く ── サーバはここで pending session を登録し、認可 UI を表示
//! 3. ユーザがブラウザで承認
//! 4. クライアントが `POST /api/miauth/{uuid}/check` を polling し、承認後の
//!    レスポンス `{token, user}` を取得
//!
//! ## Sakurasato の差分
//!
//! CLAUDE.md §1 「Web 認証 UI は作らない」原則を保つため、本 endpoint は
//! Web 上の承認ボタンを **生やさない** ── 代わりに以下のテキスト指示だけを
//! `text/html` で返す:
//!
//! ```text
//! sakurasato-server miauth approve <uuid> --permission ...
//! ```
//!
//! ユーザは別端末/別 tmux でサーバ CLI を叩いて承認する。これにより:
//!
//! - 既存 Misskey クライアントの **browser open 経路はそのまま動く** (= AC「実機
//!   ログイン」を成立させる)
//! - Web の認証 UI / セッション cookie / CSRF は一切持たない (= 攻撃面ゼロ)
//! - 認可は **サーバ管理者 (= お一人様)** のみが CLI 経由で行う = アクセス制御
//!   は OS のユーザ権限境界と一致
//!
//! ## query パラメータ
//!
//! | name        | 型     | 必須 | 用途 |
//! |-------------|--------|------|------|
//! | `name`      | string | no   | アプリ表示名 (例: `Milktea-iPhone`) |
//! | `permission`| string | no   | CSV (例: `read:account,write:reactions`) |
//! | `callback`  | string | no   | 認可完了後の redirect 先 (informational のみ) |
//!
//! Misskey 公式 spec では全部 optional。`permission` 欠落時は **scope ゼロ** で
//! session を登録し、CLI 側で `--permission` 指定して上書き snapshot するのが
//! 標準フロー。
//!
//! ## 状態遷移
//!
//! 同 UUID の `GET /miauth/{uuid}` が **2 回目以降** に来たケース ──
//! Misskey 公式仕様はクライアント生成 UUID を一度きりに使う想定だが、本実装は
//! 防御的に「既存 session が pending / approved / consumed / rejected いずれ
//! でも、新しい行は作らず既存行の現状をそのまま表示」する (= UUID は client
//! 生成なので INSERT に失敗してもサーバ側はエラーを返さず、ユーザ視点での
//! 操作性を維持)。

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Duration;
use sakurasato_core::model::MiAuthSessionState;
use sakurasato_core::repo;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

/// `GET /miauth/{uuid}` の query 文字列。`name` / `permission` / `callback`
/// の 3 つは Misskey 仕様準拠で全部 optional。
///
/// `permission` は **CSV** で渡される (= Misskey クライアントは `,` 区切り
/// で連結する慣行)。Sakurasato は CSV 分解 + trim + dedup を本 handler 内で
/// 行い JSONB array として保管する。
#[derive(Debug, Deserialize)]
pub struct LandingQuery {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub callback: Option<String>,
}

/// CSV (`read:account,write:reactions`) を `Vec<String>` に分解する。空白は
/// trim、空要素 / 重複は除去、結果が空なら `Vec::new()`。
///
/// CLI 側 `miauth_cli::normalize_permissions` と同等のロジックだが、こちらは
/// **入力が空でも error にしない** (= Misskey spec で `permission` は optional
/// なので、空文字列を「scope ゼロ session」として受け入れる)。
fn parse_permissions_csv(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in raw.split(',') {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// `text/html` 形式で CLI 指示を返す landing。HTML エスケープは本 handler が
/// 行う ── `name` / `callback` / permission CSV はクライアント由来でユーザ
/// ブラウザに描画されるため、`<script>` 等を入れられた場合の XSS を防ぐ。
fn render_landing(
    uuid: Uuid,
    app_name: &str,
    callback_url: Option<&str>,
    permissions: &[String],
) -> String {
    // `html_escape` クレートを足さずに最小限の手書き escape ── `&`/`<`/`>`/`"`
    // /`'` の 5 文字だけで、HTML5 のテキストコンテキスト + 属性両方を安全に
    // できる (`html-escape` の `encode_quoted_attribute` と同等)。本 endpoint
    // が描画するのは「ユーザにコマンドを案内する固定 page」だけで、`<a href>`
    // 等の動的属性は持たないので本程度の escape で十分。
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(c),
            }
        }
        out
    }

    let cb_line = match callback_url {
        Some(c) if !c.is_empty() => format!("<p>Callback URL: <code>{cb}</code></p>", cb = esc(c)),
        _ => String::new(),
    };
    let perm_csv = permissions.join(",");
    let perm_line_for_human = if permissions.is_empty() {
        "<em>(none requested)</em>".to_string()
    } else {
        esc(&permissions.join(", "))
    };

    // CLI コマンド文字列。`--permission` の引数は CSV (= `clap` 側
    // `value_delimiter = ','` で分解される)。
    let cli_cmd = if permissions.is_empty() {
        format!("sakurasato-server miauth approve {uuid} --permission &lt;scope&gt;")
    } else {
        let perm_csv_esc = esc(&perm_csv);
        format!("sakurasato-server miauth approve {uuid} --permission {perm_csv_esc}")
    };
    let app_name_esc = esc(app_name);

    format!(
        r#"<!doctype html>
<html lang="ja">
<head>
<meta charset="utf-8">
<title>Sakurasato MiAuth — {app_name_esc}</title>
<style>
body {{ font-family: ui-monospace, monospace; max-width: 720px; margin: 4em auto; padding: 0 1em; line-height: 1.5; color: #222; background: #fafafa; }}
code, pre {{ background: #eee; padding: 0.2em 0.4em; border-radius: 3px; }}
pre {{ padding: 1em; overflow-x: auto; }}
h1 {{ font-size: 1.4em; }}
p {{ margin: 0.6em 0; }}
.warn {{ color: #b00; }}
</style>
</head>
<body>
<h1>Sakurasato MiAuth — {app_name_esc}</h1>
<p>An app is requesting authorization. <strong>This server has no web approval UI by design</strong> (single-user instance, CLAUDE.md §1).</p>
<p>To grant access, run the following on the server host:</p>
<pre><code>{cli_cmd}</code></pre>
<p>Requested permission scope: {perm_line_for_human}</p>
{cb_line}
<p>Session UUID: <code>{uuid}</code></p>
<p class="warn">If you did not initiate this request, do not approve. The session expires automatically.</p>
</body>
</html>
"#
    )
}

/// `GET /miauth/{uuid}` handler。
///
/// - UUID が parse できない: 400 (= クライアントバグ、URL がそもそも `MiAuth` 仕様
///   違反)
/// - 既存 session が pending: そのまま landing 描画 (idempotent)
/// - 既存 session が approved / consumed / rejected / expired: 現状を反映した
///   landing を描画 (= ユーザに「既に承認済」「拒否済」「期限切れ」「token 発行済」
///   を伝える)
/// - 既存 session が無い: pending 行を INSERT して landing 描画
///
/// **`MiAuth` config 未設定環境では本 route は mount されない** (= [`crate::miauth::router`]
/// が config の有無で判定するため、`miauth: None` の deploy では `404` が
/// 自然に返る)。本 PR で `session_ttl` は **`AppState` から引かず** 直接 `Config`
/// 参照する ── handler が呼ばれる時点で `miauth: Some(_)` は config 側で
/// 確定しているため。
pub async fn handle(
    State(state): State<AppState>,
    Path(uuid_str): Path<String>,
    Query(query): Query<LandingQuery>,
) -> Response {
    let Ok(uuid) = Uuid::parse_str(&uuid_str) else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "invalid UUID",
        )
            .into_response();
    };

    let app_name = query.name.as_deref().unwrap_or("unknown app").to_string();
    let permissions = query
        .permission
        .as_deref()
        .map(parse_permissions_csv)
        .unwrap_or_default();
    let callback_url = query
        .callback
        .as_deref()
        .map(str::to_owned)
        .filter(|s| !s.is_empty());

    // 既存 session の探索 ── pending / approved / consumed / rejected / expired
    // いずれの場合も同 UUID を **再 INSERT しない** (= UUID は client 生成、
    // 同じ UUID を再 GET したとき防御的に no-op で済ます)。
    match repo::miauth::get_session(state.pool(), uuid).await {
        Ok(Some(row)) => {
            // 既存 session: その時点の permissions snapshot をそのまま表示
            // (= browser landing が描画する CLI コマンドは「初回 GET 時に
            // 来た permission」を反映する。再 GET で permission を上書きし
            // ない設計 ── client が url を組み直すケースを想定していない)。
            let body = match row.state_enum() {
                Some(MiAuthSessionState::Pending) | None => render_landing(
                    uuid,
                    &row.app_name,
                    row.callback_url.as_deref(),
                    &row.permissions.0,
                ),
                Some(state_kind) => render_existing_state(uuid, state_kind),
            };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                body,
            )
                .into_response();
        }
        Ok(None) => { /* fall through to INSERT */ }
        Err(err) => {
            tracing::error!(?err, %uuid, "miauth_session lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let ttl_secs = state
        .config()
        .miauth
        .as_ref()
        .map_or(600, |m| m.session_ttl_secs);
    // u64 → i64 のキャスト。`session_ttl_secs` は config で取り得る値が秒 ──
    // 現実的に i64::MAX 秒 (= 約 2900 億年) を超えることはない。`Duration::seconds`
    // は i64 を取るため、`try_from` で範囲確認した上で fallback として 10 分
    // (= deploy 既定) に倒す。
    let ttl_i64 = i64::try_from(ttl_secs).unwrap_or(600);
    let expires_at = chrono::Utc::now() + Duration::seconds(ttl_i64);

    let new = repo::miauth::NewMiAuthSession {
        uuid,
        app_name: app_name.clone(),
        callback_url: callback_url.clone(),
        permissions: permissions.clone(),
        expires_at,
    };
    if let Err(err) = repo::miauth::insert_session(state.pool(), new).await {
        // INSERT race: 同 UUID で並行 GET が走ったケース。既存 row を取得
        // して landing を描画し直す (= ユーザ体験を壊さない)。`collapsible_if`
        // を避けるため `if let` を 1 段に集約する。
        if matches!(err, sqlx::Error::Database(ref dbe) if dbe.is_unique_violation())
            && let Ok(Some(row)) = repo::miauth::get_session(state.pool(), uuid).await
        {
            let body = render_landing(
                uuid,
                &row.app_name,
                row.callback_url.as_deref(),
                &row.permissions.0,
            );
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                body,
            )
                .into_response();
        }
        tracing::error!(?err, %uuid, "miauth_session insert failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let body = render_landing(uuid, &app_name, callback_url.as_deref(), &permissions);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// 既存 session が pending **以外** だったときの informational page。
/// CLI 側で何が起きたかをユーザに告げるだけで、追加の操作は受け付けない。
fn render_existing_state(uuid: Uuid, state: MiAuthSessionState) -> String {
    let (heading, detail) = match state {
        MiAuthSessionState::Approved => (
            "Already approved",
            "This session is approved. The client should now poll <code>POST /api/miauth/{uuid}/check</code> to receive its token.",
        ),
        MiAuthSessionState::Consumed => (
            "Token already issued",
            "This session has already produced an access token for the client.",
        ),
        MiAuthSessionState::Rejected => (
            "Session rejected",
            "This session was rejected on the server CLI. The client must generate a new UUID and retry.",
        ),
        MiAuthSessionState::Expired => (
            "Session expired",
            "This session expired before approval. The client must generate a new UUID and retry.",
        ),
        // Pending should not reach here (caller branches on it first).
        MiAuthSessionState::Pending => (
            "Pending",
            "Session is awaiting CLI approval.",
        ),
    };
    format!(
        r#"<!doctype html>
<html lang="ja">
<head>
<meta charset="utf-8">
<title>Sakurasato MiAuth — {heading}</title>
<style>
body {{ font-family: ui-monospace, monospace; max-width: 720px; margin: 4em auto; padding: 0 1em; line-height: 1.5; color: #222; background: #fafafa; }}
code {{ background: #eee; padding: 0.2em 0.4em; border-radius: 3px; }}
</style>
</head>
<body>
<h1>Sakurasato MiAuth — {heading}</h1>
<p>{detail}</p>
<p>Session UUID: <code>{uuid}</code></p>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permissions_csv_basic() {
        let v = parse_permissions_csv("read:account,write:reactions");
        assert_eq!(v, vec!["read:account", "write:reactions"]);
    }

    /// 空白 trim + 重複 dedup (= CLI normalize と同じ流儀)。
    #[test]
    fn parse_permissions_csv_trims_and_dedups() {
        let v = parse_permissions_csv("  read:account , write:reactions, read:account , ,write:reactions");
        assert_eq!(v, vec!["read:account", "write:reactions"]);
    }

    /// 空文字列 → 空 Vec (= CLI と違ってエラーにしない、Misskey spec で
    /// optional)。
    #[test]
    fn parse_permissions_csv_empty_returns_empty() {
        assert!(parse_permissions_csv("").is_empty());
        assert!(parse_permissions_csv(" , ,, ").is_empty());
    }

    /// landing HTML が CLI 文字列を含んでいて、`<script>` 等の埋め込みを
    /// escape している。
    #[test]
    fn render_landing_includes_cli_and_escapes_html() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let body = render_landing(
            uuid,
            "Milktea<script>alert(1)</script>",
            Some("https://app.test/cb?x=1&y=2"),
            &["read:account".to_string(), "write:reactions".to_string()],
        );
        assert!(body.contains("sakurasato-server miauth approve 550e8400"));
        assert!(body.contains("--permission read:account,write:reactions"));
        // script タグが escape されていて raw 出力されない (= XSS 防御)。
        assert!(!body.contains("<script>alert(1)</script>"));
        assert!(body.contains("Milktea&lt;script&gt;"));
        // callback URL も `&` が escape される。
        assert!(body.contains("https://app.test/cb?x=1&amp;y=2"));
        // 'する' 等の日本語が UTF-8 で混在しても落ちない (バランスチェック)。
        assert!(body.contains("Session UUID"));
    }

    /// permission 未指定なら CLI 文字列は `<scope>` プレースホルダになる。
    #[test]
    fn render_landing_without_permissions_shows_placeholder() {
        let uuid = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let body = render_landing(uuid, "TestApp", None, &[]);
        assert!(body.contains("--permission &lt;scope&gt;"));
        assert!(body.contains("(none requested)"));
        // callback 未指定なら該当行を出さない。
        assert!(!body.contains("Callback URL"));
    }

    /// 既存 state ページが state ごとに正しい heading を出す。
    #[test]
    fn render_existing_state_messages() {
        let uuid = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        assert!(render_existing_state(uuid, MiAuthSessionState::Approved).contains("Already approved"));
        assert!(render_existing_state(uuid, MiAuthSessionState::Consumed).contains("already produced"));
        assert!(render_existing_state(uuid, MiAuthSessionState::Rejected).contains("rejected"));
        assert!(render_existing_state(uuid, MiAuthSessionState::Expired).contains("expired"));
    }
}
