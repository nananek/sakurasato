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
use crate::client::{
    ApiError, CreateNoteRequest, FollowTarget, LocalApi, MediaResponse, ProfileUpdate, StreamEvent,
};
use crate::compose::AttachmentRef;
use crate::event::{Action, translate};
use crate::image_cache::ImageCache;
use crate::in_flight::InFlightGuard;
use crate::picker::{Activation, FilePicker, PickerMode};
use crate::profile::ProfileScreen;
use crate::sse;
use crate::theme::Theme;
use crate::ui;

/// 端末描画と入力をひとまとめにした TUI バックエンド。
type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// 1 ループあたりの待ち上限。これより長く何も起きないと `tick` が走る
/// (= 一時 status メッセージの TTL 消去等)。
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// M7: アップロードタスクがメインループに返す結果。
///
/// `mpsc::Sender` で `runtime::main_loop` の `tokio::select!` に流す。
/// SSE と同じく非同期で来るので、`apply_action` が直接 `await` で待つのでは
/// なく、`tokio::spawn` して完了を別経路で受ける。
#[derive(Debug)]
enum UploadOutcome {
    /// 添付アップロード成功。compose の attachment 列に追加する。
    AttachmentReady {
        media: MediaResponse,
        label: String,
    },
    /// アバター更新成功。`PATCH /api/v1/actor/profile` も済み済みで、whoami
    /// の `icon_url` を更新する。
    ProfileUpdated {
        icon_url: Option<String>,
        image_url: Option<String>,
        queued: usize,
        kind: PickerMode,
    },
    Failed {
        kind: PickerMode,
        message: String,
    },
}

/// メイン関数。`main.rs` から呼ぶ唯一のエントリ。
pub async fn run(options: TuiOptions) -> anyhow::Result<()> {
    let api = LocalApi::new(options.endpoint.clone(), options.token.clone());

    // whoami は接続テストも兼ねる。失敗したらここで abort して main にエラーを
    // 返す ── 端末はまだ raw mode に入っていないので追加の cleanup 不要。
    let endpoint_label = api.endpoint_display();
    let whoami = api
        .whoami()
        .await
        .with_context(|| format!("whoami via {endpoint_label}"))?;
    info!(
        ap_id = %whoami.ap_id,
        user = %whoami.preferred_username,
        endpoint = %endpoint_label,
        "TUI: connected to local API"
    );

    let socket_label = endpoint_label;

    // Picker は端末を実際に問い合わせる (escape sequence 送出 → 応答待ち)。
    // alt screen / raw mode に切り替える **前** に呼ぶのが穏当 ── 失敗しても
    // 画像表示は単に無効化するだけで TUI は続行する。Kitty 等で `?` を出すと
    // Foot や非対応端末では即座に Err になり、grace-degrade する。
    //
    // M9 PR2: 視覚刺激抑制で「画像系がすべて off」のときは端末問い合わせも
    // 省く ── 1 要素でも on なら問い合わせて Picker を確保する (= ランタイム
    // 中に toggle で on に戻したくなったときに使える)。
    let picker = if options.suppression.any_enabled() {
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
        info!("TUI: all image elements suppressed; skipping picker query");
        None
    };
    // M6: 画像取得は LocalApi 経由で server → media-proxy に委譲する。
    // ImageCache は API クライアントを clone して持つ (Arc 同等のコスト)。
    let images = ImageCache::new(picker.clone(), Some(api.clone()));
    // M7: ファイルピッカ用ローカル画像プレビュー。Picker は同じ端末向けの
    // ものを共有する (端末問い合わせを 2 度するのを避ける)。
    let previews = crate::preview::PreviewCache::new(picker);
    let mut app = App::new(
        options.theme.clone(),
        whoami,
        socket_label,
        images,
        previews,
        options.suppression,
    );

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

    // M7: アップロード結果を main_loop に戻すチャネル。capacity は 8 程度で
    // 十分 (= 同時アップロードは picker UX 的に 1 件 / 時々 2 件)。
    let (upload_tx, mut upload_rx) = mpsc::channel::<UploadOutcome>(8);

    let (mut terminal, enhancement_active) = init_terminal()?;
    let mut event_stream = EventStream::new();
    let mut last_rects = ui::PanelRects::default();

    let result = main_loop(
        &mut terminal,
        &mut app,
        &api,
        options.page_size,
        &mut event_stream,
        &mut sse_rx,
        &mut upload_rx,
        &upload_tx,
        &mut last_rects,
    )
    .await;

    restore_terminal(&mut terminal, enhancement_active)?;
    drop(sse_rx); // receiver drop → SSE task が次ループで終了。
    sse_task.abort();
    let _ = sse_task.await;
    result
}

#[allow(clippy::too_many_arguments, reason = "TUI mainloop は依存が多い")]
async fn main_loop(
    terminal: &mut TuiTerminal,
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    events: &mut EventStream,
    sse_rx: &mut mpsc::Receiver<StreamEvent>,
    upload_rx: &mut mpsc::Receiver<UploadOutcome>,
    upload_tx: &mpsc::Sender<UploadOutcome>,
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
        // M12 (#66): Follow Requests 一覧画面のスクロール追従。1 行 = 1 件
        // (アバター無し)。`last_rects.follow_requests` は直前フレームで
        // 確定した一覧領域の Rect。
        if let Some(fr) = app.follow_requests.as_mut() {
            let viewport = last_rects.follow_requests.height as usize;
            fr.ensure_visible(viewport);
        }
        // Issue #115: FollowList 画面のスクロール追従。avatar 表示時は 1 件 2 行、
        // 抑制時は 1 件 1 行 (= render_follow_list_screen と同じ row_step 計算)。
        // `last_rects.follow_list` は直前フレームで確定した一覧領域の Rect。
        {
            let avatar_enabled = app.images.enabled() && app.suppression.avatar;
            let row_step: u16 = if avatar_enabled { 2 } else { 1 };
            let viewport = (last_rects.follow_list.height / row_step.max(1)) as usize;
            if let Some(fl) = app.follow_list.as_mut() {
                fl.ensure_visible(viewport);
            }
        }

        tokio::select! {
            biased;

            maybe_evt = events.next() => {
                match maybe_evt {
                    Some(Ok(evt)) => handle_event(evt, app, api, page_size, last_rects, upload_tx).await,
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
            maybe_upload = upload_rx.recv() => {
                if let Some(outcome) = maybe_upload {
                    handle_upload_outcome(app, outcome);
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

fn redraw(terminal: &mut TuiTerminal, app: &mut App) -> anyhow::Result<ui::PanelRects> {
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
    upload_tx: &mpsc::Sender<UploadOutcome>,
) {
    let action = translate(event, app.focus);
    apply_action(action, app, api, page_size, rects, upload_tx).await;
}

#[allow(clippy::too_many_lines, reason = "single dispatcher for all actions")]
async fn apply_action(
    action: Action,
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    rects: &ui::PanelRects,
    upload_tx: &mpsc::Sender<UploadOutcome>,
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
        Action::FocusTimeline | Action::SuppressionClose => {
            app.focus = Focus::Timeline;
        }
        Action::ToggleHelp => {
            app.focus = if app.focus == Focus::Help {
                Focus::Timeline
            } else {
                // 開くたびに先頭から読めるよう scroll をリセット。
                // `last_*` 寸法は次フレームの render_help が書き戻す。
                app.help_state.scroll_top();
                Focus::Help
            };
        }
        Action::HelpScrollDown => {
            app.help_state.scroll_down(1);
        }
        Action::HelpScrollUp => {
            app.help_state.scroll_up(1);
        }
        Action::HelpPageDown => {
            let step = app.help_state.page_step();
            app.help_state.scroll_down(step);
        }
        Action::HelpPageUp => {
            let step = app.help_state.page_step();
            app.help_state.scroll_up(step);
        }
        Action::HelpScrollTop => {
            app.help_state.scroll_top();
        }
        Action::HelpScrollBottom => {
            app.help_state.scroll_bottom();
        }
        Action::RefreshTimeline => {
            let _g = InFlightGuard::new(app.in_flight.clone());
            match api.timeline_home(None, page_size).await {
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
            }
        }
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
            let _g = InFlightGuard::new(app.in_flight.clone());
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
            // round-4 review Finding 2: モーダル / overlay focus 中はマウス
            // ホイールが背後 Timeline に抜けないようガード。Note 詳細では
            // ホイールを `NoteDetailScrollDown/Up` に振り向ける ── UX 改善
            // も兼ねる。他 overlay (Suppression / Picker / EmojiSearch /
            // AltPrompt / Command) はホイール無視で十分。
            match app.focus {
                Focus::NoteDetail => {
                    if let Some(s) = app.note_detail.as_mut() {
                        if delta > 0 {
                            for _ in 0..delta {
                                s.scroll_down();
                            }
                        } else {
                            for _ in 0..(-delta) {
                                s.scroll_up();
                            }
                        }
                    }
                }
                Focus::Suppression
                | Focus::Picker
                | Focus::EmojiSearch
                | Focus::AltPrompt
                | Focus::Command
                | Focus::Requests => {
                    // overlay 中は背後 Timeline を動かさない。`Requests`
                    // (= follow request 承認画面) も同じく overlay 風だが
                    // round-4 で漏れていた (= round-6 review F7)。
                }
                _ => {
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
            }
        }
        Action::MouseClick(col, row) => {
            handle_click(app, rects, col, row);
        }
        Action::OpenPicker(mode) => open_picker(app, mode),
        Action::PickerNext => {
            if let Some(p) = app.picker.as_mut() {
                p.select_next();
            }
        }
        Action::PickerPrev => {
            if let Some(p) = app.picker.as_mut() {
                p.select_prev();
            }
        }
        Action::PickerPageDown => {
            if let Some(p) = app.picker.as_mut() {
                let h = rects.picker_list.height.saturating_sub(2).max(1) as usize;
                p.page_down(h);
            }
        }
        Action::PickerPageUp => {
            if let Some(p) = app.picker.as_mut() {
                let h = rects.picker_list.height.saturating_sub(2).max(1) as usize;
                p.page_up(h);
            }
        }
        Action::PickerActivate => picker_activate(app, api, upload_tx),
        Action::PickerParent => {
            if let Some(p) = app.picker.as_mut() {
                p.go_parent();
            }
        }
        Action::PickerToggleHidden => {
            if let Some(p) = app.picker.as_mut() {
                p.toggle_hidden();
            }
        }
        Action::PickerCancel => close_picker(app, false),
        Action::PopAttachment => {
            if let Some(att) = app.compose.pop_attachment() {
                app.set_status(
                    format!("detached: {}", att.label),
                    StatusKind::Info,
                    Some(Duration::from_secs(2)),
                );
            }
        }
        Action::OpenEmojiSearch => open_emoji_search(app, api).await,
        Action::EmojiSearchDown => {
            if let Some(s) = app.emoji_suggest.as_mut() {
                s.select_next();
            }
        }
        Action::EmojiSearchUp => {
            if let Some(s) = app.emoji_suggest.as_mut() {
                s.select_prev();
            }
        }
        Action::EmojiSearchConfirm => emoji_search_confirm(app, api, page_size).await,
        Action::EmojiSearchCancel => emoji_search_cancel(app),
        Action::EmojiSearchInsertChar(c) => {
            if let Some(s) = app.emoji_suggest.as_mut() {
                s.insert_char(c);
            }
        }
        Action::EmojiSearchBackspace => {
            if let Some(s) = app.emoji_suggest.as_mut() {
                s.backspace();
            }
        }
        Action::ToggleSuppression => toggle_suppression_overlay(app),
        Action::SuppressionNext => {
            let len = crate::suppression::Element::all().len();
            app.suppression_cursor = (app.suppression_cursor + 1) % len;
        }
        Action::SuppressionPrev => {
            let len = crate::suppression::Element::all().len();
            app.suppression_cursor = (app.suppression_cursor + len - 1) % len;
        }
        Action::SuppressionToggle => {
            let elements = crate::suppression::Element::all();
            if let Some(e) = elements.get(app.suppression_cursor) {
                app.suppression.toggle(*e);
                app.set_status(
                    format!(
                        "{} = {}",
                        e.label(),
                        if app.suppression.is_on(*e) {
                            "on"
                        } else {
                            "off"
                        },
                    ),
                    StatusKind::Info,
                    Some(Duration::from_secs(2)),
                );
            }
        }
        Action::SuppressionDisableAll => {
            app.suppression.disable_all();
            app.set_status(
                "all image elements suppressed",
                StatusKind::Info,
                Some(Duration::from_secs(2)),
            );
        }
        Action::SuppressionEnableAll => {
            app.suppression = crate::suppression::ImageSuppression::all_on();
            // [[m9-pr2-review]] Finding 3: 起動時に `any_enabled()=false` だと
            // Picker は `None` のままで再初期化できない (ratatui-image の
            // Picker は端末問い合わせを mid-runtime に再実行できない設計)。
            // ユーザに偽の "enabled" メッセージを返さないよう、現在の
            // ImageCache 状態を見て status の文面を切り替える。
            let msg = if app.images.enabled() {
                "all image elements enabled"
            } else {
                "suppression flags cleared (restart to enable images)"
            };
            app.set_status(msg, StatusKind::Info, Some(Duration::from_secs(3)));
        }
        Action::ReplyToSelected => start_reply(app),
        Action::UndoReactionOnSelected => undo_reaction(app, api, page_size).await,
        Action::RenoteSelected => send_renote(app, api, page_size).await,
        Action::UndoRenoteOnSelected => undo_renote(app, api, page_size).await,
        Action::AltPromptInsertChar(c) => {
            if let Some(p) = app.alt_prompt.as_mut() {
                p.insert_char(c);
            }
        }
        Action::AltPromptBackspace => {
            if let Some(p) = app.alt_prompt.as_mut() {
                p.backspace();
            }
        }
        Action::AltPromptSubmit => submit_alt_prompt(app, api, upload_tx),
        Action::AltPromptCancel => {
            if let Some(p) = app.alt_prompt.take() {
                app.set_status(
                    format!("upload cancelled: {}", p.label),
                    StatusKind::Info,
                    Some(Duration::from_secs(2)),
                );
            }
            app.focus = Focus::Compose;
        }
        Action::OpenProfileFromSelected => open_profile_from_selected(app, api, page_size).await,
        Action::ProfileSelectNext => {
            if let Some(p) = app.current_profile_mut() {
                p.select_next_note();
            }
        }
        Action::ProfileSelectPrev => {
            if let Some(p) = app.current_profile_mut() {
                p.select_prev_note();
            }
        }
        Action::ProfileLoadMoreNotes => profile_load_more_notes(app, api, page_size).await,
        Action::ProfileToggleFollow => profile_toggle_follow(app, api).await,
        Action::ProfileBack => profile_back(app),
        Action::ProfileRefresh => profile_refresh(app, api, page_size).await,
        Action::OpenCommand => open_command(app),
        Action::CommandInsertChar(c) => {
            if let Some(p) = app.command.as_mut() {
                p.insert_char(c);
            }
        }
        Action::CommandBackspace => {
            if let Some(p) = app.command.as_mut() {
                p.backspace();
            }
        }
        Action::CommandSubmit => command_submit(app, api, page_size).await,
        Action::CommandCancel => command_cancel(app),
        Action::CommandComplete => {
            if let Some(p) = app.command.as_mut() {
                p.complete();
            }
        }
        Action::FollowListSelectNext => {
            if let Some(fl) = app.follow_list.as_mut() {
                fl.select_next();
            }
        }
        Action::FollowListSelectPrev => {
            if let Some(fl) = app.follow_list.as_mut() {
                fl.select_prev();
            }
        }
        Action::FollowListPageDown => {
            let avatar_enabled = app.images.enabled() && app.suppression.avatar;
            let row_step: u16 = if avatar_enabled { 2 } else { 1 };
            let viewport = (rects.follow_list.height / row_step.max(1)) as usize;
            if let Some(fl) = app.follow_list.as_mut() {
                fl.select_page_down(viewport.max(1));
            }
        }
        Action::FollowListPageUp => {
            let avatar_enabled = app.images.enabled() && app.suppression.avatar;
            let row_step: u16 = if avatar_enabled { 2 } else { 1 };
            let viewport = (rects.follow_list.height / row_step.max(1)) as usize;
            if let Some(fl) = app.follow_list.as_mut() {
                fl.select_page_up(viewport.max(1));
            }
        }
        Action::FollowListToggleMode => follow_list_toggle_mode(app, api, page_size).await,
        Action::FollowListOpenSelected => {
            follow_list_open_selected(app, api, page_size).await;
        }
        Action::FollowListLoadMore => follow_list_load_more(app, api, page_size).await,
        Action::FollowListRefresh => follow_list_refresh(app, api, page_size).await,
        Action::FollowListClose => follow_list_close(app),
        Action::ActorLock => command_actor_lock(app, api, true).await,
        Action::ActorUnlock => command_actor_lock(app, api, false).await,
        Action::OpenFollowRequests => command_open_requests(app, api).await,
        Action::RequestsSelectNext => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.select_next();
            }
        }
        Action::RequestsSelectPrev => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.select_prev();
            }
        }
        Action::RequestsApproveSelected => requests_mutate_selected(app, api, true).await,
        Action::RequestsRejectSelected => requests_mutate_selected(app, api, false).await,
        Action::RequestsRefresh => requests_refresh(app, api).await,
        Action::RequestsClose => requests_close(app),
        Action::OpenNoteDetail => open_note_detail(app),
        Action::NoteDetailClose => close_note_detail(app),
        Action::NoteDetailScrollDown => {
            if let Some(s) = app.note_detail.as_mut() {
                s.scroll_down();
            }
        }
        Action::NoteDetailScrollUp => {
            if let Some(s) = app.note_detail.as_mut() {
                s.scroll_up();
            }
        }
        Action::NoteDetailNextAttachment => {
            if let Some(s) = app.note_detail.as_mut() {
                s.select_next_attachment();
            }
        }
        Action::NoteDetailPrevAttachment => {
            if let Some(s) = app.note_detail.as_mut() {
                s.select_prev_attachment();
            }
        }
        Action::NoteDetailToggleReveal => {
            if let Some(s) = app.note_detail.as_mut() {
                s.toggle_reveal();
            }
        }
    }
}

/// Issue #133 (3): Timeline で `Enter` ── 選択中 Note の snapshot を取って
/// 詳細モーダルを開く。空 timeline / 範囲外なら status だけ更新して focus
/// は移さない (= UI 状態を壊さない)。
fn open_note_detail(app: &mut App) {
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let origin = app.focus;
    let emoji_visible = app.suppression.emoji;
    app.note_detail = Some(crate::note_detail::NoteDetailScreen::new(
        note.clone(),
        origin,
        emoji_visible,
    ));
    app.focus = Focus::NoteDetail;
}

/// Issue #133 (3): `Esc` / `q` でモーダルを閉じる。`origin` に記録した
/// 起動元 Focus に戻す ── 現状は Timeline からしか開けないが、将来
/// Profile 経路を増やしたときに「閉じると Timeline に飛ばされる」事故を
/// 起こさない。Note: `note_detail` を `take()` してから focus 操作に進む
/// (= 順序逆だと state を持ったまま Timeline focus に戻りバグの温床)。
fn close_note_detail(app: &mut App) {
    let origin = app
        .note_detail
        .as_ref()
        .map_or(Focus::Timeline, |s| s.origin);
    app.note_detail = None;
    app.focus = origin;
}

/// `p` で選択中の Note の author を Profile push する。
async fn open_profile_from_selected(app: &mut App, api: &LocalApi, page_size: i64) {
    // 本関数は actor_id を解決して [`push_profile_for_actor_id`] を呼ぶ
    // 薄い wrapper。後者が `InFlightGuard` を持つので、ここでは二重 inc を
    // 避けるため guard を作らない。
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let actor_id = note.actor_id;
    push_profile_for_actor_id(app, api, actor_id, page_size).await;
}

/// `actor_id` から actor + relationship + 直近 notes を取り、Profile stack に
/// 1 段 push する。失敗時は status だけ更新して focus は変えない。
async fn push_profile_for_actor_id(app: &mut App, api: &LocalApi, actor_id: i64, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let actor = match api.get_actor(actor_id).await {
        Ok(resp) => resp.actor,
        Err(err) => {
            app.set_status(
                format!("actor lookup failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
            return;
        }
    };
    let relationship = match api.get_relationship(actor_id).await {
        Ok(r) => r,
        Err(err) => {
            // 自分自身の lookup 等は 200 + neutral で返る想定だが、移行時の
            // 互換性として「relationship 不明 → neutral 扱い」を許す。
            warn!(
                ?err,
                actor_id, "relationship fetch failed; neutral fallback"
            );
            crate::client::Relationship::neutral()
        }
    };
    let notes = match api.list_actor_notes(actor_id, None, page_size).await {
        Ok(resp) => resp,
        Err(err) => {
            app.set_status(
                format!("actor notes fetch failed: {err}"),
                StatusKind::Warning,
                Some(Duration::from_secs(5)),
            );
            crate::client::TimelineResponse {
                notes: Vec::new(),
                next_before_id: None,
            }
        }
    };
    let acct = format!("@{}@{}", actor.preferred_username, actor.host);
    app.profile_stack.push(ProfileScreen::new(
        actor,
        relationship,
        notes.notes,
        notes.next_before_id,
    ));
    app.focus = Focus::Profile;
    app.set_status(
        format!("profile: {acct}"),
        StatusKind::Info,
        Some(Duration::from_secs(2)),
    );
}

async fn profile_load_more_notes(app: &mut App, api: &LocalApi, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(profile) = app.current_profile() else {
        return;
    };
    if profile.notes_exhausted {
        app.set_status(
            "no more notes",
            StatusKind::Info,
            Some(Duration::from_secs(2)),
        );
        return;
    }
    let actor_id = profile.actor.id;
    let before = profile.next_before_id;
    match api.list_actor_notes(actor_id, before, page_size).await {
        Ok(resp) => {
            let n = resp.notes.len();
            if let Some(p) = app.current_profile_mut() {
                p.append_older_notes(resp.notes, resp.next_before_id);
            }
            app.set_status(
                format!("loaded {n} older"),
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

async fn profile_toggle_follow(app: &mut App, api: &LocalApi) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(profile) = app.current_profile() else {
        return;
    };
    let actor_id = profile.actor.id;
    let acct_label = profile.acct();
    let is_self_lookup = profile.actor.ap_id == app.whoami.ap_id;
    if is_self_lookup {
        app.set_status(
            "cannot follow yourself",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    }
    let active = profile.has_active_follow();

    if active {
        // Unfollow ── relationship に同梱された `follow_id` を使って
        // `DELETE /api/v1/follow/{id}` を撃つ。pending / accepted のときだけ
        // server が `follow_id` を露出する仕様 (= rejected のときは toggle 自体
        // を出さない、has_active_follow が false なので)。
        let Some(follow_id) = profile.relationship.follow_id else {
            app.set_status(
                "relationship has no follow_id; refresh and try again",
                StatusKind::Warning,
                Some(Duration::from_secs(4)),
            );
            return;
        };
        match api.unfollow(follow_id).await {
            Ok(_) => {
                if let Some(p) = app.current_profile_mut() {
                    p.update_relationship(crate::client::Relationship {
                        following: false,
                        follow_state: None,
                        followed_by: p.relationship.followed_by,
                        follow_id: None,
                    });
                }
                app.set_status(
                    format!("unfollowed {acct_label}"),
                    StatusKind::Success,
                    Some(Duration::from_secs(3)),
                );
            }
            Err(err) => {
                app.set_status(
                    format!("unfollow failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                );
            }
        }
    } else {
        match api.follow(&FollowTarget::for_actor_id(actor_id)).await {
            Ok(resp) => {
                let label = if resp.already_accepted {
                    format!("{acct_label} (already following)")
                } else {
                    format!("follow requested → {acct_label}")
                };
                if let Some(p) = app.current_profile_mut() {
                    p.update_relationship(crate::client::Relationship {
                        following: resp.state == "accepted",
                        follow_state: Some(resp.state.clone()),
                        followed_by: p.relationship.followed_by,
                        follow_id: Some(resp.follow_id),
                    });
                }
                app.set_status(label, StatusKind::Success, Some(Duration::from_secs(3)));
            }
            Err(err) => {
                app.set_status(
                    format!("follow failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                );
            }
        }
    }
}

fn profile_back(app: &mut App) {
    app.profile_stack.pop();
    if app.profile_stack.is_empty() {
        // M13 PR5: Profile を抜けたあと FollowList が下層に居れば戻る。
        // 例: Timeline → `:following` → FollowList → Enter → Profile → Esc
        // → FollowList。
        app.focus = if app.follow_list.is_some() {
            Focus::FollowList
        } else {
            Focus::Timeline
        };
    }
}

async fn profile_refresh(app: &mut App, api: &LocalApi, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(profile) = app.current_profile() else {
        return;
    };
    let actor_id = profile.actor.id;
    // actor 本体は再取得しない (= DB の値で十分、Update Activity を rerun したい
    // ケースは別 issue)。relationship + notes を撃ち直す。
    if let Ok(rel) = api.get_relationship(actor_id).await
        && let Some(p) = app.current_profile_mut()
    {
        p.update_relationship(rel);
    }
    match api.list_actor_notes(actor_id, None, page_size).await {
        Ok(resp) => {
            if let Some(p) = app.current_profile_mut() {
                p.notes = resp.notes;
                p.next_before_id = resp.next_before_id;
                p.notes_exhausted = p.notes.is_empty();
                p.selected_note = 0;
                p.note_top = 0;
            }
            app.set_status(
                "profile refreshed",
                StatusKind::Success,
                Some(Duration::from_secs(2)),
            );
        }
        Err(err) => {
            app.set_status(
                format!("profile refresh failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(5)),
            );
        }
    }
}

fn toggle_suppression_overlay(app: &mut App) {
    if app.focus == Focus::Suppression {
        app.focus = Focus::Timeline;
    } else {
        app.focus = Focus::Suppression;
        app.suppression_cursor = 0;
    }
}

fn start_reply(app: &mut App) {
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    // 親 note のラベルは「@user@host: 抜粋 (60 文字)」。author host が無い
    // ローカル post も `@user` だけは出るので識別子として使える。
    // AP HTML はプレーン化してから切り詰める ── Timeline 表示と同じ
    // 形にして compose 画面で `<p>...</p>` 等を生で見せない。
    let plain = crate::content::to_plain_text(&note.content);
    let mut excerpt: String = plain.chars().take(60).collect();
    if plain.chars().count() > 60 {
        excerpt.push('…');
    }
    excerpt = excerpt.replace('\n', " ");
    let label = format!("@{}: {}", note.actor_preferred_username, excerpt);
    app.compose.set_reply_target(note.ap_id.clone(), label);
    app.focus = Focus::Compose;
    app.set_status(
        format!("replying to #{}", note.id),
        StatusKind::Info,
        Some(Duration::from_secs(3)),
    );
}

async fn undo_reaction(app: &mut App, api: &LocalApi, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let note_id = note.id;
    let Some(reaction_id) = app.last_reaction_ids.get(&note_id).copied() else {
        app.set_status(
            "no recent reaction to undo on this note",
            StatusKind::Warning,
            Some(Duration::from_secs(3)),
        );
        return;
    };
    match api.delete_reaction(reaction_id).await {
        Ok(()) => {
            app.last_reaction_ids.remove(&note_id);
            app.set_status(
                "reaction removed",
                StatusKind::Success,
                Some(Duration::from_secs(3)),
            );
            if let Ok(resp) = api.timeline_home(None, page_size).await {
                app.replace_timeline(resp.notes, resp.next_before_id);
            }
        }
        Err(err) => {
            app.set_status(
                format!("undo failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// Issue #118: 絵文字検索モーダルを開く。
///
/// 起動経路は 2 つ:
///   - Timeline `e` → 選択中 Note への即時リアクション送信
///     (`Mode::ReactToNote(note_id)`)
///   - Compose `Ctrl-E` → 本文 buffer に `:shortcode:` / Unicode 1 字を挿入
///     (`Mode::InsertIntoCompose`)
///
/// モードは現在の Focus から自動判定。`Mode::ReactToNote` で選択中 Note が
/// 存在しないときは status 警告だけ出してモーダルは開かない。
///
/// server `/api/v1/emojis` から custom emoji を fetch し、`EmojiSuggestState`
/// が静的 Unicode emoji を merge する。fetch 失敗時はモーダル開かず status
/// 表示 ── Timeline `e` 経路ではユーザ側で再試行できる。
async fn open_emoji_search(app: &mut App, api: &LocalApi) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let mode = match app.focus {
        Focus::Timeline => {
            let Some(note) = app.notes.get(app.selected) else {
                app.set_status(
                    "no note selected",
                    StatusKind::Warning,
                    Some(Duration::from_secs(2)),
                );
                return;
            };
            crate::emoji_suggest::Mode::ReactToNote(note.id)
        }
        Focus::Compose => crate::emoji_suggest::Mode::InsertIntoCompose,
        _ => return,
    };
    match api.list_emojis("", crate::emoji_suggest::FETCH_LIMIT).await {
        Ok(resp) => {
            app.emoji_suggest = Some(crate::emoji_suggest::EmojiSuggestState::open(
                mode, resp.items,
            ));
            app.focus = Focus::EmojiSearch;
        }
        Err(err) => {
            tracing::warn!(?err, "emoji search fetch failed");
            app.set_status(
                format!("emoji search failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(5)),
            );
        }
    }
}

/// Enter 確定: モードに応じて即リアクション送信 or 本文挿入。
///
/// `Mode::ReactToNote(note_id)` の場合は `POST /api/v1/reactions` を打ち、
/// 成功なら timeline を再取得して reaction count を反映する。失敗は status
/// に出してモーダルだけ閉じる (= ユーザ操作を奪い続けない)。
///
/// `Mode::InsertIntoCompose` の場合は `EmojiItem::content_token()` を compose
/// 本文に挿入し、Compose focus に戻る。
async fn emoji_search_confirm(app: &mut App, api: &LocalApi, page_size: i64) {
    // `ReactToNote` 経路は `send_reaction` が `InFlightGuard` を持ち、
    // `InsertIntoCompose` 経路はネットワーク呼び出しを伴わないため、
    // 本関数自身では guard を作らない (PR #149 review 二重 inc 修正)。
    let Some(state) = app.emoji_suggest.as_ref() else {
        return;
    };
    let mode = state.mode;
    let Some(item) = state.current().cloned() else {
        // 候補なし → 何もせず閉じる。
        emoji_search_cancel(app);
        return;
    };
    let token = item.content_token();
    app.emoji_suggest = None;

    match mode {
        crate::emoji_suggest::Mode::ReactToNote(note_id) => {
            app.focus = Focus::Timeline;
            send_reaction(app, api, note_id, &token, page_size).await;
        }
        crate::emoji_suggest::Mode::InsertIntoCompose => {
            for ch in token.chars() {
                app.compose.insert_char(ch);
            }
            app.focus = Focus::Compose;
        }
    }
}

/// Esc キャンセル: 何もせずモーダルを閉じ、モード由来の元 Focus に戻る。
fn emoji_search_cancel(app: &mut App) {
    let mode = app.emoji_suggest.as_ref().map(|s| s.mode);
    app.emoji_suggest = None;
    app.focus = match mode {
        Some(crate::emoji_suggest::Mode::InsertIntoCompose) => Focus::Compose,
        _ => Focus::Timeline,
    };
}

/// `POST /api/v1/notes/{id}/renote` ── 選択中 Note を renote する (#151)。
/// 成功時は timeline を再取得して `announce_count` / `viewer_renoted` を反映。
/// visibility が public / unlisted 以外なら server 側 400 で弾かれ、status に
/// エラー表示する。
async fn send_renote(app: &mut App, api: &LocalApi, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let note_id = note.id;
    match api.create_renote(note_id).await {
        Ok(resp) => {
            app.last_renote_ids.insert(note_id, resp.id);
            app.set_status(
                format!("renoted #{note_id} ({} queued)", resp.queued_deliveries),
                StatusKind::Success,
                Some(Duration::from_secs(3)),
            );
            if let Ok(resp) = api.timeline_home(None, page_size).await {
                app.replace_timeline(resp.notes, resp.next_before_id);
            }
        }
        Err(err) => {
            app.set_status(
                format!("renote failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `DELETE /api/v1/notes/{id}/renote` ── 自分の renote を取り消し (#151)。
/// path には選択中 Note の id を渡す ── サーバが `(note_id, local_actor.id)`
/// で announce row を引いて Undo を配送する。`last_renote_ids` 経由のヒント
/// が無くても動くが、UI 整合のために taken して削除する。
async fn undo_renote(app: &mut App, api: &LocalApi, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let note_id = note.id;
    match api.delete_renote(note_id).await {
        Ok(()) => {
            app.last_renote_ids.remove(&note_id);
            app.set_status(
                "renote removed",
                StatusKind::Success,
                Some(Duration::from_secs(3)),
            );
            if let Ok(resp) = api.timeline_home(None, page_size).await {
                app.replace_timeline(resp.notes, resp.next_before_id);
            }
        }
        Err(err) => {
            app.set_status(
                format!("undo renote failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `POST /api/v1/reactions` 本体。Timeline `e` 経路で確定した `content` を
/// 送る。成功時は timeline を再取得して reaction count を反映する。
async fn send_reaction(app: &mut App, api: &LocalApi, note_id: i64, content: &str, page_size: i64) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    match api.create_reaction(note_id, content).await {
        Ok(resp) => {
            // M13 PR6: 取り消し (`u`) で参照するため reaction id を覚えておく。
            // 同じ note に上書きすると以前の id が落ちるが、サーバは 1 user 1
            // reaction 制約があるので「最後の 1 件」だけ追えれば足りる。
            app.last_reaction_ids.insert(note_id, resp.id);
            app.set_status(
                format!(
                    "reacted with {} ({} queued)",
                    resp.content, resp.queued_deliveries
                ),
                StatusKind::Success,
                Some(Duration::from_secs(3)),
            );
            // 成功 → タイムラインを取り直して reaction count を反映。
            if let Ok(resp) = api.timeline_home(None, page_size).await {
                app.replace_timeline(resp.notes, resp.next_before_id);
            }
        }
        Err(err) => {
            app.set_status(
                format!("reaction failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

fn open_picker(app: &mut App, mode: PickerMode) {
    // attachment ピッカは compose 上限を先に弾く ── ユーザを picker に入れて
    // から「上限超え」と知らせるより UX が良い。
    if mode == PickerMode::Attachment && app.compose.attachments_full() {
        app.set_status(
            "attachments full (max 4)",
            StatusKind::Warning,
            Some(Duration::from_secs(3)),
        );
        return;
    }
    let start = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    app.picker = Some(FilePicker::new(mode, start));
    app.focus = Focus::Picker;
    app.set_status(
        format!("file picker: {} (Enter=select, Esc=cancel)", mode.label()),
        StatusKind::Info,
        Some(Duration::from_secs(4)),
    );
}

/// `keep_compose = true` のときはピッカ閉じて Compose に戻る。Attachment
/// モードでキャンセル / 完了したときに使う ── ピッカ前の入力中だった本文
/// を失わないため (PR #43 review Minor)。
fn close_picker(app: &mut App, keep_compose: bool) {
    let mode = app.picker.as_ref().map(|p| p.mode);
    app.picker = None;
    app.focus = if keep_compose || matches!(mode, Some(PickerMode::Attachment)) {
        Focus::Compose
    } else {
        Focus::Timeline
    };
}

fn picker_activate(app: &mut App, api: &LocalApi, upload_tx: &mpsc::Sender<UploadOutcome>) {
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    let mode = picker.mode;
    match picker.activate() {
        Activation::Noop | Activation::Descended => {}
        Activation::Selected(path) => {
            // attachment はこのタイミングで上限再確認 (picker 開閉中に他経路で
            // 添付が増えることは無いが、二重押下対策で念のため)。
            if mode == PickerMode::Attachment && app.compose.attachments_full() {
                app.set_status(
                    "attachments full (max 4)",
                    StatusKind::Warning,
                    Some(Duration::from_secs(3)),
                );
                return;
            }
            let label = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("(file)")
                .to_string();
            // M13 PR6: Attachment は alt text プロンプトを挟む。Avatar/Header
            // は alt text 概念が無い (= AP `name` を載せる先が無い) ので
            // 即時アップロード。
            if mode == PickerMode::Attachment {
                close_picker(app, true);
                app.alt_prompt = Some(crate::alt_prompt::AltPrompt::new(mode, path, label));
                app.focus = Focus::AltPrompt;
                app.set_status(
                    "alt text (Enter to submit / Esc to skip & cancel)",
                    StatusKind::Info,
                    None,
                );
                return;
            }
            app.pending_uploads = app.pending_uploads.saturating_add(1);
            app.set_status(
                format!("uploading {label} as {}...", mode.label()),
                StatusKind::Info,
                None,
            );
            close_picker(app, false);
            let api = api.clone();
            let tx = upload_tx.clone();
            let guard = InFlightGuard::new(app.in_flight.clone());
            tokio::spawn(async move {
                // guard を move して保持 ── アップロード完了 (= tx.send 後)
                // にこの closure を抜けて drop され、in_flight が dec される。
                let _g = guard;
                let outcome = run_upload(api, mode, path, label, None).await;
                let _ = tx.send(outcome).await;
            });
        }
    }
}

/// M13 PR6: alt text 入力確定 → upload kick。空入力 (Enter のみ) でも
/// 通る ── 空文字は `run_upload` 側で `None` 同等扱い。
fn submit_alt_prompt(app: &mut App, api: &LocalApi, upload_tx: &mpsc::Sender<UploadOutcome>) {
    let Some(prompt) = app.alt_prompt.take() else {
        return;
    };
    let alt = prompt.alt_text().to_string();
    let alt_arg = if alt.is_empty() { None } else { Some(alt) };
    let mode = prompt.mode;
    let path = prompt.path;
    let label = prompt.label;
    app.focus = Focus::Compose;
    app.pending_uploads = app.pending_uploads.saturating_add(1);
    app.set_status(
        format!("uploading {label} as {}...", mode.label()),
        StatusKind::Info,
        None,
    );
    let api = api.clone();
    let tx = upload_tx.clone();
    let guard = InFlightGuard::new(app.in_flight.clone());
    tokio::spawn(async move {
        // guard を move して保持 ── alt 入力後アップロード完了で drop。
        let _g = guard;
        let outcome = run_upload(api, mode, path, label, alt_arg).await;
        let _ = tx.send(outcome).await;
    });
}

/// アップロード前の TUI 側ファイルサイズ上限 (25 MiB)。`config/default.toml`
/// の `media_proxy.max_bytes` (= 25 MiB) と揃える。本来 TUI は config を
/// 持たないので決め打ち ── server 側で同値の上限が再チェックされるため、
/// この値より大きく見積もって TUI が拒否しないケースが出ても安全に 413 で
/// 返る。但し 4GiB 動画を誤選択した際に TUI ホストプロセスが OOM する事故
/// を避けるため、ローカルで弾く方が UX 上望ましい (PR #43 review Medium)。
const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// バックグラウンドアップロードの本体。bytes を読み、`POST /api/v1/media`、
/// 必要なら `PATCH /api/v1/actor/profile` まで叩いて結果を返す。
async fn run_upload(
    api: LocalApi,
    mode: PickerMode,
    path: std::path::PathBuf,
    label: String,
    alt: Option<String>,
) -> UploadOutcome {
    // ファイルを読む **前** にメタデータで上限チェック。`tokio::fs::read`
    // はサイズ無制限に Vec に積むので、4 GiB 動画を選んでも握り込んで
    // しまう。`MAX_UPLOAD_BYTES` で先に弾く (= server 側 `max_bytes` と
    // 二重防御)。
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.len() > MAX_UPLOAD_BYTES => {
            return UploadOutcome::Failed {
                kind: mode,
                message: format!(
                    "file too large: {} bytes (max {MAX_UPLOAD_BYTES})",
                    meta.len()
                ),
            };
        }
        Ok(_) => {}
        Err(err) => {
            return UploadOutcome::Failed {
                kind: mode,
                message: format!("stat {}: {err}", path.display()),
            };
        }
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(err) => {
            return UploadOutcome::Failed {
                kind: mode,
                message: format!("read {}: {err}", path.display()),
            };
        }
    };
    let media = match api
        .upload_media(mode.as_kind(), alt.as_deref(), bytes)
        .await
    {
        Ok(m) => m,
        Err(err) => {
            return UploadOutcome::Failed {
                kind: mode,
                message: format!("upload: {err}"),
            };
        }
    };
    match mode {
        PickerMode::Attachment => UploadOutcome::AttachmentReady { media, label },
        PickerMode::Avatar => {
            let req = ProfileUpdate {
                icon_media_id: Some(media.id),
                ..ProfileUpdate::default()
            };
            match api.patch_profile(&req).await {
                Ok(resp) => UploadOutcome::ProfileUpdated {
                    icon_url: resp.icon_url,
                    image_url: resp.image_url,
                    queued: resp.queued_deliveries,
                    kind: mode,
                },
                Err(err) => UploadOutcome::Failed {
                    kind: mode,
                    message: format!("profile patch: {err}"),
                },
            }
        }
        PickerMode::Header => {
            let req = ProfileUpdate {
                image_media_id: Some(media.id),
                ..ProfileUpdate::default()
            };
            match api.patch_profile(&req).await {
                Ok(resp) => UploadOutcome::ProfileUpdated {
                    icon_url: resp.icon_url,
                    image_url: resp.image_url,
                    queued: resp.queued_deliveries,
                    kind: mode,
                },
                Err(err) => UploadOutcome::Failed {
                    kind: mode,
                    message: format!("profile patch: {err}"),
                },
            }
        }
    }
}

fn handle_upload_outcome(app: &mut App, outcome: UploadOutcome) {
    app.pending_uploads = app.pending_uploads.saturating_sub(1);
    match outcome {
        UploadOutcome::AttachmentReady { media, label } => {
            let added = app.compose.add_attachment(AttachmentRef {
                media_id: media.id,
                label: label.clone(),
            });
            if added {
                app.set_status(
                    format!("attached {label} ({}x{})", media.width, media.height),
                    StatusKind::Success,
                    Some(Duration::from_secs(3)),
                );
            } else {
                app.set_status(
                    "attachments full (max 4); upload discarded",
                    StatusKind::Warning,
                    Some(Duration::from_secs(4)),
                );
            }
        }
        UploadOutcome::ProfileUpdated {
            icon_url,
            image_url,
            queued,
            kind,
        } => {
            // whoami をローカル更新しておく ── サーバへ再 whoami しなくても
            // すぐ UI に反映される (アバターウィジェット等)。
            if let Some(url) = icon_url {
                app.whoami.icon_url = Some(url);
            }
            if let Some(url) = image_url {
                app.whoami.image_url = Some(url);
            }
            app.set_status(
                format!("{} updated ({queued} delivered)", kind.label()),
                StatusKind::Success,
                Some(Duration::from_secs(4)),
            );
        }
        UploadOutcome::Failed { kind, message } => {
            app.set_status(
                format!("{} upload failed: {message}", kind.label()),
                StatusKind::Error,
                Some(Duration::from_secs(8)),
            );
        }
    }
}

fn handle_click(app: &mut App, rects: &ui::PanelRects, col: u16, row: u16) {
    // [[m9-pr2-review]] Finding 1 + #133 PR3 round-2 C1: overlay 系 focus
    // (Suppression / Picker / EmojiSearch / AltPrompt / Command / NoteDetail)
    // の最中は背後パネルへの hit test を抜けさせない ── クリックでサイレント
    // に overlay が閉じてしまい、背後のノートが選択されたり compose に
    // フォーカスが奪われるのを防ぐ。`NoteDetail` を入れずに置くと、モーダル
    // 外クリックで focus だけが Timeline に書き換わり `app.note_detail` は
    // ゴミデータとして残る split state を起こす。Help は overlay 中の
    // クリックで明示的に閉じる従来挙動を維持 (既存テストの依存)。
    if matches!(
        app.focus,
        Focus::Suppression
            | Focus::Picker
            | Focus::EmojiSearch
            | Focus::AltPrompt
            | Focus::Command
            | Focus::NoteDetail,
    ) {
        return;
    }
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
        in_reply_to_ap_id: app.compose.in_reply_to_ap_id().map(str::to_string),
        attachment_ids: app.compose.attachment_ids(),
    };
    let _g = InFlightGuard::new(app.in_flight.clone());
    match api.create_note(&req).await {
        Ok(resp) => {
            app.set_status(
                format!("posted #{} ({} delivered)", resp.id, resp.queued_deliveries),
                StatusKind::Success,
                Some(Duration::from_secs(4)),
            );
            remember_compose_state_and_reseed(app);
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

/// Issue #93: 送信成功直後の compose リセット。
///
/// 1. 直前送信時の (visibility / sensitive / CW 使用有無) を
///    `app.last_compose_defaults` に控えておく。
/// 2. `Compose::clear` で本文・カーソル・添付・返信先などを全部リセット。
/// 3. `apply_last_defaults` で 1. の 3 値だけを再シードする。CW 本文 (`cw`)
///    は引き継がない ── 内容は投稿ごとに固有なので毎回新規入力させる。
///
/// 「失敗時 (POST エラー / Esc 離脱) には更新しない」という Issue 仕様は
/// この関数を成功パスからだけ呼ぶことで担保する。
fn remember_compose_state_and_reseed(app: &mut App) {
    app.last_compose_defaults = app.compose.snapshot_defaults();
    app.compose.clear();
    app.compose.apply_last_defaults(app.last_compose_defaults);
}

// ── M13 PR5: Command prompt & FollowList ──────────────────────────────

fn open_command(app: &mut App) {
    // Timeline / FollowList から開ける。Profile からは PR4 keymap で `:` を
    // 処理していないのでここに来ない (= 想定通り)。
    app.command = Some(crate::command::CommandPrompt::new());
    app.focus = Focus::Command;
}

fn command_cancel(app: &mut App) {
    app.command = None;
    app.focus = current_screen_focus(app);
}

/// Profile / `FollowList` が残っている場合はそこへ、両方無ければ Timeline へ
/// 戻る ── command prompt を閉じたあとに使う共通関数。
fn current_screen_focus(app: &App) -> Focus {
    if !app.profile_stack.is_empty() {
        Focus::Profile
    } else if app.follow_list.is_some() {
        Focus::FollowList
    } else {
        Focus::Timeline
    }
}

async fn command_submit(app: &mut App, api: &LocalApi, page_size: i64) {
    use crate::command::Command;
    // 本関数は dispatcher で、配下の `command_open_*` / `command_follow_*` /
    // `command_actor_lock` / `command_open_requests` 等が個別に
    // `InFlightGuard` を持つ。ここで guard を作ると counter が二重に
    // increment されるので意図的に作らない (= UI には影響しないが意味
    // のあるカウンタを保つため)。
    let Some(prompt) = app.command.take() else {
        return;
    };
    app.focus = current_screen_focus(app);
    let raw = prompt.buffer.trim().to_string();
    let cmd = crate::command::parse(&raw);
    match cmd {
        Command::Quit => {
            app.should_quit = true;
        }
        Command::Help => {
            // Help overlay は Focus::Help。コマンド経路では明示的にトグルする。
            // ToggleHelp 経路と同様、開いたら先頭から読めるよう scroll を
            // リセット。
            app.help_state.scroll_top();
            app.focus = Focus::Help;
        }
        Command::OpenSelf => {
            command_open_self(app, api, page_size).await;
        }
        Command::ListFollowing => {
            open_follow_list(
                app,
                api,
                page_size,
                crate::follow_list::FollowListMode::Following,
            )
            .await;
        }
        Command::ListFollowers => {
            open_follow_list(
                app,
                api,
                page_size,
                crate::follow_list::FollowListMode::Followers,
            )
            .await;
        }
        Command::Open(target) => {
            command_open_target(app, api, page_size, &target).await;
        }
        Command::Follow(target) => {
            command_follow_target(app, api, &target, false).await;
        }
        Command::Unfollow(target) => {
            command_follow_target(app, api, &target, true).await;
        }
        Command::Lock => command_actor_lock(app, api, true).await,
        Command::Unlock => command_actor_lock(app, api, false).await,
        Command::OpenRequests => command_open_requests(app, api).await,
        Command::Renote => send_renote(app, api, page_size).await,
        Command::Unrenote => undo_renote(app, api, page_size).await,
        Command::Invalid { reason } => {
            app.set_status(
                format!(":: {reason}"),
                StatusKind::Warning,
                Some(Duration::from_secs(5)),
            );
        }
        Command::Unknown { name } => {
            app.set_status(
                format!("unknown command: :{name}"),
                StatusKind::Warning,
                Some(Duration::from_secs(4)),
            );
        }
    }
}

async fn command_open_self(app: &mut App, api: &LocalApi, page_size: i64) {
    let lookup_guard = InFlightGuard::new(app.in_flight.clone());
    let ap_id = app.whoami.ap_id.clone();
    match api.lookup_actor_by_ap_id(&ap_id).await {
        Ok(resp) => {
            // `lookup_actor_by_ap_id` 完了済み。続く `push_profile_from_lookup`
            // も guard を持つので、ここで明示 drop して二重 inc を避ける
            // (PR #149 review)。
            drop(lookup_guard);
            push_profile_from_lookup(app, api, page_size, resp).await;
        }
        Err(err) => {
            // Err 経路は後続 API 呼び出しなしなのでスコープ末尾で自然 drop。
            app.set_status(
                format!(":me failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(5)),
            );
        }
    }
}

async fn command_open_target(
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    target: &crate::command::LookupTarget,
) {
    let lookup_guard = InFlightGuard::new(app.in_flight.clone());
    let resp = match target {
        crate::command::LookupTarget::Acct(acct) => api.lookup_actor_by_acct(acct).await,
        crate::command::LookupTarget::ApId(uri) => api.lookup_actor_by_ap_id(uri).await,
    };
    match resp {
        Ok(r) => {
            // lookup 完了。続く `push_profile_from_lookup` も guard を持つ
            // ので明示 drop して二重 inc を避ける (PR #149 review)。
            drop(lookup_guard);
            push_profile_from_lookup(app, api, page_size, r).await;
        }
        Err(err) => {
            app.set_status(
                format!(":open failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `lookup_actor_by_*` の戻りから Profile screen を構築して push。
async fn push_profile_from_lookup(
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    resp: crate::client::ActorWithRelationship,
) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let actor_id = resp.actor.id;
    let acct = format!("@{}@{}", resp.actor.preferred_username, resp.actor.host);
    let notes = match api.list_actor_notes(actor_id, None, page_size).await {
        Ok(t) => t,
        Err(err) => {
            warn!(
                ?err,
                actor_id, "actor notes fetch failed; opening with empty"
            );
            crate::client::TimelineResponse {
                notes: Vec::new(),
                next_before_id: None,
            }
        }
    };
    app.profile_stack.push(ProfileScreen::new(
        resp.actor,
        resp.relationship,
        notes.notes,
        notes.next_before_id,
    ));
    app.focus = Focus::Profile;
    app.set_status(
        format!("profile: {acct}"),
        StatusKind::Info,
        Some(Duration::from_secs(2)),
    );
}

async fn command_follow_target(
    app: &mut App,
    api: &LocalApi,
    target: &crate::command::LookupTarget,
    unfollow: bool,
) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    // 共通: target を解決して relationship を得る。
    let resp = match target {
        crate::command::LookupTarget::Acct(acct) => api.lookup_actor_by_acct(acct).await,
        crate::command::LookupTarget::ApId(uri) => api.lookup_actor_by_ap_id(uri).await,
    };
    let resolved = match resp {
        Ok(r) => r,
        Err(err) => {
            app.set_status(
                format!(":(un)follow lookup failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
            return;
        }
    };
    let acct_label = format!(
        "@{}@{}",
        resolved.actor.preferred_username, resolved.actor.host
    );
    if unfollow {
        let Some(follow_id) = resolved.relationship.follow_id else {
            app.set_status(
                format!("not currently following {acct_label}"),
                StatusKind::Warning,
                Some(Duration::from_secs(4)),
            );
            return;
        };
        match api.unfollow(follow_id).await {
            Ok(_) => {
                app.set_status(
                    format!("unfollowed {acct_label}"),
                    StatusKind::Success,
                    Some(Duration::from_secs(3)),
                );
            }
            Err(err) => {
                app.set_status(
                    format!("unfollow failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                );
            }
        }
    } else {
        match api
            .follow(&FollowTarget::for_actor_id(resolved.actor.id))
            .await
        {
            Ok(r) => {
                let label = if r.already_accepted {
                    format!("already following {acct_label}")
                } else {
                    format!("follow requested → {acct_label}")
                };
                app.set_status(label, StatusKind::Success, Some(Duration::from_secs(3)));
            }
            Err(err) => {
                app.set_status(
                    format!("follow failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                );
            }
        }
    }
}

/// M12 (#66): `:lock` / `:unlock` 実行ハンドラ。
///
/// 成功時は `LockResponse` を見て (a) 既に同じ状態 → `no-op`、(b) 切替成功 +
/// queued 配送本数を status に出す。`enqueue_failures > 0` のときは warn 扱い
/// で個別 follower への配送失敗があったことを示す ── 配送自体は state 切替の
/// 副作用 (= 相手側 UI のキャッシュ更新) なので、失敗していても切替は完了して
/// いる。
async fn command_actor_lock(app: &mut App, api: &LocalApi, lock: bool) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let verb = if lock { "lock" } else { "unlock" };
    let result = if lock {
        api.actor_lock().await
    } else {
        api.actor_unlock().await
    };
    match result {
        Ok(resp) => {
            let prefix = if resp.changed {
                format!(":{verb} ok ({} Update queued)", resp.queued_deliveries)
            } else {
                format!(":{verb} no-op (already in target state)")
            };
            let kind = if resp.enqueue_failures > 0 {
                StatusKind::Warning
            } else {
                StatusKind::Info
            };
            let body = if resp.enqueue_failures > 0 {
                format!(
                    "{prefix}; {} follower(s) failed to enqueue",
                    resp.enqueue_failures
                )
            } else {
                prefix
            };
            app.set_status(body, kind, Some(Duration::from_secs(6)));
        }
        Err(err) => {
            app.set_status(
                format!(":{verb} failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// M12 (#66): `:requests` 実行 ── 一覧画面を開く + 初回 fetch。失敗しても
/// 画面は開く (= 空表示でユーザに通知)。
///
/// **PR #95 review fix**: `app.follow_requests = Some(...)` + `focus` 切替
/// を `await` の **前** に行う。これにより fetch 中も `"loading…"` が描画
/// され、`requests_refresh` と挙動が揃う。
async fn command_open_requests(app: &mut App, api: &LocalApi) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let mut screen = crate::follow_requests::FollowRequestsScreen::new();
    screen.fetching = true;
    app.follow_requests = Some(screen);
    app.focus = Focus::Requests;
    let result = api.list_follow_requests().await;
    match result {
        Ok(resp) => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.replace(resp.items);
            }
        }
        Err(err) => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.fetching = false;
            }
            app.set_status(
                format!(":requests fetch failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `a` / `x` ── 選択中の row を approve / reject 配信し、成功すれば list から
/// 除去する。失敗時は除去せず status に出す (= ユーザが再試行可能)。
async fn requests_mutate_selected(app: &mut App, api: &LocalApi, approve: bool) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(screen) = app.follow_requests.as_ref() else {
        return;
    };
    let Some(target) = screen.current() else {
        app.set_status(
            "no pending request selected",
            StatusKind::Warning,
            Some(Duration::from_secs(4)),
        );
        return;
    };
    let id = target.id;
    let follower = target.follower_ap_id.clone();
    let verb = if approve { "approve" } else { "reject" };
    let result = if approve {
        api.approve_follow_request(id).await
    } else {
        api.reject_follow_request(id).await
    };
    match result {
        Ok(resp) => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.remove_id(resp.id);
            }
            app.set_status(
                format!("{verb}d follow from {follower}"),
                StatusKind::Info,
                Some(Duration::from_secs(5)),
            );
        }
        Err(err) => {
            app.set_status(
                format!("{verb} failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `r` ── 再取得。
async fn requests_refresh(app: &mut App, api: &LocalApi) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let Some(screen) = app.follow_requests.as_mut() else {
        return;
    };
    screen.fetching = true;
    match api.list_follow_requests().await {
        Ok(resp) => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.replace(resp.items);
            }
        }
        Err(err) => {
            if let Some(s) = app.follow_requests.as_mut() {
                s.fetching = false;
            }
            app.set_status(
                format!("refresh failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
        }
    }
}

/// `Esc` / `q` ── 画面を閉じて Timeline へ戻る。state は破棄する (= 再 fetch
/// 込みで `:requests` を再実行する流れ)。
fn requests_close(app: &mut App) {
    app.follow_requests = None;
    app.focus = Focus::Timeline;
}

async fn open_follow_list(
    app: &mut App,
    api: &LocalApi,
    page_size: i64,
    mode: crate::follow_list::FollowListMode,
) {
    let _g = InFlightGuard::new(app.in_flight.clone());
    let entries = fetch_follow_list_page(api, mode, None, page_size).await;
    let mut screen = crate::follow_list::FollowListScreen::new(mode);
    match entries {
        Ok((rows, next)) => {
            screen.current_mut().replace(rows, next);
        }
        Err(err) => {
            app.set_status(
                format!(":{} failed: {err}", mode.label()),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
            // 失敗しても画面は開く (= 空表示でユーザに状況を伝える)。
        }
    }
    app.follow_list = Some(screen);
    app.focus = Focus::FollowList;
}

async fn fetch_follow_list_page(
    api: &LocalApi,
    mode: crate::follow_list::FollowListMode,
    before_id: Option<i64>,
    page_size: i64,
) -> Result<(Vec<crate::client::FollowListEntry>, Option<i64>), ApiError> {
    let resp = match mode {
        crate::follow_list::FollowListMode::Following => {
            api.list_following(before_id, page_size).await?
        }
        crate::follow_list::FollowListMode::Followers => {
            api.list_followers(before_id, page_size).await?
        }
    };
    Ok((resp.entries, resp.next_before_id))
}

async fn follow_list_toggle_mode(app: &mut App, api: &LocalApi, page_size: i64) {
    let Some(fl) = app.follow_list.as_mut() else {
        return;
    };
    fl.toggle_mode();
    // 反対タブを 1 度も fetch していなかったら今 fetch する (= UX 待ちが
    // 短い `:following` ↔ `:followers` 切替時にも常に直近データが見える)。
    if !fl.current().fetched {
        let mode = fl.mode;
        let _g = InFlightGuard::new(app.in_flight.clone());
        let res = fetch_follow_list_page(api, mode, None, page_size).await;
        if let Some(fl) = app.follow_list.as_mut() {
            match res {
                Ok((rows, next)) => fl.current_mut().replace(rows, next),
                Err(err) => app.set_status(
                    format!("{} fetch failed: {err}", mode.label()),
                    StatusKind::Error,
                    Some(Duration::from_secs(6)),
                ),
            }
        }
    }
}

async fn follow_list_open_selected(app: &mut App, api: &LocalApi, page_size: i64) {
    let Some(fl) = app.follow_list.as_ref() else {
        return;
    };
    let Some(entry) = fl.current_entry() else {
        app.set_status(
            "no entry selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    let actor_id = entry.actor.id;
    push_profile_for_actor_id(app, api, actor_id, page_size).await;
}

async fn follow_list_load_more(app: &mut App, api: &LocalApi, page_size: i64) {
    let Some(fl) = app.follow_list.as_ref() else {
        return;
    };
    let page = fl.current();
    if page.exhausted {
        app.set_status(
            "no more entries",
            StatusKind::Info,
            Some(Duration::from_secs(2)),
        );
        return;
    }
    let mode = fl.mode;
    let before = page.next_before_id;
    let _g = InFlightGuard::new(app.in_flight.clone());
    let res = fetch_follow_list_page(api, mode, before, page_size).await;
    if let Some(fl) = app.follow_list.as_mut() {
        match res {
            Ok((rows, next)) => {
                let n = rows.len();
                fl.current_mut().append(rows, next);
                app.set_status(
                    format!("loaded {n} more"),
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
}

async fn follow_list_refresh(app: &mut App, api: &LocalApi, page_size: i64) {
    let Some(fl) = app.follow_list.as_ref() else {
        return;
    };
    let mode = fl.mode;
    let _g = InFlightGuard::new(app.in_flight.clone());
    let res = fetch_follow_list_page(api, mode, None, page_size).await;
    if let Some(fl) = app.follow_list.as_mut() {
        match res {
            Ok((rows, next)) => {
                fl.current_mut().replace(rows, next);
                fl.selected = 0;
                fl.top = 0;
                app.set_status(
                    format!("{} refreshed", mode.label()),
                    StatusKind::Success,
                    Some(Duration::from_secs(2)),
                );
            }
            Err(err) => {
                app.set_status(
                    format!("refresh failed: {err}"),
                    StatusKind::Error,
                    Some(Duration::from_secs(5)),
                );
            }
        }
    }
}

fn follow_list_close(app: &mut App) {
    app.follow_list = None;
    // Profile stack が残っているケース (= FollowList → Profile → Esc → FollowList
    // → Esc) は無い (Profile に行ったら follow_list はそのまま、profile_back で
    // FollowList に戻る → Esc で本関数が呼ばれて follow_list が None になる)。
    // よって profile_back のように分岐は要らず、必ず Timeline に戻る。
    app.focus = Focus::Timeline;
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

/// 端末の初期化。戻り値の bool は **Kitty keyboard protocol を有効化できたか**
/// (= [`PushKeyboardEnhancementFlags`] が通ったか)。`restore_terminal` で対称的に
/// [`PopKeyboardEnhancementFlags`] を呼ぶか判断するため呼び出し側に渡す。
///
/// この拡張プロトコルが効くと `Ctrl-Enter` / `Ctrl-J` 等の修飾キーが modifier
/// 付きの [`KeyEvent`] として届くようになる。**無効化のままだと普通の VT 端末
/// (xterm / `GNOME` Terminal / tmux 等) では Ctrl-Enter が単なる `\r` として
/// 来てしまい、modifier も立たないので compose 送信ができない**。Kitty /
/// `WezTerm` / Alacritty (CSI u) / foot 等は対応する。
///
/// tmux 中継経由や非対応端末では `supports_keyboard_enhancement` が false /
/// Err を返すので push しない (= 従来挙動)。その場合の救済は代替送信キー
/// (`F2`) に倒す ── 詳細は [`crate::event::translate_compose_key`]。
fn init_terminal() -> anyhow::Result<(TuiTerminal, bool)> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen + mouse capture")?;
    let mut enhancement_active = false;
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        // 最小フラグ: DISAMBIGUATE_ESCAPE_CODES のみ。REPORT_EVENT_TYPES (=
        // Release/Repeat 配信) は既存 event loop が Release を捨てる前提なので
        // 入れない (= 動作変化を最小化)。
        if execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            )
        )
        .is_ok()
        {
            enhancement_active = true;
        }
    }
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).context("create ratatui Terminal")?;
    Ok((terminal, enhancement_active))
}

fn restore_terminal(terminal: &mut TuiTerminal, enhancement_active: bool) -> anyhow::Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    // 対称的に Pop ── push していない端末で Pop だけ呼ぶと端末によっては
    // 不明シーケンスとして表示される事故が報告されているので、push の成否を
    // bool で持ち回す設計にしている。
    if enhancement_active {
        let _ = execute!(
            terminal.backend_mut(),
            crossterm::event::PopKeyboardEnhancementFlags,
        );
    }
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

    fn make_test_app() -> App {
        use crate::client::Whoami;
        use crate::image_cache::ImageCache;
        use crate::suppression::ImageSuppression;
        App::new(
            Theme::default(),
            Whoami {
                ap_id: "https://x.test/users/me".into(),
                preferred_username: "me".into(),
                host: "x.test".into(),
                display_name: None,
                summary: None,
                icon_url: None,
                image_url: None,
                inbox: "https://x.test/users/me/inbox".into(),
                outbox: None,
            },
            "test".into(),
            ImageCache::new(None, None),
            crate::preview::PreviewCache::new(None),
            ImageSuppression::default(),
        )
    }

    #[test]
    fn click_in_overlay_focus_is_ignored() {
        // [[m9-pr2-review]] Finding 1: Suppression / Picker / EmojiSearch
        // が開いている間のクリックは背後パネルへ抜けない (= overlay が
        // サイレントに閉じてノートが選択される事故を防ぐ)。
        let mut app = make_test_app();
        let rects = ui::PanelRects {
            timeline: ratatui::layout::Rect::new(0, 0, 80, 24),
            compose: ratatui::layout::Rect::new(0, 24, 80, 5),
            ..ui::PanelRects::default()
        };
        for focus in [Focus::Suppression, Focus::Picker, Focus::EmojiSearch] {
            app.focus = focus;
            handle_click(&mut app, &rects, 10, 5);
            assert_eq!(
                app.focus, focus,
                "click in {focus:?} focus must not change focus"
            );
        }
    }

    /// Issue #93: 送信成功時の reseed は (visibility / sensitive / CW 使用有無)
    /// を覚え、`Compose::clear` で本文側を全部捨てたあと 3 値だけ書き戻す。
    /// CW 本文 (`cw`) は引き継がない。
    #[test]
    fn remember_compose_state_reseeds_three_axes_after_clear() {
        use crate::compose::Visibility;

        let mut app = make_test_app();
        app.compose.insert_char('h');
        app.compose.insert_char('i');
        // visibility = followers, sensitive = on, CW あり (内容 "nsfw")。
        app.compose.cycle_visibility(); // public -> unlisted
        app.compose.cycle_visibility(); // unlisted -> followers
        app.compose.toggle_sensitive();
        app.compose.toggle_cw_focus(); // CW 入力モードに
        app.compose.insert_char('n');
        app.compose.insert_char('s');
        app.compose.insert_char('f');
        app.compose.insert_char('w');
        app.compose.toggle_cw_focus(); // 本文に戻して送信前の状態を再現

        remember_compose_state_and_reseed(&mut app);

        // app.last_compose_defaults に「直前の値」が保存される。
        assert_eq!(app.last_compose_defaults.visibility, Visibility::Followers);
        assert!(app.last_compose_defaults.sensitive);
        assert!(app.last_compose_defaults.cw_enabled);
        // compose 自身も 3 値だけ再シードされる。
        assert_eq!(app.compose.visibility(), Visibility::Followers);
        assert!(app.compose.sensitive());
        assert!(app.compose.editing_cw());
        // 本文・CW 本文・カーソルは捨てる (= clear 経路)。
        assert!(app.compose.buffer().is_empty());
        assert!(app.compose.cw().is_empty());
        assert_eq!(app.compose.cursor(), 0);
    }

    /// CW を使わずに送信したケースは `cw_enabled = false` を保持し、次回
    /// compose を開いたとき `editing_cw` は false (= 本文フォーカス) で
    /// 始まる。`sensitive` も同様。
    #[test]
    fn remember_compose_state_keeps_defaults_when_axes_unused() {
        use crate::compose::Visibility;

        let mut app = make_test_app();
        app.compose.insert_char('y');

        remember_compose_state_and_reseed(&mut app);

        assert_eq!(app.last_compose_defaults.visibility, Visibility::Public);
        assert!(!app.last_compose_defaults.sensitive);
        assert!(!app.last_compose_defaults.cw_enabled);
        assert_eq!(app.compose.visibility(), Visibility::Public);
        assert!(!app.compose.sensitive());
        assert!(!app.compose.editing_cw());
    }
}
