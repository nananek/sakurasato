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
use crate::compose::{AttachmentRef, Visibility};
use crate::event::{Action, translate};
use crate::image_cache::ImageCache;
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
        &mut upload_rx,
        &upload_tx,
        &mut last_rects,
    )
    .await;

    restore_terminal(&mut terminal)?;
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
        Action::OpenReactionPrompt => open_reaction_prompt(app),
        Action::ReactionPromptInsertChar(c) => {
            if let Some(p) = app.reaction_prompt.as_mut() {
                p.insert_char(c);
            }
        }
        Action::ReactionPromptBackspace => {
            if let Some(p) = app.reaction_prompt.as_mut() {
                p.backspace();
            }
        }
        Action::ReactionPromptSubmit => {
            submit_reaction(app, api, page_size).await;
        }
        Action::ReactionPromptCancel => {
            close_reaction_prompt(app);
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
    }
}

/// `p` で選択中の Note の author を Profile push する。
async fn open_profile_from_selected(app: &mut App, api: &LocalApi, page_size: i64) {
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
        app.focus = Focus::Timeline;
    }
}

async fn profile_refresh(app: &mut App, api: &LocalApi, page_size: i64) {
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
    let mut excerpt: String = note.content.chars().take(60).collect();
    if note.content.chars().count() > 60 {
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

fn open_reaction_prompt(app: &mut App) {
    let Some(note) = app.notes.get(app.selected) else {
        app.set_status(
            "no note selected",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    };
    app.reaction_prompt = Some(crate::reaction_prompt::ReactionPrompt::new(note.id));
    app.focus = Focus::ReactionPrompt;
}

fn close_reaction_prompt(app: &mut App) {
    app.reaction_prompt = None;
    app.focus = Focus::Timeline;
}

async fn submit_reaction(app: &mut App, api: &LocalApi, page_size: i64) {
    let Some(prompt) = app.reaction_prompt.as_ref() else {
        return;
    };
    if prompt.is_empty() {
        app.set_status(
            "reaction is empty",
            StatusKind::Warning,
            Some(Duration::from_secs(2)),
        );
        return;
    }
    let note_id = prompt.note_id;
    let content = prompt.buffer.trim().to_string();

    match api.create_reaction(note_id, &content).await {
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
        }
        Err(err) => {
            app.set_status(
                format!("reaction failed: {err}"),
                StatusKind::Error,
                Some(Duration::from_secs(6)),
            );
            // 失敗時はプロンプトを残したままにしてユーザが修正できるようにする。
            return;
        }
    }
    close_reaction_prompt(app);

    // 成功 → タイムラインを取り直して reaction count を反映。
    if let Ok(resp) = api.timeline_home(None, page_size).await {
        app.replace_timeline(resp.notes, resp.next_before_id);
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
            tokio::spawn(async move {
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
    tokio::spawn(async move {
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
    // [[m9-pr2-review]] Finding 1: overlay 系 focus (Suppression / Picker /
    // ReactionPrompt) の最中は背後パネルへの hit test を抜けさせない ──
    // クリックでサイレントに overlay が閉じてしまい、背後のノートが選択
    // されたり compose にフォーカスが奪われるのを防ぐ。Help は overlay 中の
    // クリックで明示的に閉じる従来挙動を維持 (既存テストの依存)。
    if matches!(
        app.focus,
        Focus::Suppression | Focus::Picker | Focus::ReactionPrompt,
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
        Visibility::Direct,
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
        // [[m9-pr2-review]] Finding 1: Suppression / Picker / ReactionPrompt
        // が開いている間のクリックは背後パネルへ抜けない (= overlay が
        // サイレントに閉じてノートが選択される事故を防ぐ)。
        let mut app = make_test_app();
        let rects = ui::PanelRects {
            timeline: ratatui::layout::Rect::new(0, 0, 80, 24),
            compose: ratatui::layout::Rect::new(0, 24, 80, 5),
            ..ui::PanelRects::default()
        };
        for focus in [Focus::Suppression, Focus::Picker, Focus::ReactionPrompt] {
            app.focus = focus;
            handle_click(&mut app, &rects, 10, 5);
            assert_eq!(
                app.focus, focus,
                "click in {focus:?} focus must not change focus"
            );
        }
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
        // M13 PR6: Direct を含む 4 値 cycle に拡張。
        assert_eq!(visibility_steps(Visibility::Public, Visibility::Direct), 3);
        assert_eq!(
            visibility_steps(Visibility::Followers, Visibility::Public),
            2
        );
        assert_eq!(visibility_steps(Visibility::Direct, Visibility::Public), 1);
    }
}
