//! TUI のメインループ。
//!
//! - 端末初期化 (raw mode + alt screen + mouse capture)
//! - 初回 whoami / timeline 取得
//! - SSE 購読タスクの起動
//! - `tokio::select!` で input / sse / tick を回し、Action を [`App`] に適用
//! - 終了時に端末を完全に元に戻す ([`restore_terminal`] は panic 時にも呼ぶ)

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::TuiOptions;
use crate::app::{App, Focus, StatusKind};
use crate::client::{ApiError, CreateNoteRequest, LocalApi, StreamEvent};
use crate::compose::Visibility;
use crate::event::{Action, translate};
use crate::image_cache::ImageCache;
use crate::sse;
use crate::theme::Theme;
use crate::ui;

/// 端末描画と入力をひとまとめにした TUI バックエンド。
type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// 1 ループあたりの待ち上限。これより長く何も起きないと `tick` が走る
/// (= 一時 status メッセージの TTL 消去等)。
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// メイン関数。`main.rs` から呼ぶ唯一のエントリ。
pub async fn run(options: TuiOptions) -> anyhow::Result<()> {
    let api = LocalApi::new(options.socket.clone(), options.token.clone());

    // whoami は接続テストも兼ねる。失敗したらここで abort して main にエラーを
    // 返す ── 端末はまだ raw mode に入っていないので追加の cleanup 不要。
    let whoami = api
        .whoami()
        .await
        .with_context(|| format!("whoami via {}", api.socket().display()))?;
    info!(
        ap_id = %whoami.ap_id,
        user = %whoami.preferred_username,
        socket = %api.socket().display(),
        "TUI: connected to local API"
    );

    let socket_label = api.socket().display().to_string();

    // Picker は端末を実際に問い合わせる (escape sequence 送出 → 応答待ち)。
    // alt screen / raw mode に切り替える **前** に呼ぶのが穏当 ── 失敗しても
    // 画像表示は単に無効化するだけで TUI は続行する。Kitty 等で `?` を出すと
    // Foot や非対応端末では即座に Err になり、grace-degrade する。
    let picker = if options.images_enabled {
        match ratatui_image::picker::Picker::from_query_stdio() {
            Ok(p) => {
                info!(?p, "TUI: ratatui-image picker initialized");
                Some(p)
            }
            Err(err) => {
                warn!(
                    ?err,
                    "TUI: failed to query terminal graphics; images disabled"
                );
                None
            }
        }
    } else {
        info!("TUI: images disabled by --no-images");
        None
    };
    // M6: 画像取得は LocalApi 経由で server → media-proxy に委譲する。
    // ImageCache は API クライアントを clone して持つ (Arc 同等のコスト)。
    let images = ImageCache::new(picker, Some(api.clone()));
    let mut app = App::new(options.theme.clone(), whoami, socket_label, images);

    // 初回タイムライン取得。
    match api.timeline_home(None, options.page_size).await {
        Ok(resp) => {
            app.replace_timeline(resp.notes, resp.next_before_id);
        }
        Err(err) => {
            warn!(?err, "initial timeline fetch failed");
            app.set_status(
                format!("timeline fetch failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(8)),
            );
        }
    }

    let (sse_tx, mut sse_rx) = mpsc::channel::<StreamEvent>(64);
    let sse_api = api.clone();
    let sse_task = tokio::spawn(async move { sse::run(sse_api, sse_tx).await });

    let mut terminal = init_terminal()?;
    let mut event_stream = EventStream::new();
    let mut last_rects = ui::PanelRects::default();

    let result = main_loop(
        &mut terminal,
        &mut app,
        &api,
        options.page_size,
        &mut event_stream,
        &mut sse_rx,
        &mut last_rects,
    )
    .await;

    restore_terminal(&mut terminal)?;
    drop(sse_rx); // receiver drop → SSE task が次ループで終了。
    sse_task.abort();
    let _ = sse_task.await;
    result
}

async fn main_loop(
    terminal: &mut TuiTerminal,
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    events: &mut EventStream,
    sse_rx: &mut mpsc::Receiver<StreamEvent>,
    last_rects: &mut ui::PanelRects,
) -> anyhow::Result<()> {
    // ratatui に描画。最初の 1 frame。
    *last_rects = redraw(terminal, app)?;

    while !app.should_quit {
        // 投稿エディタが viewport から外れたら戻す前にスクロール調整しておく。
        let timeline_capacity = last_rects.timeline.height.saturating_sub(2).max(1) as usize;
        // 1 note ≒ 4 行と粗く見積もる。viewport_items は粗くてよい
        // (ensure_visible は二段スクロール禁止のための保険)。
        let approx_items = timeline_capacity.max(1) / 4;
        app.ensure_visible(approx_items.max(1));

        tokio::select! {
            biased;

            maybe_evt = events.next() => {
                match maybe_evt {
                    Some(Ok(evt)) => handle_event(evt, app, api, page_size, last_rects).await,
                    Some(Err(e)) => {
                        warn!(?e, "terminal event stream error");
                    }
                    None => {
                        info!("terminal event stream ended");
                        break;
                    }
                }
            }
            maybe_sse = sse_rx.recv() => {
                match maybe_sse {
                    Some(StreamEvent::NoteCreated(payload)) => {
                        app.ingest_note_created(*payload);
                    }
                    None => {
                        debug!("SSE channel closed");
                    }
                }
            }
            () = tokio::time::sleep(TICK_INTERVAL) => {
                app.tick();
            }
        }
        *last_rects = redraw(terminal, app)?;
    }
    Ok(())
}

fn redraw(terminal: &mut TuiTerminal, app: &App) -> anyhow::Result<ui::PanelRects> {
    let mut captured = ui::PanelRects::default();
    terminal.draw(|f| {
        captured = ui::draw(f, app);
    })?;
    Ok(captured)
}

async fn handle_event(
    event: Event,
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    rects: &ui::PanelRects,
) {
    let action = translate(event, app.focus);
    apply_action(action, app, api, page_size, rects).await;
}

#[allow(clippy::too_many_lines, reason = "single dispatcher for all actions")]
async fn apply_action(
    action: Action,
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    rects: &ui::PanelRects,
) {
    match action {
        Action::Noop => {}
        Action::Quit => app.should_quit = true,
        Action::SelectNext => app.select_next(),
        Action::SelectPrev => app.select_prev(),
        Action::PageDown => {
            let h = rects.timeline.height.saturating_sub(2).max(1) as usize / 4;
            app.page_down(h.max(1));
        }
        Action::PageUp => {
            let h = rects.timeline.height.saturating_sub(2).max(1) as usize / 4;
            app.page_up(h.max(1));
        }
        Action::EnterCompose => {
            app.focus = Focus::Compose;
        }
        Action::FocusTimeline => {
            app.focus = Focus::Timeline;
        }
        Action::ToggleHelp => {
            app.focus = if app.focus == Focus::Help {
                Focus::Timeline
            } else {
                Focus::Help
            };
        }
        Action::RefreshTimeline => match api.timeline_home(None, page_size).await {
            Ok(resp) => {
                let n = resp.notes.len();
                app.replace_timeline(resp.notes, resp.next_before_id);
                app.set_status(
                    format!("refreshed: {n} notes"),
                    StatusKind::Success,
                    Some(Duration::from_secs(3)),
                );
            }
            Err(err) => {
                error!(?err, "refresh failed");
                app.set_status(
                    format!("refresh failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                );
            }
        },
        Action::LoadMore => {
            if app.timeline_exhausted {
                app.set_status(
                    "no more notes",
                    StatusKind::Info,
                    Some(Duration::from_secs(2)),
                );
                return;
            }
            let before = app.next_before_id;
            match api.timeline_home(before, page_size).await {
                Ok(resp) => {
                    let added = resp.notes.len();
                    app.append_older(resp.notes, resp.next_before_id);
                    app.set_status(
                        format!("loaded {added} older"),
                        StatusKind::Info,
                        Some(Duration::from_secs(2)),
                    );
                }
                Err(err) => {
                    app.set_status(
                        format!("load more failed: {err}"),
                        StatusKind::Error,
                        Some(Duration::from_secs(5)),
                    );
                }
            }
        }
        Action::CycleTheme => {
            let next = next_theme(&app.theme);
            match Theme::builtin(next) {
                Ok(t) => {
                    let label = t.name.clone();
                    app.theme = t;
                    app.set_status(
                        format!("theme: {label}"),
                        StatusKind::Info,
                        Some(Duration::from_secs(2)),
                    );
                }
                Err(err) => warn!(?err, "theme cycle failed"),
            }
        }
        Action::InsertChar(c) => app.compose.insert_char(c),
        Action::InsertNewline => app.compose.insert_newline(),
        Action::Backspace => app.compose.backspace(),
        Action::DeleteForward => app.compose.delete_forward(),
        Action::MoveLeft => app.compose.move_left(),
        Action::MoveRight => app.compose.move_right(),
        Action::MoveLineStart => app.compose.move_line_start(),
        Action::MoveLineEnd => app.compose.move_line_end(),
        Action::ToggleCw => app.compose.toggle_cw_focus(),
        Action::ToggleSensitive => app.compose.toggle_sensitive(),
        Action::CycleVisibility => app.compose.cycle_visibility(),
        Action::SubmitNote => {
            submit_note(app, api).await;
        }
        Action::Scroll(delta) => {
            if delta > 0 {
                for _ in 0..delta {
                    app.select_next();
                }
            } else {
                for _ in 0..(-delta) {
                    app.select_prev();
                }
            }
        }
        Action::MouseClick(col, row) => {
            handle_click(app, rects, col, row);
        }
    }
}

fn handle_click(app: &mut App, rects: &ui::PanelRects, col: u16, row: u16) {
    if let Some(help_rect) = rects.help
        && rect_contains(help_rect, col, row)
    {
        // Help を開いている間はクリックで閉じる。
        app.focus = Focus::Timeline;
        return;
    }
    if rect_contains(rects.compose, col, row) {
        app.focus = Focus::Compose;
        return;
    }
    if rect_contains(rects.timeline, col, row) {
        app.focus = Focus::Timeline;
        if let Some(idx) = rects.timeline_rows.resolve(row) {
            app.select_index(idx);
        }
    }
}

fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

async fn submit_note(app: &mut App, api: &LocalApi) {
    if !app.compose.is_submittable() {
        app.set_status(
            "compose: nothing to send",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    }
    if app.compose.remaining() < 0 {
        app.set_status(
            "compose: over limit",
            StatusKind::Error,
            Some(Duration::from_secs(3)),
        );
        return;
    }
    let req = CreateNoteRequest {
        content: app.compose.buffer().to_string(),
        summary: if app.compose.cw().is_empty() {
            None
        } else {
            Some(app.compose.cw().to_string())
        },
        visibility: Some(app.compose.visibility().as_wire().to_string()),
        sensitive: Some(app.compose.sensitive()),
        language: None,
        in_reply_to_ap_id: None,
    };
    match api.create_note(&req).await {
        Ok(resp) => {
            app.set_status(
                format!("posted #{} ({} delivered)", resp.id, resp.queued_deliveries),
                StatusKind::Success,
                Some(Duration::from_secs(4)),
            );
            app.compose.clear();
            // visibility は記憶しておきたいので clear 後に上書き。
            for _ in 0..visibility_steps(Visibility::Public, app.compose.visibility()) {
                app.compose.cycle_visibility();
            }
            app.focus = Focus::Timeline;
            // SSE で push されない場合の保険として、即時タイムライン再取得は
            // しない (= POST notes 内の SSE publish が同 socket で先に届く)。
        }
        Err(ApiError::Status { status, body }) => {
            app.set_status(
                format!("post failed: HTTP {status}: {body}"),
                StatusKind::Error,
                Some(Duration::from_secs(8)),
            );
        }
        Err(err) => {
            app.set_status(
                format!("post failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

fn visibility_steps(from: Visibility, to: Visibility) -> usize {
    let order = [
        Visibility::Public,
        Visibility::Unlisted,
        Visibility::Followers,
    ];
    let i = order.iter().position(|v| *v == from).unwrap_or(0);
    let j = order.iter().position(|v| *v == to).unwrap_or(0);
    (j + order.len() - i) % order.len()
}

/// 現在テーマ名 → 次に切り替えるテーマ名 (組み込み 3 種をぐるぐる)。
///
/// マッチは `Theme::name` の末尾に組み込み slug が並ぶ前提 (`Sakurasato Sakura`
/// なら `sakura`)。`sakurasato` 自体は全 builtin に含まれるので最後の単語だけを
/// 見る。
fn next_theme(current: &Theme) -> &'static str {
    let lower = current.name.to_ascii_lowercase();
    let last_word = lower.split_whitespace().next_back().unwrap_or("");
    let names = Theme::builtin_names();
    let cur = names.iter().position(|n| *n == last_word).unwrap_or(0);
    names[(cur + 1) % names.len()]
}

fn init_terminal() -> anyhow::Result<TuiTerminal> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen + mouse capture")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create ratatui Terminal")
}

fn restore_terminal(terminal: &mut TuiTerminal) -> anyhow::Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    )
    .context("leave alternate screen + mouse capture")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn next_theme_cycles() {
        // builtin_names = sakura -> dark -> light -> sakura
        let sakura = Theme::builtin("sakura").unwrap();
        assert_eq!(next_theme(&sakura), "dark");
        let dark = Theme::builtin("dark").unwrap();
        assert_eq!(next_theme(&dark), "light");
        let light = Theme::builtin("light").unwrap();
        assert_eq!(next_theme(&light), "sakura");
    }

    #[test]
    fn visibility_steps_wraps() {
        assert_eq!(visibility_steps(Visibility::Public, Visibility::Public), 0);
        assert_eq!(
            visibility_steps(Visibility::Public, Visibility::Unlisted),
            1
        );
        assert_eq!(
            visibility_steps(Visibility::Public, Visibility::Followers),
            2
        );
        assert_eq!(
            visibility_steps(Visibility::Followers, Visibility::Public),
            1
        );
    }
}
