//! TUI 描画関数群。
//!
//! 描画は副作用のない関数として書き、`App` (= state) と [`Theme`] を受け取って
//! `Frame` に書き込むだけにする。すべての色は `theme.palette.*` 経由 (色の
//! ハードコード禁止 ── CLAUDE.md §10)。
//!
//! レイアウト:
//!
//! ```text
//! ┌─ sakurasato 🌸 ─────────────────────────────────────┐
//! │ timeline                                           │
//! │ ...                                                │
//! ├─ compose [public] [CW] [SENS] [N left] ────────────┤
//! │ ...                                                │
//! ├─ socket=/run/... user=@me focus=timeline ──────────┤
//! └────────────────────────────────────────────────────┘
//! ```

use chrono::Local;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui_image::Image;

use crate::app::{App, Focus, StatusKind};
use crate::client::TimelineNote;
use crate::follow_list::{FollowListMode, FollowListScreen};
use crate::profile::ProfileScreen;
use crate::theme::{Palette, Theme};

/// avatar をレンダリングするときに左側へ確保する cell 数。`width` = この値、
/// `height` = 2 行で固定 (header + CW or 1 行 content)。3 セル × 2 行は
/// Kitty/Sixel/iTerm2 の最小フォントサイズでも視認できる程度。
const AVATAR_CELLS_W: u16 = 4;
const AVATAR_CELLS_H: u16 = 2;

/// 非フォーカス note の本文を Timeline で折りたたむ最大行数。「最大」は
/// 超過時の indicator 行を含む最終行数 ── 通常は [`AVATAR_CELLS_H`] と
/// 同じ 2 行に揃え、視覚的にアバター高さと整合させる。
const UNFOCUSED_BODY_LINES: usize = 2;
/// フォーカス中の note の本文を表示する最大行数 (折りたたみ後)。十分
/// 閲覧できる量を確保しつつ、Timeline 1 画面に複数 note が並ぶ余地を残す。
const FOCUSED_BODY_LINES: usize = 5;

pub mod hit;

/// 描画したパネルの矩形 (マウスヒット判定用)。
#[derive(Debug, Default, Clone)]
pub struct PanelRects {
    pub timeline: Rect,
    /// タイムライン内の各 note のヒット行。`(note_index, top_row, height)`。
    pub timeline_rows: ScrollHits,
    pub compose: Rect,
    pub help: Option<Rect>,
    /// M7: ピッカ表示中はリスト部分の矩形 (= PageDown/Up の高さ算出用)。
    /// 非表示時は zero rect。
    pub picker_list: Rect,
    /// M13 PR4: Profile 画面の notes 一覧領域 (= PageDown/Up 高さ算出用)。
    /// 非表示時は zero rect。
    pub profile_notes: Rect,
    /// M12 (#66): Follow Requests 画面の一覧領域 (= `ensure_visible` 用)。
    /// 非表示時は zero rect。
    pub follow_requests: Rect,
    /// #206 PR3: 通知一覧画面の一覧領域 (= `ensure_visible` 用)。非表示時は zero rect。
    pub notifications: Rect,
    /// Issue #115: `FollowList` 画面の一覧領域 (= `ensure_visible` / `PageDown` 用)。
    /// 非表示時は zero rect。
    pub follow_list: Rect,
    /// リスト機能の一覧 / メンバー一覧領域 (= `ensure_visible` 用)。
    /// 非表示時は zero rect。
    pub lists: Rect,
}

/// タイムラインのスクロール可能領域内に並んだ note の行位置をビット圧縮せず
/// `Vec` に積む。MouseClick 解決時に上から線形探索する (高々 80 件)。
pub type ScrollHits = hit::ScrollHits;

/// 1 frame 分を描画。返り値は次の `MouseClick` を解決するためのレイアウト矩形。
///
/// `app` は `&mut` ── Help overlay scroll で、描画した content の総行数 /
/// viewport を [`crate::app::HelpState`] に書き戻すため。
pub fn draw(frame: &mut Frame<'_>, app: &mut App) -> PanelRects {
    let area = frame.area();

    // 縦 3 段: timeline / compose / status
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(compose_height(app)),
            Constraint::Length(1),
        ])
        .split(area);
    let timeline_area = chunks[0];
    let compose_area = chunks[1];
    let status_area = chunks[2];

    // M13 PR4: Profile が積まれているときは Timeline 領域を Profile で
    // 上書きする (compose / status バーは下に残す ── 終了したら Timeline に
    // 戻る視覚的連続性のため)。M13 PR5 で FollowList も同様に Timeline 領域を
    // 占有する画面として描く。
    let mut profile_notes_rect = Rect::default();
    let mut follow_requests_rect = Rect::default();
    let mut notifications_rect = Rect::default();
    let mut follow_list_rect = Rect::default();
    let mut lists_rect = Rect::default();
    let rows = if matches!(app.focus, Focus::Profile)
        && let Some(profile) = app.current_profile()
    {
        profile_notes_rect = render_profile_screen(frame, timeline_area, app, profile);
        ScrollHits::default()
    } else if matches!(app.focus, Focus::FollowList)
        && let Some(fl) = app.follow_list.as_ref()
    {
        follow_list_rect = render_follow_list_screen(frame, timeline_area, app, fl);
        ScrollHits::default()
    } else if matches!(app.focus, Focus::Requests)
        && let Some(fr) = app.follow_requests.as_ref()
    {
        follow_requests_rect = render_follow_requests_screen(frame, timeline_area, app, fr);
        ScrollHits::default()
    } else if matches!(app.focus, Focus::Notifications)
        && let Some(n) = app.notifications.as_ref()
    {
        notifications_rect = render_notifications_screen(frame, timeline_area, app, n);
        ScrollHits::default()
    } else if matches!(app.focus, Focus::Lists)
        && let Some(ls) = app.lists.as_ref()
    {
        lists_rect = render_lists_screen(frame, timeline_area, app, ls);
        ScrollHits::default()
    } else {
        render_timeline(frame, timeline_area, app)
    };
    render_compose(frame, compose_area, app);
    render_status(frame, status_area, app);

    let help_area = if app.focus == Focus::Help {
        Some(render_help(frame, area, app))
    } else {
        None
    };

    // M7: ピッカは画面中央 overlay。Help と同じ層に出すので Help と排他的に
    // しなくてもいいが、両方同時に出ると操作が混乱するので Picker focus 時
    // は Help は描かない設計 (= 上で focus == Help のときだけ render_help)。
    let picker_list = if app.focus == Focus::Picker {
        // ピッカ overlay は最下段の status バーを覆わない。status には
        // `render_status` が「file picker: <mode> (Enter=select, Esc=cancel)」の
        // キーヒントを出しており、ピッカを開いている間こそ見せたい (ボックス内に
        // 同じヒントは無い)。全画面塗り (fill_modal_backdrop) が status 行まで
        // 潰すと、このヒントが実ユーザーからも federation TUI テストからも消える。
        let overlay_area = Rect {
            height: area.height.saturating_sub(status_area.height),
            ..area
        };
        render_picker(frame, overlay_area, app)
    } else {
        Rect::default()
    };

    // Issue #118: 絵文字検索モーダル。Timeline / Compose の上に中央 overlay
    // で描画する (= 旧 reaction prompt 経路は廃止、`e` で直接ここに来る)。
    if app.focus == Focus::EmojiSearch
        && let Some(s) = app.emoji_suggest.as_ref()
    {
        render_emoji_suggest(frame, area, app, s);
    }

    // Issue #133 (3): Note 詳細モーダル。Timeline 上に中央 overlay で出す。
    if app.focus == Focus::NoteDetail
        && let Some(s) = app.note_detail.as_ref()
    {
        render_note_detail(frame, area, app, s);
    }

    // M9 PR2: 視覚刺激抑制トグル overlay。
    if app.focus == Focus::Suppression {
        render_suppression_overlay(frame, area, app);
    }

    // M13 PR6: alt text 入力プロンプト。status バーに上書き表示する。
    if app.focus == Focus::AltPrompt
        && let Some(p) = app.alt_prompt.as_ref()
    {
        render_alt_prompt(frame, status_area, &app.theme, p);
    }

    // M13 PR5: `:` コマンドプロンプト。status バーに上書き表示する。
    if app.focus == Focus::Command
        && let Some(p) = app.command.as_ref()
    {
        render_command_prompt(frame, status_area, &app.theme, p);
    }

    PanelRects {
        timeline: timeline_area,
        timeline_rows: rows,
        compose: compose_area,
        help: help_area,
        picker_list,
        profile_notes: profile_notes_rect,
        follow_requests: follow_requests_rect,
        notifications: notifications_rect,
        follow_list: follow_list_rect,
        lists: lists_rect,
    }
}

/// 中央ボックス型モーダル overlay の背後に敷く全画面バックドロップ。
///
/// これらの overlay は端末の一部だけを覆うため、塗らないとボックス外の余白から
/// 背後の Timeline のテキストや Kitty アバターが透ける。ratatui-image 11 系の
/// Kitty は unicode-placeholder 方式で、画像は「プレースホルダ文字が入ったセル」
/// に表示されるため、テキストだけでなくそのセルを上書きしないと端末側の画像も
/// 残る。画面全体を `Clear` + `palette.background` で塗り潰すことで下層を隠し、
/// placeholder セルも上書きして次フレーム差分で端末側の残像画像を消す。背景色は
/// 前フレームと同一なので差分で再送されず、ちらつかない (焦点変化時の #286 全画面
/// 再描画とも整合)。status バーに出す小さな prompt (command / alt) は 1 行を完全に
/// 覆うので対象外。
fn fill_modal_backdrop(frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.background)),
        area,
    );
}

/// Issue #118: 絵文字検索モーダル。
///
/// レイアウト (上から):
///   - 検索 buffer (1 行)
///   - ヘルプ (1 行)
///   - 余白 (1 行)
///   - **プレビュー枠** (`PREVIEW_ROWS` 行) ── custom はカーソル中の画像を
///     キャッシュ経由で `ratatui-image` 描画、Unicode は codepoint を中央に
///     大きく文字描画。`emoji` suppression が off のときは枠ごと省く。
///   - 候補リスト (`VISIBLE_MAX` 行まで、cursor で自動スクロール)
///
/// 候補 0 件でも閉じない (= search buffer を消せば全候補が戻る)。
fn render_emoji_suggest(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &crate::emoji_suggest::EmojiSuggestState,
) {
    /// プレビュー枠の高さ (border 込みのモーダル外寸ではなく **inner** の行数)。
    /// 画像枠は最低 5 行確保しないと Kitty graphics protocol で潰れて見えない。
    const PREVIEW_ROWS: u16 = 6;

    let palette = &app.theme.palette;
    fill_modal_backdrop(frame, area, palette);
    let visible_max = crate::emoji_suggest::VISIBLE_MAX;
    let visible = state.filtered.len().min(visible_max).max(1);
    let show_preview = app.suppression.is_on(crate::suppression::Element::Emoji);
    let mode_label = match state.mode {
        crate::emoji_suggest::Mode::ReactToNote(_) => "react",
        crate::emoji_suggest::Mode::InsertIntoCompose => "insert",
    };
    let enter_label = match state.mode {
        crate::emoji_suggest::Mode::ReactToNote(_) => "Enter=send",
        crate::emoji_suggest::Mode::InsertIntoCompose => "Enter=insert",
    };

    // モーダル外寸: 検索 1 + ヘルプ 1 + 余白 1 + (preview + 余白 1) + 候補 visible + border 2。
    let preview_block = if show_preview { PREVIEW_ROWS + 1 } else { 0 };
    let h_inner = 1 + 1 + 1 + preview_block + u16::try_from(visible).unwrap_or(8);
    let h = h_inner + 2; // borders top + bottom
    let w = 56u16.min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h.min(area.height));

    let block = Block::default()
        .title(Span::styled(
            format!(
                "  emoji search · {} ({})  ",
                mode_label,
                state.filtered.len()
            ),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // inner を縦に分割: 検索 1 / ヘルプ 1 / 余白 1 / (preview / 余白 1) / 候補 残り。
    let constraints: Vec<Constraint> = if show_preview {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(PREVIEW_ROWS),
            Constraint::Length(1),
            Constraint::Min(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let query_area = chunks[0];
    let help_area = chunks[1];
    let (preview_area, list_area) = if show_preview {
        (Some(chunks[3]), chunks[5])
    } else {
        (None, chunks[3])
    };

    // 検索 buffer 行。
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "  / ",
                Style::default()
                    .fg(palette.accent_strong)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(state.query.clone(), Style::default().fg(palette.foreground)),
            Span::styled("▏", Style::default().fg(palette.accent)),
        ])),
        query_area,
    );
    // ヘルプ行。
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  [↑↓ navigate  {enter_label}  Esc=cancel]"),
            Style::default().fg(palette.muted),
        ))),
        help_area,
    );

    // プレビュー枠。
    if let Some(prev_area) = preview_area {
        render_emoji_preview(frame, prev_area, app, state);
    }

    // 候補リスト。
    render_emoji_list(frame, list_area, palette, state, visible);
}

/// `render_emoji_suggest` から呼ぶ候補リスト描画。
fn render_emoji_list(
    frame: &mut Frame<'_>,
    area: Rect,
    palette: &Palette,
    state: &crate::emoji_suggest::EmojiSuggestState,
    visible: usize,
) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible);
    if state.filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let scroll_top = scroll_window_top(state.cursor, visible, state.filtered.len());
    for (idx, item) in state
        .filtered
        .iter()
        .enumerate()
        .skip(scroll_top)
        .take(visible)
    {
        let selected = idx == state.cursor;
        let marker = if selected { "▶ " } else { "  " };
        let marker_style = if selected {
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        // Unicode / custom とも `:shortcode:` のみで揃える。Unicode の
        // codepoint を行頭に併記する旧仕様は列ズレを生むだけで意味が薄い
        // ため撤去 (= 実際の絵姿は preview 枠が担当する)。
        let mut spans = vec![
            Span::styled(marker.to_string(), marker_style),
            Span::styled(
                format!(":{}:", item.shortcode),
                Style::default().fg(palette.foreground),
            ),
        ];
        if let Some(cat) = item.category.as_deref()
            && !cat.is_empty()
        {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("[{cat}]"),
                Style::default().fg(palette.muted),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// `render_emoji_suggest` から呼ぶプレビュー描画。
///
/// - custom emoji: media-proxy 経由で取得済みなら `ratatui-image` で描画、
///   未取得なら fetch を `ensure()` し、描画では「loading…」を出す。
/// - Unicode emoji: codepoint 文字列を枠中央に大きく表示。フォント拡大は
///   端末側でしか効かないので「ASCII で大きく」までは出来ないが、目立つ
///   位置と色を当てて存在感を出す。
fn render_emoji_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &crate::emoji_suggest::EmojiSuggestState,
) {
    let palette = &app.theme.palette;
    let Some(item) = state.current() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "  (no candidate)",
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(p, area);
        return;
    };

    match item.kind {
        crate::client::EmojiKind::Unicode => {
            // codepoint を枠中央に大きめに置く。フォントサイズは端末依存だが、
            // 中央寄せ + 上下マージンで「ここに 1 個だけある」感は出る。
            let cp = item.codepoint.as_deref().unwrap_or(item.shortcode.as_str());
            let mid_row = area.y + area.height / 2;
            let mid_area = Rect::new(area.x, mid_row, area.width, 1);
            let p = Paragraph::new(Line::from(Span::styled(
                cp.to_string(),
                Style::default()
                    .fg(palette.accent_strong)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(p, mid_area);
        }
        crate::client::EmojiKind::Custom => {
            // custom emoji は image cache から protocol を引いて描画する。
            // suppression::Emoji が off の場合はそもそもこの関数を呼ばない
            // ([[render_emoji_suggest]] 側で枠ごと省略済み)。
            if item.url.is_empty() {
                let p = Paragraph::new(Line::from(Span::styled(
                    "  (no image url)",
                    Style::default().fg(palette.muted),
                )));
                frame.render_widget(p, area);
                return;
            }
            // 中央に正方枠を切る (= 画像はだいたい正方形)。
            let side = area.width.min(area.height * 2);
            let img_w = side.min(area.width);
            let img_h = (img_w / 2).min(area.height);
            let img_x = area.x + (area.width.saturating_sub(img_w)) / 2;
            let img_y = area.y + (area.height.saturating_sub(img_h)) / 2;
            let img_area = Rect::new(img_x, img_y, img_w, img_h);

            // Issue #134: 候補 popup の絵文字プレビューは emoji variant で
            // 取得する (= 512×512 box)。avatar 固定だった旧経路では
            // media-proxy で 256×256 に再リサイズされ、サーバ側に保存済みの
            // 512 焼き WebP を毎回 decode + 縮小 + 再エンコードしていた。
            app.images
                .ensure_with_variant(&item.url, img_area, crate::image_cache::VARIANT_EMOJI);
            if let Some(proto) = app.images.get(&item.url) {
                let widget = Image::new(proto.as_ref());
                frame.render_widget(widget, img_area);
            } else {
                let p = Paragraph::new(Line::from(Span::styled(
                    "  loading…",
                    Style::default().fg(palette.muted),
                )))
                .alignment(ratatui::layout::Alignment::Center);
                frame.render_widget(p, area);
            }
        }
    }
}

/// カーソルが見える位置に scroll する単純な top 算出。
fn scroll_window_top(cursor: usize, visible: usize, total: usize) -> usize {
    if total <= visible || cursor < visible {
        return 0;
    }
    let max_top = total.saturating_sub(visible);
    cursor
        .saturating_sub(visible.saturating_sub(1))
        .min(max_top)
}

/// M13 PR5: `:` プロンプトを status バー位置に上書きする 1 行 overlay。
///
/// Issue #116: Tab 補完で候補が複数のときは status バー 1 行上にスペース区切りで
/// 候補を並べる (= popup 風だが Status の直上に重ねるだけの軽量表示)。
fn render_command_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    prompt: &crate::command::CommandPrompt,
) {
    let palette = &theme.palette;
    frame.render_widget(Clear, area);
    let line = Line::from(vec![
        Span::styled(
            "  : ",
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt.buffer.clone(),
            Style::default().fg(palette.foreground),
        ),
        Span::styled("▏", Style::default().fg(palette.accent)),
        Span::styled(
            "  Enter=run  Tab=complete  Esc=cancel",
            Style::default().fg(palette.muted),
        ),
    ]);
    let p = Paragraph::new(line).style(Style::default().bg(palette.background));
    frame.render_widget(p, area);

    // Issue #116: 補完候補が複数あれば status バーの直上 1 行に並べる。
    // status area の上端 (y) より上に行が無ければ (= 端末が極小) 諦める。
    if !prompt.suggestions.is_empty() && area.y > 0 {
        let suggest_area = Rect::new(area.x, area.y - 1, area.width, 1);
        frame.render_widget(Clear, suggest_area);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(prompt.suggestions.len() * 2 + 1);
        spans.push(Span::styled(
            "  ↳ ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
        for (i, s) in prompt.suggestions.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ", Style::default().fg(palette.muted)));
            }
            // `*s` は `&'static str` ── `Span::styled` は `Into<Cow<'static, str>>`
            // を受けるので `.to_string()` 不要 (= 毎フレーム描画でヒープ確保しない)。
            spans.push(Span::styled(*s, Style::default().fg(palette.foreground)));
        }
        let p = Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.background));
        frame.render_widget(p, suggest_area);
    }
}

fn compose_height(app: &App) -> u16 {
    // 投稿フォーカス中は 5 行、それ以外は 3 行 (header + 入力 1 行 + spacer)。
    if app.focus == Focus::Compose { 7 } else { 3 }
}

/// alt text プロンプトを status バーに上書きする 1 行 overlay。
fn render_alt_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    prompt: &crate::alt_prompt::AltPrompt,
) {
    let palette = &theme.palette;
    frame.render_widget(Clear, area);
    let line = Line::from(vec![
        Span::styled(
            format!("  alt for {} › ", prompt.label),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt.buffer.clone(),
            Style::default().fg(palette.foreground),
        ),
        Span::styled("▏", Style::default().fg(palette.accent)),
        Span::styled(
            "  Enter=upload  Esc=cancel",
            Style::default().fg(palette.muted),
        ),
    ]);
    let p = Paragraph::new(line).style(Style::default().bg(palette.background));
    frame.render_widget(p, area);
}

/// `Paragraph::wrap` 後にこの `Line` が消費する行数を `ratatui` の
/// `WordWrapper` と完全一致で算出する。
///
/// **Issue #104** で旧実装は各 `Line` を 1 行扱いし、`Issue #144` の修正で
/// `div_ceil(line.width(), viewport_width)` の近似に置き換えていたが、これは
/// ASCII の空白を挟む長文 (`HTTP GETにも署名 ...`) で `WordWrapper` が空白優
/// 先折返しを行うと近似値より 1 行多くなり、アバター位置が縦ズレする症状を
/// 生んだ。`ratatui` 0.30 の unstable feature `unstable-rendered-line-info`
/// (`Paragraph::line_count`) は実描画と同じ `WordWrapper` を回して行数を返す
/// ため、描画と高さ計算の食い違いをゼロにできる。
///
/// `viewport_width = 0` / 空 Line は安全に 1 を返す (= 行が消えないように)。
fn wrapped_line_height(line: &Line<'_>, viewport_width: u16) -> u16 {
    if viewport_width == 0 || line.width() == 0 {
        return 1;
    }
    let para = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
    let count = para.line_count(viewport_width);
    u16::try_from(count.max(1)).unwrap_or(u16::MAX)
}

fn render_timeline(frame: &mut Frame<'_>, area: Rect, app: &App) -> ScrollHits {
    let palette = &app.theme.palette;
    let block = Block::default()
        .title(Span::styled(
            format!("  home timeline ({} notes)  ", app.notes.len()),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::Timeline))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = ScrollHits::default();
    if app.notes.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "  (no notes yet — press 'n' to compose)",
                Style::default().fg(palette.muted),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  ?: help / q: quit",
                Style::default().fg(palette.muted),
            )),
        ];
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        return hits;
    }

    // M9 PR2: 画像取得経路 (= Picker + LocalApi) が揃っていて、かつ
    // 視覚刺激抑制で avatar が on のときだけ実描画。
    let avatar_enabled = app.images.enabled() && app.suppression.avatar;
    let header_indent = if avatar_enabled {
        AVATAR_CELLS_W + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
    let mut row_cursor: u16 = 0;
    let mut idx = app.top;
    // 描画後に上書きするアバター矩形をここに溜める。Paragraph 描画より後で
    // render_widget(Image, ...) で重ねる必要があるため。
    let mut avatar_overlays: Vec<(u16, &str)> = Vec::new();
    while idx < app.notes.len() && row_cursor < inner.height {
        let note = &app.notes[idx];
        let is_selected = idx == app.selected;
        let block_lines = note_lines(note, palette, is_selected, inner.width, header_indent);

        // **Issue #104 / #144**: 各 Line の **wrap 後の高さ** を計算して
        // row_cursor を進める。`note_lines` が組む Line は 1 行扱いだが、
        // Paragraph::wrap で折り返されると実際は複数行になる ── content が
        // 長い note の次行に次 note のアバターが乗ってしまうバグの原因。
        // ratatui の `Paragraph::line_count` で実描画と同じ WordWrapper を
        // 回すので、`Wrap { trim: false }` の空白優先折返しでも食い違わない。
        // 1 Line につき WordWrapper を 2 回走らせないよう先にまとめて算出する。
        let line_heights: Vec<u16> = block_lines
            .iter()
            .map(|l| wrapped_line_height(l, inner.width))
            .collect();
        let consumed_total: u16 = line_heights.iter().copied().fold(0u16, u16::saturating_add);
        let visible_top = inner.y + row_cursor;
        let visible_height = consumed_total.min(inner.height - row_cursor);
        hits.push(idx, visible_top, visible_height);

        if avatar_enabled
            && visible_height >= 1
            && let Some(url) = note.actor_icon_url.as_deref()
        {
            // renote エントリは先頭に「🔁 …」注記行があるので、アバターは
            // author 行 (= 注記行の次) に重ねる。注記行の高さぶん下げ、author
            // 行が viewport 外にクリップされる場合は描かない。
            let avatar_offset = if note.renote.is_some() {
                line_heights.first().copied().unwrap_or(0)
            } else {
                0
            };
            let avatar_top = visible_top.saturating_add(avatar_offset);
            if avatar_top < inner.y.saturating_add(inner.height) {
                avatar_overlays.push((avatar_top, url));
            }
        }

        for (l, h) in block_lines.into_iter().zip(line_heights) {
            if row_cursor >= inner.height {
                break;
            }
            lines.push(l);
            row_cursor = row_cursor.saturating_add(h);
        }
        idx += 1;
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);

    if avatar_enabled {
        for (y, url) in avatar_overlays {
            render_avatar(frame, app, inner.x, y, url);
        }
    }
    hits
}

/// M13 PR4: Profile 画面 (Timeline の代わりに `timeline_area` に描く)。
///
/// 上段: ヘッダ画像帯 (2 行、画像があれば 1 行使う) + プロフィール (アバター +
/// 名前 + acct + 状態 + bio + counts)。下段: notes 一覧。
///
/// 返り値は notes 一覧領域の矩形 (= `PanelRects::profile_notes` に保存)。
#[allow(clippy::too_many_lines, reason = "Profile 1 画面分の宣言的描画")]
fn render_profile_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    profile: &ProfileScreen,
) -> Rect {
    let palette = &app.theme.palette;
    let title_acct = profile.acct();
    let block = Block::default()
        .title(Span::styled(
            format!("  profile — {title_acct}  "),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::Profile))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // ヘッダ部の高さは bio の行数で可変。最低 4 行 (名前 / acct / 状態 / counts)、
    // bio で +N。残りを notes 一覧に渡す。
    // 連合先 (Mastodon 等) からの bio は `<p>...</p>` 等の HTML 形式で
    // 届くため、本文と同じく `to_plain_text` でプレーン化する。
    let summary_plain = profile
        .actor
        .summary
        .as_deref()
        .map(crate::content::to_plain_text);
    let summary_lines: Vec<String> = summary_plain
        .as_deref()
        .map(|s| s.lines().map(ToOwned::to_owned).collect())
        .unwrap_or_default();
    let summary_height = u16::try_from(summary_lines.len()).unwrap_or(0).min(6);
    let banner_height: u16 = u16::from(profile.actor.moved_to_ap_id.is_some());
    // 名前行 / acct + state 行 / counts 行 / 区切り行 + summary + moved banner
    let header_height = 4u16
        .saturating_add(summary_height)
        .saturating_add(banner_height);

    let header_rect = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        header_height.min(inner.height),
    );
    let notes_top = inner.y + header_rect.height;
    let notes_rect = Rect::new(
        inner.x,
        notes_top,
        inner.width,
        inner.height.saturating_sub(header_rect.height),
    );

    let avatar_enabled = app.images.enabled() && app.suppression.avatar;
    let avatar_indent = if avatar_enabled {
        AVATAR_CELLS_W + 1
    } else {
        0
    };
    let pad = " ".repeat(avatar_indent as usize);

    let mut header_lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(header_height));
    let display_name = profile.display_name();
    header_lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            display_name.clone(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let mut second = vec![
        Span::raw(pad.clone()),
        Span::styled(title_acct.clone(), Style::default().fg(palette.muted)),
    ];
    if profile.actor.manually_approves_followers {
        second.push(Span::raw("  "));
        second.push(Span::styled(
            "🔒 locked",
            Style::default().fg(palette.warning),
        ));
    }
    if !profile.actor.is_local {
        second.push(Span::raw("  "));
        second.push(Span::styled(
            format!("({})", profile.actor.actor_type),
            Style::default().fg(palette.muted),
        ));
    }
    header_lines.push(Line::from(second));

    let rel = &profile.relationship;
    let state_label = profile.relationship_label();
    let state_color = if rel.following {
        palette.success
    } else if matches!(rel.follow_state.as_deref(), Some("pending")) {
        palette.warning
    } else if matches!(rel.follow_state.as_deref(), Some("rejected")) {
        palette.error
    } else {
        palette.muted
    };
    let mut state_spans = vec![
        Span::raw(pad.clone()),
        Span::styled(
            format!("[{state_label}]"),
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if rel.followed_by {
        state_spans.push(Span::raw("  "));
        state_spans.push(Span::styled(
            "← follows you",
            Style::default().fg(palette.accent),
        ));
    }
    header_lines.push(Line::from(state_spans));

    if let Some(moved) = profile.actor.moved_to_ap_id.as_deref() {
        header_lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                "moved to ",
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(moved.to_string(), Style::default().fg(palette.foreground)),
        ]));
    }

    if !summary_lines.is_empty() {
        let body_indent_str = format!("{pad}  ");
        let body_width = inner.width.saturating_sub(avatar_indent + 2);
        for raw in summary_lines.iter().take(usize::from(summary_height)) {
            header_lines.push(Line::from(vec![
                Span::raw(body_indent_str.clone()),
                Span::styled(
                    truncate_for_width(raw, body_width),
                    Style::default().fg(palette.foreground),
                ),
            ]));
        }
    }

    // count 行 (notes 件数のみ取り回せるが、follow counts は AP collection
    // 解決が必要なので PR4 では省略)。
    header_lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            format!("recent notes: {}", profile.notes.len()),
            Style::default().fg(palette.muted),
        ),
    ]));

    let p = Paragraph::new(header_lines).wrap(Wrap { trim: false });
    frame.render_widget(p, header_rect);

    // アバター描画。ヘッダの最初の 2 行に被せる。
    if avatar_enabled && let Some(url) = profile.actor.icon_url.as_deref() {
        render_avatar(frame, app, inner.x, inner.y, url);
    }

    // notes 一覧。
    render_profile_notes(frame, notes_rect, profile, palette);
    notes_rect
}

/// Profile 画面の下半分: notes 一覧。Timeline の `note_lines` と同様だが
/// avatar 描画は省く (= author は profile ヘッダで既に明示されている)。
fn render_profile_notes(
    frame: &mut Frame<'_>,
    area: Rect,
    profile: &ProfileScreen,
    palette: &Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if profile.notes.is_empty() {
        let msg = if profile.notes_exhausted {
            "  (no visible notes)"
        } else {
            "  (loading notes…)"
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(p, area);
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    let mut row_cursor: u16 = 0;
    let mut idx = profile.note_top;
    while idx < profile.notes.len() && row_cursor < area.height {
        let note = &profile.notes[idx];
        let is_selected = idx == profile.selected_note;
        let block_lines = profile_note_lines(note, palette, is_selected, area.width);
        // **Issue #144 同種**: `profile_note_lines` が返す header (`[time]
        // (visibility)`) / CW / reaction 行は `truncate_for_width` を通って
        // いないので、`Wrap { trim: false }` の word boundary 折返しで実描画
        // 行数が >1 になる。`row_cursor += 1` の単純加算だと `selected_note`
        // が「実は見えていない」位置に乗ったり、loop が visual 領域を超えて
        // 余分な Line を push したりする ── `wrapped_line_height` で一致させる。
        let line_heights: Vec<u16> = block_lines
            .iter()
            .map(|l| wrapped_line_height(l, area.width))
            .collect();
        for (l, h) in block_lines.into_iter().zip(line_heights) {
            if row_cursor >= area.height {
                break;
            }
            lines.push(l);
            row_cursor = row_cursor.saturating_add(h);
        }
        idx += 1;
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Profile 内 notes 一覧の 1 件分。Timeline と異なり avatar indent は不要、
/// `[time] (visibility)` ヘッダ + 本文 1〜2 行 + リアクション行。
fn profile_note_lines(
    note: &TimelineNote,
    palette: &Palette,
    selected: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(4);
    let marker_style = if selected {
        Style::default()
            .fg(palette.accent_strong)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let marker = if selected { "▍ " } else { "  " };
    let local_published = note.published_at.with_timezone(&Local);
    let time = local_published.format("%m-%d %H:%M").to_string();
    let mut header_spans = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(format!("[{time}]"), Style::default().fg(palette.muted)),
        Span::raw("  "),
        Span::styled(
            format!("({})", note.visibility),
            Style::default().fg(palette.muted),
        ),
    ];
    if !note.attachments.is_empty() {
        // round-6 review F4: sensitive Note では件数を露出させない (= 添付の
        // 規模ヒントを隠す)。Profile も Timeline と同方針。
        let badge = if note.sensitive {
            "  📎".to_string()
        } else {
            format!("  📎 {}", note.attachments.len())
        };
        header_spans.push(Span::styled(badge, Style::default().fg(palette.muted)));
    }
    out.push(Line::from(header_spans));
    if let Some(cw) = &note.summary
        && !cw.is_empty()
    {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "CW: ",
                Style::default()
                    .fg(palette.cw_marker)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cw.clone(), Style::default().fg(palette.cw_marker)),
        ]));
    }
    let body_width = width.saturating_sub(2);
    // Timeline と同じく AP HTML をプレーン化してから折りたたむ。
    let body_text = crate::content::to_plain_text(&note.content);
    let max_body = if selected {
        FOCUSED_BODY_LINES
    } else {
        UNFOCUSED_BODY_LINES
    };
    append_folded_body(&mut out, &body_text, "  ", body_width, max_body, palette);
    if body_text.is_empty() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("(empty)", Style::default().fg(palette.muted)),
        ]));
    }
    if !note.reactions.is_empty() {
        out.push(reaction_line(note, palette, ""));
    }
    if note.announce_count > 0 {
        out.push(renote_line(note, palette, ""));
    }
    out.push(Line::from(""));
    out
}

/// M13 PR5: `FollowList` 画面 (= Timeline 領域に重ねる)。
///
/// 1 行目: タブ ([following] / [followers]) + 件数。
/// 2 行目以降: 各エントリ (アバター / display name / acct / state)。
/// Issue #115: 戻り値は `list_rect` ── main loop が次フレームの
/// [`crate::follow_list::FollowListScreen::ensure_visible`] /
/// [`crate::follow_list::FollowListScreen::select_page_down`] に
/// この高さを渡すために使う。非表示時は zero rect (= main loop で件数 0 扱い)。
#[allow(clippy::too_many_lines, reason = "FollowList 1 画面分の宣言的描画")]
fn render_follow_list_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    fl: &FollowListScreen,
) -> Rect {
    let palette = &app.theme.palette;
    let block = Block::default()
        .title(Span::styled(
            format!(
                "  follow list — {} ({})  ",
                fl.mode.label(),
                fl.current().entries.len()
            ),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::FollowList))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 1 行目: タブインジケータ。
    let header = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            tab_label(
                FollowListMode::Following,
                fl.mode == FollowListMode::Following,
            ),
            tab_style(palette, fl.mode == FollowListMode::Following),
        ),
        Span::raw("  "),
        Span::styled(
            tab_label(
                FollowListMode::Followers,
                fl.mode == FollowListMode::Followers,
            ),
            tab_style(palette, fl.mode == FollowListMode::Followers),
        ),
        Span::styled(
            "    [j/k/PgUp/PgDn=move  t=toggle  Enter=open  r=refresh  o=more  Esc=back]",
            Style::default().fg(palette.muted),
        ),
    ]);
    let header_rect = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    let p = Paragraph::new(vec![header]);
    frame.render_widget(p, header_rect);

    let list_top = inner.y + header_rect.height;
    let list_height = inner.height.saturating_sub(header_rect.height);
    let list_rect = Rect::new(inner.x, list_top, inner.width, list_height);
    if list_rect.height == 0 {
        return list_rect;
    }

    let page = fl.current();
    if page.entries.is_empty() {
        let msg = if page.fetched {
            format!("  (no {})", fl.mode.label())
        } else {
            "  loading…".to_string()
        };
        let para = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(para, list_rect);
        return list_rect;
    }

    let avatar_enabled = app.images.enabled() && app.suppression.avatar;
    let row_step: u16 = if avatar_enabled { 2 } else { 1 };
    let visible = (list_rect.height / row_step) as usize;
    let top = fl.top.min(page.entries.len().saturating_sub(1));
    // 描画: 各エントリ 2 行 (avatar 有) または 1 行 (avatar 無)。
    let mut avatar_overlays: Vec<(u16, &str)> = Vec::new();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible * row_step as usize);
    let mut row_cursor: u16 = 0;
    for (offset, entry) in page.entries.iter().enumerate().skip(top).take(visible) {
        if row_cursor + row_step > list_rect.height {
            break;
        }
        let is_selected = offset == fl.selected;
        let marker = if is_selected { "▍ " } else { "  " };
        let marker_style = if is_selected {
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };
        let display = entry
            .actor
            .display_name
            .clone()
            .unwrap_or_else(|| entry.actor.preferred_username.clone());
        let acct = format!("@{}@{}", entry.actor.preferred_username, entry.actor.host);
        let state_color = match entry.follow_state.as_str() {
            "accepted" => palette.success,
            "pending" => palette.warning,
            "rejected" => palette.error,
            _ => palette.muted,
        };
        let avatar_pad = if avatar_enabled {
            " ".repeat(usize::from(AVATAR_CELLS_W + 1))
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::raw(avatar_pad.clone()),
            Span::styled(marker.to_string(), marker_style),
            Span::styled(
                display,
                Style::default()
                    .fg(palette.foreground)
                    .add_modifier(if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::raw("  "),
            Span::styled(acct, Style::default().fg(palette.muted)),
            Span::raw("  "),
            Span::styled(
                format!("[{}]", entry.follow_state),
                Style::default().fg(state_color),
            ),
        ]));
        if row_step == 2 {
            lines.push(Line::from(""));
        }
        if avatar_enabled && let Some(url) = entry.actor.icon_url.as_deref() {
            let abs_y = list_rect.y + row_cursor;
            avatar_overlays.push((abs_y, url));
        }
        row_cursor += row_step;
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, list_rect);

    if avatar_enabled {
        for (y, url) in avatar_overlays {
            render_avatar(frame, app, list_rect.x, y, url);
        }
    }
    list_rect
}

/// M12 (#66): 鍵アカ承認待ち follow 一覧画面。
///
/// シンプルなテキスト一覧 ── 各行に `[N]` `follower_ap_id` `received_at`。
/// avatar overlay は不要 (= 承認可否判断に icon は要らない、`ap_id` で十分)。
///
/// 戻り値は `list_rect` ── main loop が次フレームの
/// [`crate::follow_requests::FollowRequestsScreen::ensure_visible`] にこの
/// 高さを渡すために使う。非表示時は zero rect。
fn render_follow_requests_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    fr: &crate::follow_requests::FollowRequestsScreen,
) -> Rect {
    let palette = &app.theme.palette;
    let block = Block::default()
        .title(Span::styled(
            format!("  follow requests — pending ({})  ", fr.items.len()),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::Requests))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = Line::from(vec![Span::styled(
        "  [j/k=move  a=approve  x=reject  r=refresh  Esc=back]",
        Style::default().fg(palette.muted),
    )]);
    let header_rect = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    frame.render_widget(Paragraph::new(vec![header]), header_rect);

    let list_top = inner.y + header_rect.height;
    let list_height = inner.height.saturating_sub(header_rect.height);
    let list_rect = Rect::new(inner.x, list_top, inner.width, list_height);
    if list_rect.height == 0 {
        return list_rect;
    }

    if fr.items.is_empty() {
        let msg = if fr.fetching {
            "  loading…"
        } else {
            "  (no pending follow requests)"
        };
        let para = Paragraph::new(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(para, list_rect);
        return list_rect;
    }

    // `fr.top` を尊重して offset 描画 ── `top` の追従は
    // [`crate::runtime::main_loop`] が
    // [`crate::follow_requests::FollowRequestsScreen::ensure_visible`] を
    // 毎フレーム呼ぶことで保証される (= timeline / follow_list と同パターン)。
    let visible = list_rect.height as usize;
    let top = fr.top.min(fr.items.len().saturating_sub(1));
    let lines: Vec<Line<'static>> = fr
        .items
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(idx, item)| {
            let selected = idx == fr.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let marker_style = if selected {
                Style::default()
                    .fg(palette.accent_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), marker_style),
                Span::styled(
                    format!("[{}] ", item.id),
                    Style::default().fg(palette.muted),
                ),
                Span::styled(
                    item.follower_ap_id.clone(),
                    Style::default().fg(palette.foreground),
                ),
                Span::raw("  "),
                Span::styled(item.received_at.clone(), Style::default().fg(palette.muted)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_rect);
    list_rect
}

/// リスト機能 (Mastodon/Misskey 互換) の画面。[`crate::lists::ListsScreen`]
/// の内部 state (`members` / `input`) に応じて 3 段を描き分ける ──
/// [`render_follow_requests_screen`] と同じ組み立て。
#[allow(
    clippy::too_many_lines,
    reason = "一覧 / メンバー一覧 / 入力 overlay の 3 段を 1 関数に集約"
)]
fn render_lists_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    ls: &crate::lists::ListsScreen,
) -> Rect {
    let palette = &app.theme.palette;
    let title = if let Some(members) = ls.members.as_ref() {
        format!(
            "  list members — {} ({})  ",
            members.title,
            members.items.len()
        )
    } else {
        format!("  lists ({})  ", ls.items.len())
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::Lists))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header_rect = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    if let Some(input) = ls.input.as_ref() {
        let label = match input.kind {
            crate::lists::ListsInputKind::Create => "new list title",
            crate::lists::ListsInputKind::Rename(_) => "rename to",
            crate::lists::ListsInputKind::AddMember(_) => "add member (acct)",
        };
        let line = Line::from(vec![
            Span::styled(format!("  {label}: "), Style::default().fg(palette.muted)),
            Span::styled(
                input.buffer.clone(),
                Style::default().fg(palette.foreground),
            ),
            Span::styled("_", Style::default().fg(palette.accent)),
        ]);
        frame.render_widget(Paragraph::new(line), header_rect);
    } else {
        let hint = if ls.members.is_some() {
            "  [j/k=move  a=add  x=remove  r=refresh  Esc=back]"
        } else {
            "  [j/k=move  Enter=switch  m=members  n=new  R=rename  d=delete  r=refresh  Esc=close]"
        };
        let header = Line::from(vec![Span::styled(hint, Style::default().fg(palette.muted))]);
        frame.render_widget(Paragraph::new(header), header_rect);
    }

    let list_top = inner.y + header_rect.height;
    let list_height = inner.height.saturating_sub(header_rect.height);
    let list_rect = Rect::new(inner.x, list_top, inner.width, list_height);
    if list_rect.height == 0 {
        return list_rect;
    }

    if let Some(members) = ls.members.as_ref() {
        if members.items.is_empty() {
            let para = Paragraph::new(Line::from(Span::styled(
                "  (no members — press a to add one)".to_string(),
                Style::default().fg(palette.muted),
            )));
            frame.render_widget(para, list_rect);
            return list_rect;
        }
        let visible = list_rect.height as usize;
        let top = members.top.min(members.items.len().saturating_sub(1));
        let lines: Vec<Line<'static>> = members
            .items
            .iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(idx, actor)| {
                let selected = idx == members.cursor;
                let marker = if selected { "▶ " } else { "  " };
                let marker_style = if selected {
                    Style::default()
                        .fg(palette.accent_strong)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.muted)
                };
                let acct = format!("{}@{}", actor.preferred_username, actor.host);
                Line::from(vec![
                    Span::styled(marker.to_string(), marker_style),
                    Span::styled(acct, Style::default().fg(palette.foreground)),
                    Span::raw("  "),
                    Span::styled(
                        actor.display_name.clone().unwrap_or_default(),
                        Style::default().fg(palette.muted),
                    ),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_rect);
        return list_rect;
    }

    if ls.items.is_empty() {
        let msg = if ls.fetching {
            "  loading…"
        } else {
            "  (no lists — press n to create one)"
        };
        let para = Paragraph::new(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(para, list_rect);
        return list_rect;
    }

    let visible = list_rect.height as usize;
    let top = ls.top.min(ls.items.len().saturating_sub(1));
    let lines: Vec<Line<'static>> = ls
        .items
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(idx, item)| {
            let selected = idx == ls.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let marker_style = if selected {
                Style::default()
                    .fg(palette.accent_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), marker_style),
                Span::styled(
                    format!("[{}] ", item.id),
                    Style::default().fg(palette.muted),
                ),
                Span::styled(item.title.clone(), Style::default().fg(palette.foreground)),
                Span::raw("  "),
                Span::styled(
                    format!("({} members)", item.member_count),
                    Style::default().fg(palette.muted),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_rect);
    list_rect
}

/// #206 PR3: 通知一覧画面。1 件 1 行 (= wrap しない、`ensure_visible` の viewport
/// = 行数前提)。各行は「カーソル ▶ / 未読 ● / 種別 glyph / notifier / 動詞 /
/// reaction / 本文プレビュー」。色は全て theme palette 経由。
///
/// 戻り値は `list_rect` ── main loop が次フレームの
/// [`crate::notifications::NotificationsScreen::ensure_visible`] にこの高さを渡す。
fn render_notifications_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    n: &crate::notifications::NotificationsScreen,
) -> Rect {
    let palette = &app.theme.palette;
    let title = if n.unread_count > 0 {
        format!(
            "  notifications — {} unread / {} total  ",
            n.unread_count,
            n.items.len()
        )
    } else {
        format!("  notifications — {} total  ", n.items.len())
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(border_style(palette, app.focus == Focus::Notifications))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = Line::from(vec![Span::styled(
        "  [j/k=move  m=mark all read  r=refresh  Esc=back]",
        Style::default().fg(palette.muted),
    )]);
    let header_rect = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    frame.render_widget(Paragraph::new(vec![header]), header_rect);

    let list_top = inner.y + header_rect.height;
    let list_height = inner.height.saturating_sub(header_rect.height);
    let list_rect = Rect::new(inner.x, list_top, inner.width, list_height);
    if list_rect.height == 0 {
        return list_rect;
    }

    if n.items.is_empty() {
        let msg = if n.fetching {
            "  loading…"
        } else {
            "  (no notifications)"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg.to_string(),
                Style::default().fg(palette.muted),
            ))),
            list_rect,
        );
        return list_rect;
    }

    let visible = list_rect.height as usize;
    let top = n.top.min(n.items.len().saturating_sub(1));
    let lines: Vec<Line<'static>> = n
        .items
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(idx, item)| notification_line(item, idx == n.cursor, palette))
        .collect();
    frame.render_widget(Paragraph::new(lines), list_rect);
    list_rect
}

/// 通知 1 件を 1 行に組む (= `render_notifications_screen` のヘルパ)。
fn notification_line(
    item: &crate::client::NotificationItem,
    selected: bool,
    palette: &crate::theme::Palette,
) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let marker_style = if selected {
        Style::default()
            .fg(palette.accent_strong)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    // 未読は ● (accent)、既読は空白。
    let unread = if item.is_read { "  " } else { "● " };
    let (glyph, verb) = crate::notifications::event_glyph_label(&item.event_type);
    let who = item
        .notifier_display_name
        .clone()
        .or_else(|| item.notifier_acct.clone())
        .unwrap_or_else(|| "someone".to_string());
    let mut spans = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(
            unread.to_string(),
            Style::default().fg(palette.accent_strong),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(palette.accent)),
        Span::styled(
            who,
            Style::default()
                .fg(palette.foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {verb}"), Style::default().fg(palette.muted)),
    ];
    if let Some(reaction) = &item.reaction {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            reaction.clone(),
            Style::default().fg(palette.warning),
        ));
    }
    if let Some(preview) = &item.note_preview {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("“{preview}”"),
            Style::default().fg(palette.muted),
        ));
    }
    Line::from(spans)
}

fn tab_label(mode: FollowListMode, active: bool) -> String {
    if active {
        format!("[{}]", mode.label())
    } else {
        format!(" {} ", mode.label())
    }
}

fn tab_style(palette: &Palette, active: bool) -> Style {
    if active {
        Style::default()
            .fg(palette.accent_strong)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    }
}

fn render_avatar(frame: &mut Frame<'_>, app: &App, x: u16, y: u16, url: &str) {
    let rect = Rect::new(x, y, AVATAR_CELLS_W, AVATAR_CELLS_H);
    // 未取得ならフェッチを spawn (= 次フレームには Ready になる可能性がある)。
    app.images.ensure(url, rect);
    let Some(protocol) = app.images.get(url) else {
        // 取得待ち / 失敗のときはプレースホルダ。視覚刺激抑制との互換性も保てる。
        let p = Paragraph::new(Line::from(Span::styled(
            "▒▒",
            Style::default().fg(app.theme.palette.muted),
        )));
        frame.render_widget(p, rect);
        return;
    };
    let widget = Image::new(protocol.as_ref());
    frame.render_widget(widget, rect);
}

// header / CW / body / reactions / renote 各行を 1 関数で組み立てる都合上 100 行を
// 超える。各ブロックは独立しており分割しても可読性が上がらないため allow する。
#[allow(
    clippy::too_many_lines,
    reason = "1 note の各表示ブロックを順に積むため。分割しても読みやすくならない"
)]
fn note_lines(
    note: &TimelineNote,
    palette: &Palette,
    selected: bool,
    width: u16,
    avatar_indent: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(4);
    let marker_style = if selected {
        Style::default()
            .fg(palette.accent_strong)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let marker = if selected { "▍ " } else { "  " };
    let pad = " ".repeat(avatar_indent as usize);

    // renote として流れてきたエントリは先頭に「🔁 <renoter> がリノート」注記を
    // 出す。本体フィールド (author / content / reactions …) は **元 note** なので、
    // 続く通常描画がそのまま元 note を表す。選択マーカーは下の author 行に残す。
    if let Some(r) = &note.renote {
        let who = r
            .renoter_display_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(r.renoter_preferred_username.as_str());
        out.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::raw("  "),
            Span::styled(
                format!("🔁 {who} がリノート"),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    let local_published = note.published_at.with_timezone(&Local);
    let time = local_published.format("%H:%M").to_string();
    let handle = format_handle(note);
    let mut header_spans = vec![
        Span::raw(pad.clone()),
        Span::styled(marker.to_string(), marker_style),
        Span::styled(format!("[{time}] "), Style::default().fg(palette.muted)),
        Span::styled(
            handle,
            Style::default()
                .fg(if note.is_local {
                    palette.accent
                } else {
                    palette.foreground
                })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(
            format!("  ({})", note.visibility),
            Style::default().fg(palette.muted),
        ),
    ];
    if !note.attachments.is_empty() {
        // 添付があれば badge を visibility の隣に出す。Timeline では
        // 添付実体は出さず、Enter で詳細モーダルに遷移してプレビューする運用。
        // round-6 review F4: sensitive Note では件数を露出させない (= 添付の
        // 規模ヒントを隠す)。アイコンだけ出して「何かある」とだけ示し、
        // 詳細は reveal してから見せる。
        let badge = if note.sensitive {
            "  📎".to_string()
        } else {
            format!("  📎 {}", note.attachments.len())
        };
        header_spans.push(Span::styled(badge, Style::default().fg(palette.muted)));
    }
    out.push(Line::from(header_spans));

    if let Some(cw) = &note.summary
        && !cw.is_empty()
    {
        out.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                "  CW: ",
                Style::default()
                    .fg(palette.cw_marker)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cw.clone(), Style::default().fg(palette.cw_marker)),
        ]));
    }

    let total_indent = avatar_indent + 2;
    let body_width = width.saturating_sub(total_indent);
    // AP `Note.content` は HTML (`<p>`, `<br>`, `<a>`) 形式で配信されるため
    // TUI 描画前にプレーン化する。DB / 配送 / permalink に保存する文字列は
    // 連合互換のため触らない (`content` フィールドは読み取りのみ)。
    let body_text = crate::content::to_plain_text(&note.content);
    // 長文 note は Timeline で折りたたむ。フォーカスが当たっているときは
    // [`FOCUSED_BODY_LINES`] 行、非フォーカスは [`UNFOCUSED_BODY_LINES`]
    // 行までに切る。超過分は最終行末に ` [+N 行]` の indicator を muted 色
    // で添える ── 詳細閲覧は別途モーダルに任せる。
    let max_body = if selected {
        FOCUSED_BODY_LINES
    } else {
        UNFOCUSED_BODY_LINES
    };
    append_folded_body(
        &mut out,
        &body_text,
        &format!("{pad}  "),
        body_width,
        max_body,
        palette,
    );
    if body_text.is_empty() {
        out.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("  (empty)", Style::default().fg(palette.muted)),
        ]));
    }
    if !note.reactions.is_empty() {
        out.push(reaction_line(note, palette, &pad));
    }
    if note.announce_count > 0 {
        out.push(renote_line(note, palette, &pad));
    }
    out.push(Line::from(""));
    out
}

/// 1 件の Note のリアクション集計を 1 行にまとめる。
///
/// 表示例: ` :blob_party: 3   👍 1   :tada@misskey.io: 2 `。content 文字列は
/// AP からそのまま (= shortcode 形式は `:foo:`、Unicode はそのまま)。
/// アイコンのインライン画像描画は M? で別途 (現状は shortcode テキストのみ)。
fn reaction_line(note: &TimelineNote, palette: &Palette, pad: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        Vec::with_capacity(note.reactions.len().saturating_mul(2).saturating_add(1));
    spans.push(Span::raw(format!("{pad}  ")));
    for (i, r) in note.reactions.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().fg(palette.muted)));
        }
        let label = reaction_label(&r.content);
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" ×{}", r.count),
            Style::default().fg(palette.muted),
        ));
    }
    Line::from(spans)
}

/// #151: 1 件の Note の renote (Announce) 集計を 1 行にまとめる。
///
/// 表示例: ` ↻ 3 (you renoted) `。`viewer_renoted` が true のとき accent 色 +
/// BOLD で強調 (= 自分が boost 済みであることを一目で示す)。
/// `announce_count == 0` のときは行を作らず、呼び出し側で push を抑制する。
fn renote_line(note: &TimelineNote, palette: &Palette, pad: &str) -> Line<'static> {
    let style = if note.viewer_renoted {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let mut spans = Vec::with_capacity(3);
    spans.push(Span::raw(format!("{pad}  ")));
    spans.push(Span::styled(format!("↻ {}", note.announce_count), style));
    if note.viewer_renoted {
        spans.push(Span::styled(
            "  (you renoted)",
            Style::default().fg(palette.muted),
        ));
    }
    Line::from(spans)
}

/// 表示用ラベル: `:foo@host:` → `:foo:` に短縮、Unicode はそのまま。
fn reaction_label(content: &str) -> String {
    if let Some(stripped) = content.strip_prefix(':').and_then(|s| s.strip_suffix(':'))
        && let Some(name) = stripped.split('@').next()
        && !name.is_empty()
    {
        return format!(":{name}:");
    }
    content.to_string()
}

fn format_handle(note: &TimelineNote) -> String {
    let display = note
        .actor_display_name
        .clone()
        .unwrap_or_else(|| note.actor_preferred_username.clone());
    let host = extract_host(&note.actor_ap_id).unwrap_or_default();
    if note.is_local {
        format!("@{}", note.actor_preferred_username)
    } else {
        format!("{display} (@{}@{host})", note.actor_preferred_username)
    }
}

fn extract_host(ap_id: &str) -> Option<String> {
    url::Url::parse(ap_id)
        .ok()
        .and_then(|u| u.host_str().map(ToOwned::to_owned))
}

/// 本文行を `max_body` 行に折りたたみ、`out` に push する。
///
/// `body_text` を `.lines()` で分解し、`max_body` 行までを `prefix` 付きで
/// 行として積む。超過がある場合は最後の可視行末に ` [+N 行]` の indicator
/// を muted 色で追加する。`max_body == 0` は「折りたたみ無し」(= 全行)。
///
/// `prefix` は各行先頭に置く文字列 (Timeline は `"  "` + アバター indent、
/// Profile は `"  "` のみ) で、ヘッダ / CW 行と桁を揃える役目。
fn append_folded_body(
    out: &mut Vec<Line<'static>>,
    body_text: &str,
    prefix: &str,
    body_width: u16,
    max_body: usize,
    palette: &Palette,
) {
    let all: Vec<&str> = body_text.lines().collect();
    if all.is_empty() {
        return;
    }
    let (visible, truncated): (&[&str], usize) = if max_body == 0 || all.len() <= max_body {
        (&all[..], 0)
    } else {
        (&all[..max_body], all.len() - max_body)
    };
    let last_idx = visible.len().saturating_sub(1);
    for (i, body_line) in visible.iter().enumerate() {
        if truncated > 0 && i == last_idx {
            let indicator = format!(" [+{truncated} 行]");
            // 行末 indicator は `truncate_for_width` の対象外にしたいので、本文側を
            // 先に狭めて切る。indicator の文字幅 (= char count 近似) を引いた残りで
            // 本文を truncate する。`行` は East Asian Wide で実セル幅 2 だが、
            // `truncate_for_width` 自身も 1 char = 1 cell の素朴近似を採用しており
            // (= `unicode-width` 未導入)、本実装もそれに揃えてある。揃って改修する
            // 時は両方を同時に切り替える。
            let indicator_width = u16::try_from(indicator.chars().count()).unwrap_or(u16::MAX);
            let line_width = body_width.saturating_sub(indicator_width);
            out.push(Line::from(vec![
                Span::raw(prefix.to_string()),
                Span::styled(
                    truncate_for_width(body_line, line_width),
                    Style::default().fg(palette.foreground),
                ),
                Span::styled(indicator, Style::default().fg(palette.muted)),
            ]));
        } else {
            out.push(Line::from(vec![
                Span::raw(prefix.to_string()),
                Span::styled(
                    truncate_for_width(body_line, body_width),
                    Style::default().fg(palette.foreground),
                ),
            ]));
        }
    }
}

fn truncate_for_width(s: &str, max: u16) -> String {
    // 1 char = 1 cell の素朴近似 (East Asian Wide は将来 unicode-width で改善)。
    // 切り捨てが起きるケースでは `…` を末尾に必ず置くため、収まる char 数を
    // max-1 で予約する。max == 0 は空文字。
    if max == 0 {
        return String::new();
    }
    let max_usize = usize::from(max);
    let total = s.chars().count();
    if total <= max_usize {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_usize - 1).collect();
    out.push('…');
    out
}

/// Issue #133 (3) (4) (5): Note 詳細モーダル。Timeline 上に中央 overlay で
/// 出して `Esc` で閉じる。本文は折りたたまず (= `max_body=0`) に全文を出す。
///
/// レイアウト (上から):
///   - title bar (Note id + handle + visibility)
///   - permalink (1 行、`url` があれば)
///   - CW (full)
///   - 本文 (HTML strip 済み + 折りたたみ無し)
///   - リアクション行 (既存 [`reaction_line`] を再利用)
///   - 添付プレビュー (画像 1 枚 + メタ情報、sensitive blur 対応)
///   - 絵文字ギャラリー (`:shortcode:` + 画像、suppression.emoji が on のとき)
///   - footer (key bindings ヒント)
#[allow(clippy::too_many_lines, reason = "1 画面分の宣言的描画")]
fn render_note_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &crate::note_detail::NoteDetailScreen,
) {
    let palette = &app.theme.palette;
    fill_modal_backdrop(frame, area, palette);
    let note = &state.note;

    // 全画面の 80% を modal 領域に使う。
    let modal_w = area.width.saturating_sub(4).max(50);
    let modal_h = area.height.saturating_sub(2).max(15);
    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let rect = Rect::new(modal_x, modal_y, modal_w, modal_h);

    let handle = format_handle(note);
    let title = format!("  note #{} — {} ({})  ", note.id, handle, note.visibility);
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .padding(Padding::new(1, 1, 0, 0))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // 縦 split: 本文 (= scroll 対象) / 添付プレビュー / footer。
    let footer_height: u16 = 1;
    let preview_height: u16 = if note.attachments.is_empty() {
        0
    } else {
        inner.height.clamp(6, 10)
    };
    let body_height = inner.height.saturating_sub(footer_height + preview_height);

    let body_rect = Rect::new(inner.x, inner.y, inner.width, body_height);
    let preview_rect = Rect::new(inner.x, inner.y + body_height, inner.width, preview_height);
    let footer_rect = Rect::new(
        inner.x,
        inner.y + body_height + preview_height,
        inner.width,
        footer_height,
    );

    // 本文ブロック (= 折りたたみ無しの完全版)。
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(16);
    let local_published = note.published_at.with_timezone(&Local);
    lines.push(Line::from(vec![Span::styled(
        format!("[{}]", local_published.format("%Y-%m-%d %H:%M")),
        Style::default().fg(palette.muted),
    )]));
    if let Some(u) = note.url.as_deref() {
        lines.push(Line::from(vec![Span::styled(
            format!("↳ {u}"),
            Style::default().fg(palette.muted),
        )]));
    }
    if let Some(cw) = note.summary.as_deref()
        && !cw.is_empty()
    {
        lines.push(Line::from(vec![
            Span::styled(
                "CW: ",
                Style::default()
                    .fg(palette.cw_marker)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(cw.to_string(), Style::default().fg(palette.cw_marker)),
        ]));
        lines.push(Line::from(""));
    }
    let body_text = crate::content::to_plain_text(&note.content);
    if body_text.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty)",
            Style::default().fg(palette.muted),
        )));
    } else {
        // `max_body = 0` で折りたたみ無し ── `append_folded_body` のヘルパに
        // 任せて、Timeline と同じ width / palette 経路で描く。
        append_folded_body(&mut lines, &body_text, "", body_rect.width, 0, palette);
    }
    if !note.reactions.is_empty() {
        lines.push(Line::from(""));
        lines.push(reaction_line(note, palette, ""));
    }
    if note.announce_count > 0 {
        lines.push(Line::from(""));
        lines.push(renote_line(note, palette, ""));
    }
    // round-4 review Finding 1: 視覚刺激抑制 `suppression.emoji` が off の
    // ときは emoji セクションを丸ごとスキップする (= CLAUDE.md §5.2
    // 「カスタム絵文字表示 on/off」要件)。`emoji_gallery_line` の
    // docstring も「on のとき」と書いているが実装側で逃げていなかった。
    if !note.emojis.is_empty() && app.suppression.emoji {
        lines.push(Line::from(""));
        lines.push(emoji_gallery_line(note, palette));
    }
    if !note.attachments.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("📎 attachments ({}):", note.attachments.len()),
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::BOLD),
        )]));
        for (i, att) in note.attachments.iter().enumerate() {
            let cursor = if i == state.selected_attachment {
                "▶ "
            } else {
                "  "
            };
            // round-6 review F3: blur 中 (= 個別 `revealed[i] == false`) の
            // 添付はメタ情報 (mediaType / 寸法 / alt) も隠す。alt を見せると
            // sensitive コンテンツのヒントになりうるため fail-closed で
            // プレースホルダだけ出す。
            let revealed = state.revealed.get(i).copied().unwrap_or(false);
            let label = if revealed {
                let mt = att.media_type.as_deref().unwrap_or("?");
                let dims = match (att.width, att.height) {
                    (Some(w), Some(h)) => format!(" {w}×{h}"),
                    _ => String::new(),
                };
                let alt = att.alt.as_deref().unwrap_or("");
                let alt_part = if alt.is_empty() {
                    String::new()
                } else {
                    format!(" — {alt}")
                };
                format!(
                    "{cursor}[{}/{}] {mt}{dims}{alt_part}",
                    i + 1,
                    note.attachments.len()
                )
            } else {
                format!(
                    "{cursor}[{}/{}] [hidden — press s to reveal]",
                    i + 1,
                    note.attachments.len()
                )
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default().fg(if i == state.selected_attachment {
                    palette.accent
                } else {
                    palette.foreground
                }),
            )]));
        }
    }

    // スクロール: state.scroll 行分先頭をスキップする。
    let skipped: Vec<Line<'static>> = lines.into_iter().skip(state.scroll).collect();
    let p = Paragraph::new(skipped).wrap(Wrap { trim: false });
    frame.render_widget(p, body_rect);

    // 添付プレビュー (画像)。
    if preview_height > 0 {
        render_note_detail_preview(frame, preview_rect, app, state, palette);
    }

    // footer のキー bindings ヒント。
    let footer = if note.attachments.is_empty() {
        " Esc/q close · j/k scroll".to_string()
    } else {
        format!(
            " Esc/q close · j/k scroll · n/p attach ({}/{}) · s reveal",
            state.selected_attachment + 1,
            note.attachments.len()
        )
    };
    let f = Paragraph::new(Line::from(Span::styled(
        footer,
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(f, footer_rect);
}

/// Issue #133 (4): 詳細モーダル下半分の添付プレビュー。`sensitive` Note の
/// 場合は初期 blur、`s` で個別 reveal (= [`NoteDetailScreen::toggle_reveal`])。
/// 画像取得は既存 [`ImageCache`] (= `media-proxy` 経由) を流用。
fn render_note_detail_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &crate::note_detail::NoteDetailScreen,
    palette: &Palette,
) {
    let Some(att) = state.note.attachments.get(state.selected_attachment) else {
        return;
    };
    let is_image = att
        .media_type
        .as_deref()
        .is_some_and(|m| m.starts_with("image/"));
    // revealed vec が attachments.len() で初期化されるので index 範囲外は
    // 通常起きないが、防御的に **fail-closed** = 範囲外なら blur 扱い。
    // `unwrap_or(true)` だと万一 sensitive Note の添付が無音で見えてしまう。
    let revealed = state
        .revealed
        .get(state.selected_attachment)
        .copied()
        .unwrap_or(false);
    // round-3 review Finding 1: **画像か否かに関わらず** blur チェックを先に
    // 行う ── 非画像 (動画 / 音声) の URL も sensitive Note では `s` で
    // 解除するまで隠す。`!is_image` を先に置くと、非画像添付の URL が常時
    // 平文表示されて sensitive 保護をバイパスする。
    if !revealed {
        // round-6 review F8: 非 sensitive Note でも `s` で再ブラー可能なので
        // メッセージは Note の `sensitive` フラグを参照して切り替える ──
        // sensitive=false で「sensitive」と出すのは誤誘導。
        let msg = if state.note.sensitive {
            "  [sensitive — press s to reveal]"
        } else {
            "  [hidden — press s to reveal]"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ))),
            area,
        );
        return;
    }
    if !is_image {
        let msg = format!("  [non-image attachment: {}]", att.url);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(palette.muted),
            ))),
            area,
        );
        return;
    }
    // 視覚刺激抑制 (attachment トグル) が off なら画像描画を抜く。
    if !app.suppression.attachment || !app.images.enabled() {
        let msg = "  [preview disabled — toggle in suppression overlay]";
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(palette.muted),
            ))),
            area,
        );
        return;
    }
    // 添付プレビューは `preview` variant (1280×1280) を要求 ── アバター用の
    // 256×256 では Note 詳細モーダルで粗くなる。
    app.images
        .ensure_with_variant(&att.url, area, crate::image_cache::VARIANT_PREVIEW);
    if let Some(proto) = app.images.get(&att.url) {
        let widget = Image::new(proto.as_ref());
        frame.render_widget(widget, area);
    } else {
        let msg = "  loading…";
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(palette.muted),
            ))),
            area,
        );
    }
}

/// Issue #133 (5): 詳細モーダル本文に挟む emoji ギャラリー行。本文中の
/// `:shortcode:` に対応する custom emoji を `:foo:` の形で一覧表示する。
///
/// MVP: ratatui のテキスト Span に画像を埋め込めないため、まずは shortcode
/// テキストだけを accent 色で並べる ── 将来的に行下に小さい画像ストリップ
/// を `ratatui-image` で描く方針 (= reaction 画像と同じパターン)。
fn emoji_gallery_line(note: &TimelineNote, palette: &Palette) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(note.emojis.len() * 2 + 1);
    spans.push(Span::styled(
        format!("🌸 emojis ({}): ", note.emojis.len()),
        Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::BOLD),
    ));
    for (i, e) in note.emojis.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            e.shortcode.clone(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

#[allow(clippy::too_many_lines)]
fn render_compose(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let palette = &app.theme.palette;
    let header = Line::from(vec![
        Span::styled(
            "  compose  ",
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{}]", app.compose.visibility().label()),
            Style::default().fg(palette.accent),
        ),
        Span::raw("  "),
        Span::styled(
            if app.compose.cw().is_empty() {
                "CW: -".into()
            } else {
                format!("CW: {}", app.compose.cw())
            },
            Style::default().fg(palette.cw_marker),
        ),
        Span::raw("  "),
        Span::styled(
            if app.compose.sensitive() {
                "SENS: on"
            } else {
                "SENS: off"
            },
            Style::default().fg(if app.compose.sensitive() {
                palette.warning
            } else {
                palette.muted
            }),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} left", app.compose.remaining()),
            Style::default().fg(if app.compose.remaining() < 0 {
                palette.error
            } else {
                palette.muted
            }),
        ),
    ]);

    let block = Block::default()
        .title(header)
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(border_style(palette, app.focus == Focus::Compose))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // M13 PR6: 返信モードは親 note のラベルを 1 行目に固定表示する。
    if let Some(label) = app.compose.reply_parent_label() {
        lines.push(Line::from(Span::styled(
            format!("↳ {label}"),
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    if app.compose.editing_cw() {
        // CW 行にフォーカス表示。
        let cw_line = if app.compose.cw().is_empty() {
            "CW> ".to_string()
        } else {
            format!("CW> {}", app.compose.cw())
        };
        lines.push(Line::from(Span::styled(
            cw_line,
            Style::default().fg(palette.cw_marker),
        )));
    }
    for body in app.compose.lines() {
        lines.push(Line::from(Span::styled(
            body.to_string(),
            Style::default().fg(palette.foreground),
        )));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty — type to write, Ctrl-Enter to send, Esc to leave)",
            Style::default().fg(palette.muted),
        )));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);

    // カーソル位置 (本文行のみ)。CW 編集中は CW 行の末尾に出す。
    if app.focus == Focus::Compose {
        use unicode_width::UnicodeWidthStr;

        // M13 PR6: 返信ラベル行 (= 1 行先頭固定) があれば本文行を 1 行ずらす。
        let reply_offset = usize::from(app.compose.reply_parent_label().is_some());
        let (row, col) = if app.compose.editing_cw() {
            // **Issue #105**: char 数ではなく display width で測る ── 日本語の
            // CW にもカーソルが揃うように。
            (
                reply_offset,
                UnicodeWidthStr::width("CW> ") + UnicodeWidthStr::width(app.compose.cw()),
            )
        } else {
            let (r, c) = app.compose.cursor_row_col();
            // CW 行が乗っている場合は +1。
            let cw_offset = usize::from(app.compose.editing_cw());
            (r + reply_offset + cw_offset, c)
        };
        let cx = inner.x + u16::try_from(col).unwrap_or(0);
        let cy = inner.y + u16::try_from(row).unwrap_or(0);
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            frame.set_cursor_position((cx, cy));
        }
    }
}

/// Issue #131: status bar 左端の spinner 文字を決める。
///
/// `animation_on = false` (= suppression.animation off) のときは ● 静止。
/// それ以外は Braille 10-frame を `SystemTime` 経由で進める ── state を
/// 持たないので `tick` カウンタや App フィールド追加が不要。位相は
/// 100 ms ごとに進み、tick 周期 (= 250ms) と整合する。
#[must_use]
fn spinner_char_for_now(animation_on: bool) -> char {
    spinner_char_at_ms(animation_on, current_ms_since_epoch())
}

#[must_use]
fn current_ms_since_epoch() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Braille spinner の 10 frame。100 ms / frame で進める。
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// 単体テスト可能な phase 計算本体 (= `SystemTime` を引数化)。
#[must_use]
fn spinner_char_at_ms(animation_on: bool, ms_since_epoch: u128) -> char {
    if !animation_on {
        return '●';
    }
    let idx = ((ms_since_epoch / 100) as usize) % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let palette = &app.theme.palette;
    let focus_label = match app.focus {
        Focus::Timeline => "timeline",
        Focus::Compose => "compose",
        Focus::Help => "help",
        Focus::Picker => "picker",
        Focus::Suppression => "suppress",
        Focus::AltPrompt => "alt",
        Focus::Profile => "profile",
        Focus::FollowList => "follow-list",
        Focus::Command => "cmd",
        Focus::Requests => "requests",
        Focus::Notifications => "notif",
        Focus::EmojiSearch => "emoji",
        Focus::NoteDetail => "note",
        Focus::Lists => "lists",
    };
    // Issue #131: in-flight な async 操作があれば左端 3 cells に spinner を
    // 出す。0 件のときも 3 cells 確保して後続 span の位置を揺らさない。
    let in_flight = app.in_flight_count();
    let spinner_span: Span<'static> = if in_flight > 0 {
        let ch = spinner_char_for_now(app.suppression.animation);
        Span::styled(
            format!(" {ch} "),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("   ")
    };
    let mut spans: Vec<Span<'static>> = vec![
        spinner_span,
        Span::styled(
            format!("@{}", app.whoami.preferred_username),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(app.socket_label.clone(), Style::default().fg(palette.muted)),
        Span::raw("  "),
        Span::styled(
            format!("[{focus_label}]"),
            Style::default().fg(palette.accent_strong),
        ),
        Span::raw("  "),
        Span::styled(
            format!("theme:{}", app.theme.name),
            Style::default().fg(palette.muted),
        ),
    ];
    // リスト表示中はどのリストを見ているか一目でわかるようバッジを出す
    // (= home のときは何も出さない、既定状態を煩雑にしない)。
    if let crate::app::TimelineSource::List { title, .. } = &app.current_timeline {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("tl:{title}"),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.pending_uploads > 0 {
        spans.push(Span::raw("  │  "));
        spans.push(Span::styled(
            format!("↑{}", app.pending_uploads),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let att_count = app.compose.attachments().len();
    if att_count > 0 {
        spans.push(Span::raw("  │  "));
        spans.push(Span::styled(
            format!("attach:{att_count}"),
            Style::default().fg(palette.accent),
        ));
    }
    if let Some(s) = &app.status {
        let color = match s.kind {
            StatusKind::Info => palette.foreground,
            StatusKind::Success => palette.success,
            StatusKind::Warning => palette.warning,
            StatusKind::Error => palette.error,
        };
        spans.push(Span::raw("  │  "));
        spans.push(Span::styled(s.text.clone(), Style::default().fg(color)));
    }

    let line = Line::from(spans).style(
        Style::default()
            .bg(palette.status_bar_bg)
            .fg(palette.status_bar_fg),
    );
    let p = Paragraph::new(line).style(Style::default().bg(palette.status_bar_bg));
    frame.render_widget(p, area);
}

#[allow(
    clippy::many_single_char_names,
    reason = "矩形 w/h/x/y は ratatui 慣習"
)]
#[allow(clippy::too_many_lines, reason = "help は宣言的でひと固まり")]
fn render_help(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Rect {
    let palette = &app.theme.palette;
    fill_modal_backdrop(frame, area, palette);
    // 中央に max(60, area.width * 0.6) x min(20, area.height - 4) を浮かべる。
    let w = area.width.clamp(40, 60);
    let h = area.height.saturating_sub(6).clamp(10, 18);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    let lines = vec![
        Line::from(Span::styled("timeline", help_section(palette))),
        help_entry(palette, "j / ↓", "select next"),
        help_entry(palette, "k / ↑", "select previous"),
        help_entry(palette, "space / PgDn", "page down"),
        help_entry(palette, "PgUp", "page up"),
        help_entry(palette, "n", "compose new note"),
        help_entry(palette, "r", "refresh timeline"),
        help_entry(palette, "o", "load more (older)"),
        help_entry(palette, "t", "cycle theme"),
        help_entry(palette, "q / Esc", "quit"),
        help_entry(palette, "A", "upload avatar"),
        help_entry(palette, "H", "upload header"),
        help_entry(palette, ";", "attach image (picker)"),
        help_entry(palette, "e", "react: open emoji search modal"),
        help_entry(palette, "i", "image suppression toggle"),
        help_entry(palette, "p", "open profile of author"),
        help_entry(palette, ":", "command prompt"),
        help_entry(palette, "Ctrl-L", "force full redraw (fix image glitches)"),
        Line::from(""),
        Line::from(Span::styled("compose", help_section(palette))),
        help_entry(palette, "Enter", "insert newline"),
        help_entry(palette, "Ctrl-Enter", "send note"),
        help_entry(palette, "Ctrl-W", "toggle CW field"),
        help_entry(palette, "Ctrl-V", "cycle visibility"),
        help_entry(palette, "Ctrl-S", "toggle sensitive"),
        help_entry(palette, "Ctrl-A", "attach image (picker)"),
        help_entry(palette, "Ctrl-D", "detach last attachment"),
        help_entry(palette, "Esc", "leave compose"),
        Line::from(""),
        Line::from(Span::styled("file picker", help_section(palette))),
        help_entry(palette, "j / k", "select next / prev"),
        help_entry(palette, "Enter", "descend / select file"),
        help_entry(palette, "Backspace", "go to parent"),
        help_entry(palette, ".", "toggle hidden files"),
        help_entry(palette, "/", "type a path directly"),
        help_entry(palette, "Tab (in path input)", "complete path"),
        help_entry(palette, "Esc / q", "cancel picker"),
        Line::from(""),
        Line::from(Span::styled("emoji search modal", help_section(palette))),
        help_entry(
            palette,
            "e (timeline)",
            "open modal → Enter で即リアクション送信",
        ),
        help_entry(
            palette,
            "Ctrl-E (compose)",
            "open modal → Enter で本文に挿入",
        ),
        help_entry(
            palette,
            "type",
            "substring filter (prefix prioritized, alias 可)",
        ),
        help_entry(palette, "↑ / ↓", "navigate candidates"),
        help_entry(palette, "Enter", "confirm (mode に応じて 送信 / 挿入)"),
        help_entry(palette, "Esc", "cancel (何もしない)"),
        help_entry(palette, ":foo:", "local custom emoji (画像プレビュー)"),
        help_entry(
            palette,
            "👍 / 🎉 etc.",
            "Unicode emoji (gemoji 由来 shortcode)",
        ),
        Line::from(""),
        Line::from(Span::styled("profile", help_section(palette))),
        help_entry(palette, "j / k", "next / prev note"),
        help_entry(palette, "f", "follow / unfollow toggle"),
        help_entry(palette, "o", "load older notes"),
        help_entry(palette, "r", "refresh relationship + notes"),
        help_entry(palette, "Esc / q", "back to previous screen"),
        Line::from(""),
        Line::from(Span::styled("follow list", help_section(palette))),
        help_entry(palette, "j / k", "next / prev entry"),
        help_entry(palette, "t", "toggle following / followers"),
        help_entry(palette, "Enter", "open profile"),
        help_entry(palette, "o", "load more"),
        help_entry(palette, "r", "refresh tab"),
        help_entry(palette, "Esc / q", "back to timeline"),
        Line::from(""),
        Line::from(Span::styled("command mode (:)", help_section(palette))),
        help_entry(palette, ":follow X", "follow @user@host or URL"),
        help_entry(palette, ":unfollow X", "unfollow same"),
        help_entry(palette, ":open X", "open profile (acct or URL)"),
        help_entry(palette, ":lookup X", "alias of :open (Misskey 照会)"),
        help_entry(palette, ":me", "open own profile"),
        help_entry(palette, ":following", "open following list"),
        help_entry(palette, ":followers", "open followers list"),
        help_entry(palette, ":lock", "key-only mode (鍵アカ) on"),
        help_entry(palette, ":unlock", "key-only mode off"),
        help_entry(palette, ":requests", "pending follow requests"),
        help_entry(palette, ":lists", "open lists screen"),
        help_entry(palette, ":home", "back to home timeline"),
        help_entry(palette, ":q / :quit", "exit TUI"),
        help_entry(palette, "Tab", "complete command head"),
        Line::from(""),
        Line::from(Span::styled("follow requests", help_section(palette))),
        help_entry(palette, "j / k", "next / prev request"),
        help_entry(palette, "a", "approve selected"),
        help_entry(palette, "x", "reject selected"),
        help_entry(palette, "r", "refresh list"),
        help_entry(palette, "Esc / q", "back to timeline"),
        Line::from(""),
        Line::from(Span::styled("lists", help_section(palette))),
        help_entry(palette, "j / k", "next / prev list or member"),
        help_entry(palette, "Enter", "switch timeline to selected list"),
        help_entry(palette, "m", "open member list"),
        help_entry(palette, "n", "new list"),
        help_entry(palette, "R", "rename selected list"),
        help_entry(palette, "d", "delete selected list"),
        help_entry(palette, "a", "add member (in member list)"),
        help_entry(palette, "x", "remove member (in member list)"),
        help_entry(palette, "r", "refresh"),
        help_entry(palette, "Esc / q", "back (member list -> list, or close)"),
        Line::from(""),
        Line::from(Span::styled("compose alt submit", help_section(palette))),
        help_entry(palette, "F2", "send (always works)"),
        help_entry(palette, "Ctrl-Enter", "send (Kitty/WezTerm/Alacritty 等)"),
        Line::from(""),
        Line::from(Span::styled(
            "j/k=scroll  Space/PgDn=page  g/G=top/bottom  ?=close",
            Style::default().fg(palette.muted),
        )),
    ];

    // overlay 内寸 = block.inner で border 2 行を除いたサイズ。総行数と
    // viewport を `HelpState` に書き戻してから scroll をクランプ、その scroll
    // で `Paragraph::scroll` を呼ぶ。
    //
    // help テキストは静的かつ高々 100 行程度。`u16::MAX` (= 65535) を超える
    // ことは仕様上ありえないので `debug_assert` で開発時に気付けるようにし、
    // release では `as u16` で饱和 cast (wrap しない範囲)。
    debug_assert!(
        u16::try_from(lines.len()).is_ok(),
        "help text grew unreasonably large: {} lines",
        lines.len()
    );
    let total_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let inner_height = h.saturating_sub(2);
    app.help_state.sync_geometry(total_lines, inner_height);
    let scroll = app.help_state.scroll;
    let max_scroll = app.help_state.max_scroll();

    // scroll 可否を `▲▼` で示唆。リサイズで `max_scroll` が 0 ↔ 非 0 を行き来
    // してもタイトル幅がずれないよう、スクロール不要時も同じ幅 (= スペース 2
    // 文字) を予約しておく。
    let (up, down) = if max_scroll == 0 {
        (' ', ' ')
    } else {
        let u = if scroll == 0 { ' ' } else { '▲' };
        let d = if scroll >= max_scroll { ' ' } else { '▼' };
        (u, d)
    };
    let title = format!("  help {up}{down}  ");

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(p, inner);
    rect
}

fn help_section(palette: &Palette) -> Style {
    Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD)
}

fn help_entry(palette: &Palette, key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{key:<14}"),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(palette.foreground)),
    ])
}

/// M9 PR2: 視覚刺激抑制 overlay。中央に小さなパネルを浮かべ、各要素の
/// on/off を一覧する。`j/k` でカーソル移動、`space/Enter` でトグル、`!` で
/// 一括 off、`Esc/i` で閉じる (実際のキーバインドは [`crate::event`])。
#[allow(
    clippy::many_single_char_names,
    reason = "矩形 w/h/x/y は ratatui 慣習"
)]
fn render_suppression_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    use crate::suppression::Element;
    let palette = &app.theme.palette;
    fill_modal_backdrop(frame, area, palette);
    let w = area.width.clamp(36, 48);
    let h = 12.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    let block = Block::default()
        .title(Span::styled(
            "  visual suppression  ",
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let elements = Element::all();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(elements.len() + 3);
    lines.push(Line::from(Span::styled(
        "  toggle image elements".to_string(),
        Style::default().fg(palette.muted),
    )));
    lines.push(Line::from(""));
    for (i, e) in elements.iter().enumerate() {
        let on = app.suppression.is_on(*e);
        let cursor = if i == app.suppression_cursor {
            "▍ "
        } else {
            "  "
        };
        let box_char = if on { "[x]" } else { "[ ]" };
        let style = if i == app.suppression_cursor {
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD)
        } else if on {
            Style::default().fg(palette.foreground)
        } else {
            Style::default().fg(palette.muted)
        };
        lines.push(Line::from(Span::styled(
            format!("{cursor}{box_char} {}", e.label()),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j/k=move  space=toggle  !=all off  *=all on  Esc=close",
        Style::default().fg(palette.muted),
    )));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// M7: ファイルピッカの描画。中央に大きめの overlay を出して、左にエントリ
/// リスト、右にプレビューを並べる。返り値はリスト矩形 (= PageDown/Up の
/// 高さ算出用に runtime に戻す)。
#[allow(
    clippy::many_single_char_names,
    reason = "矩形 w/h/x/y は ratatui 慣習"
)]
fn render_picker(frame: &mut Frame<'_>, area: Rect, app: &App) -> Rect {
    let Some(picker) = app.picker.as_ref() else {
        return Rect::default();
    };
    let palette = &app.theme.palette;

    // 背後の Timeline がボックス外の余白から透けないよう全画面を塗る。
    fill_modal_backdrop(frame, area, palette);

    // ボックス本体 (= 端末の 8 割) を中央に置く。
    let w = area.width.saturating_sub(4).max(40);
    let h = area.height.saturating_sub(4).max(15);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    let title = format!(
        "  picker — {} :: {}  ",
        picker.mode.label(),
        picker.cwd.display()
    );
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        );
    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // 左 (リスト) と右 (プレビュー) に半々 split。
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(inner);
    let list_area = cols[0];
    let preview_area = cols[1];

    // `/` で開いたパス直接入力は一覧の上に 1 行の入力欄として重ねる
    // ([`crate::lists`] のタイトル/acct 入力と同じ組み立て)。
    let entries_area = if let Some(input) = picker.path_input.as_ref() {
        let input_rect = Rect::new(
            list_area.x,
            list_area.y,
            list_area.width,
            1.min(list_area.height),
        );
        render_picker_path_input(frame, input_rect, input, palette);
        Rect::new(
            list_area.x,
            list_area.y + input_rect.height,
            list_area.width,
            list_area.height.saturating_sub(input_rect.height),
        )
    } else {
        list_area
    };

    render_picker_list(frame, entries_area, picker, palette);
    render_picker_preview(frame, preview_area, picker, app, palette);

    list_area
}

/// パス直接入力欄。[`crate::lists`] のタイトル/acct 入力と同じ
/// 「ラベル + buffer + カーソル `_`」の組み立て。
fn render_picker_path_input(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &crate::picker::PathInput,
    palette: &Palette,
) {
    let line = Line::from(vec![
        Span::styled("  path: ", Style::default().fg(palette.muted)),
        Span::styled(
            input.buffer.clone(),
            Style::default().fg(palette.foreground),
        ),
        Span::styled("_", Style::default().fg(palette.accent)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_picker_list(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &crate::picker::FilePicker,
    palette: &Palette,
) {
    let visible = area.height as usize;
    if picker.entries.is_empty() {
        let msg = picker
            .last_error
            .clone()
            .unwrap_or_else(|| "(empty directory)".into());
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(p, area);
        return;
    }
    let top = picker.selected.saturating_sub(visible / 2);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible);
    for (i, entry) in picker.entries.iter().enumerate().skip(top).take(visible) {
        let marker = if i == picker.selected { "▍ " } else { "  " };
        let kind = if entry.is_dir { "[d]" } else { "[f]" };
        let style = if i == picker.selected {
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_dir {
            Style::default().fg(palette.accent)
        } else {
            Style::default().fg(palette.foreground)
        };
        let display = format!("{marker}{kind} {}", entry.name);
        lines.push(Line::from(Span::styled(display, style)));
    }
    if picker.truncated {
        lines.push(Line::from(Span::styled(
            "  … (truncated)".to_string(),
            Style::default().fg(palette.warning),
        )));
    }
    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

fn render_picker_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &crate::picker::FilePicker,
    app: &App,
    palette: &Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(current) = picker.current() else {
        return;
    };
    // メタデータ 3 行を上に書き、残りをプレビュー画像 (あれば) に使う。
    let head_lines = vec![
        Line::from(Span::styled(
            format!("  {}", current.name),
            Style::default()
                .fg(palette.accent_strong)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {}", current.path.display()),
            Style::default().fg(palette.muted),
        )),
        Line::from(Span::styled(
            // size は picker::try_read_dir で事前に stat 済み (= render は
            // I/O フリー)。Some(0) も Some(n) と同形で表示する。
            if current.is_dir {
                "  (directory)".to_string()
            } else if let Some(n) = current.size {
                format!("  size: {n} bytes")
            } else {
                "  size: (unknown)".to_string()
            },
            Style::default().fg(palette.foreground),
        )),
        Line::from(""),
    ];
    let head_height = u16::try_from(head_lines.len())
        .unwrap_or(4)
        .min(area.height);
    let head_rect = Rect::new(area.x, area.y, area.width, head_height);
    let preview_rect = Rect::new(
        area.x,
        area.y + head_height,
        area.width,
        area.height.saturating_sub(head_height),
    );
    let p = Paragraph::new(head_lines).wrap(Wrap { trim: false });
    frame.render_widget(p, head_rect);

    if current.is_dir {
        return;
    }

    // M9 PR2: 画像取得経路と視覚刺激抑制 (preview トグル) の両方が on
    // のときだけプレビューを描く。
    if !app.previews.enabled() || !app.suppression.preview {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  (preview disabled)",
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(placeholder, preview_rect);
        return;
    }

    // ensure (= fetch trigger) は副作用つき。`app` は不変参照だが、ensure
    // 自身は内部 Mutex で書き換える。
    app.previews.ensure(&current.path, preview_rect);
    if let Some(proto) = app.previews.get(&current.path) {
        let widget = Image::new(proto.as_ref());
        frame.render_widget(widget, preview_rect);
    } else {
        // 取得中 / 失敗。失敗理由を出す。
        let msg = match app.previews.state(&current.path) {
            Some(crate::preview::PreviewState::Failed { reason, .. }) => {
                format!("  preview failed: {reason}")
            }
            Some(crate::preview::PreviewState::Video { size }) => {
                // Kitty graphics protocol は静止画向けのため実再生・
                // サムネイル生成はしない (ポスターフレーム抽出は別issue)。
                format!("  🎬 video attachment ({size} bytes) — no inline preview")
            }
            _ => "  loading preview…".to_string(),
        };
        let placeholder = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(palette.muted),
        )));
        frame.render_widget(placeholder, preview_rect);
    }
}

fn border_style(palette: &Palette, focused: bool) -> Style {
    if focused {
        Style::default().fg(palette.accent)
    } else {
        Style::default().fg(palette.border)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_caps_at_max() {
        assert_eq!(truncate_for_width("hello", 3), "he…");
        assert_eq!(truncate_for_width("hi", 5), "hi");
    }

    #[test]
    fn spinner_static_when_animation_off() {
        // suppression.animation = false: 時刻によらず常に ● 静止。
        assert_eq!(spinner_char_at_ms(false, 0), '●');
        assert_eq!(spinner_char_at_ms(false, 12_345), '●');
        assert_eq!(spinner_char_at_ms(false, u128::MAX), '●');
    }

    #[test]
    fn spinner_cycles_braille_when_animation_on() {
        // animation on: 100 ms ごとに位相が進み、10 周期で wrap する。
        for (i, expected) in SPINNER_FRAMES.iter().enumerate() {
            let ms = (i as u128) * 100;
            assert_eq!(spinner_char_at_ms(true, ms), *expected, "frame {i}");
        }
        // 1 周回って先頭に戻る。
        assert_eq!(spinner_char_at_ms(true, 1_000), SPINNER_FRAMES[0]);
        assert_eq!(spinner_char_at_ms(true, 1_100), SPINNER_FRAMES[1]);
    }

    #[test]
    fn spinner_phase_advances_after_100ms() {
        // 99ms 以下は同じ frame、100ms 跨ぐと隣の frame に進む。
        assert_eq!(spinner_char_at_ms(true, 50), spinner_char_at_ms(true, 99));
        assert_ne!(spinner_char_at_ms(true, 99), spinner_char_at_ms(true, 100));
    }

    #[test]
    fn extract_host_works() {
        assert_eq!(
            extract_host("https://example.test/users/alice"),
            Some("example.test".into()),
        );
        assert_eq!(extract_host("not a url"), None);
    }

    /// **Issue #144 回帰**: ASCII の空白を含む CJK 長文で `WordWrapper` が
    /// 空白優先折返しを行うと、旧 `div_ceil(line.width(), width)` 近似は実
    /// 描画より 1 行少ない値を返していた。`Paragraph::line_count` 経由で
    /// 実描画と完全一致することを確認する。
    #[test]
    fn wrapped_line_height_matches_word_wrapper_with_cjk_and_space() {
        // 「HTTP GET にも署名を実装してみたい今日この頃ですが ──」
        // ASCII の "HTTP GET" の直後に空白があり、WordWrapper はそこで折り
        // 返すため、近似 ceil(width / cols) よりも 1 行多く必要になる幅を
        // 選ぶ。viewport_width = 12 cells のとき:
        //   span 全体の display width = 8 (HTTP GET) + 1 (space) + ...
        //   先頭 word "HTTP" は 4 cells、続く word "GET" は 3 cells で 12
        //   セルに収まるが、次の word "にも署名" の直前で折り返される。
        let line =
            Line::from("HTTP GETにも署名を実装してみたい今日この頃ですが ── そう簡単じゃない");
        let h = wrapped_line_height(&line, 12);

        // Paragraph::line_count と一致 (= 同じ実装に委譲しているので自明)。
        // ここでは「div_ceil 近似より大きい」ことだけ確認する: 旧実装の
        // バグはこの差で発生していた。
        let display_width = u16::try_from(line.width()).unwrap();
        let approx = display_width.div_ceil(12);
        assert!(
            h > approx,
            "WordWrapper の実行数 {h} は div_ceil 近似 {approx} より大きいはず (空白優先折返しで 1 行多くなる)"
        );
    }

    /// 空 Line / `viewport_width = 0` は 1 行扱い ── 行が消えてアバター
    /// 位置が縮退しないように維持する不変条件。
    #[test]
    fn wrapped_line_height_handles_empty_and_zero_width() {
        assert_eq!(wrapped_line_height(&Line::from(""), 80), 1);
        assert_eq!(wrapped_line_height(&Line::from("hello"), 0), 1);
    }

    /// ASCII のみで wrap が発生しないケース ── `div_ceil` 近似と
    /// `Paragraph::line_count` が一致するため、回帰しても気付きにくいので
    /// 1 行で済む短文と確実に折り返す長文の双方で確認しておく。
    #[test]
    fn wrapped_line_height_ascii_basic() {
        let short = Line::from("hello world");
        assert_eq!(wrapped_line_height(&short, 80), 1);

        let long = Line::from("abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(wrapped_line_height(&long, 10), 4); // 36 / 10 → 4
    }

    #[test]
    fn reaction_label_strips_colons_and_host() {
        assert_eq!(reaction_label(":blob:"), ":blob:");
        assert_eq!(reaction_label(":blob_party:"), ":blob_party:");
        assert_eq!(reaction_label(":blob@misskey.io:"), ":blob:");
        // Unicode は素通り。
        assert_eq!(reaction_label("👍"), "👍");
        // 空 / 不正は素通り (= サーバ側で検証済み)。
        assert_eq!(reaction_label(""), "");
        assert_eq!(reaction_label("::"), "::");
    }

    /// `append_folded_body` テストのために palette だけ取り出すヘルパ。
    /// 組み込みテーマの sakura を読んでパレットを使う ── ハードコードしない
    /// 規約 (CLAUDE.md §10) に沿う。
    fn test_palette() -> crate::theme::Theme {
        crate::theme::Theme::builtin("sakura").expect("builtin sakura theme")
    }

    /// `Line` から prefix 以外の本文 / indicator スパンを「 [+N 行]」を含めて
    /// 1 文字列に連結する。assert で正確に何が出るか比較するため。
    fn body_text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .skip(1) // 先頭 Span は prefix
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn fold_no_truncation_when_under_max() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        append_folded_body(&mut out, "a\nb", "  ", 80, 2, &theme.palette);
        assert_eq!(out.len(), 2);
        assert_eq!(body_text_of(&out[0]), "a");
        assert_eq!(body_text_of(&out[1]), "b");
    }

    #[test]
    fn fold_inserts_indicator_when_truncated() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        append_folded_body(&mut out, "a\nb\nc\nd\ne", "  ", 80, 2, &theme.palette);
        // max=2 のうち最終行に ` [+3 行]` indicator が付く。
        assert_eq!(out.len(), 2);
        assert_eq!(body_text_of(&out[0]), "a");
        assert_eq!(body_text_of(&out[1]), "b [+3 行]");
    }

    #[test]
    fn fold_focused_more_lines() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        // 6 行入力、max=5 でフォーカス相当。最後 1 行は indicator 付き。
        append_folded_body(
            &mut out,
            "1\n2\n3\n4\n5\n6",
            "  ",
            80,
            FOCUSED_BODY_LINES,
            &theme.palette,
        );
        assert_eq!(out.len(), FOCUSED_BODY_LINES);
        assert_eq!(body_text_of(&out[FOCUSED_BODY_LINES - 1]), "5 [+1 行]");
    }

    #[test]
    fn fold_max_zero_disables_folding() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        append_folded_body(&mut out, "a\nb\nc", "  ", 80, 0, &theme.palette);
        assert_eq!(out.len(), 3);
        // どの行にも indicator は付かない。
        assert_eq!(body_text_of(&out[2]), "c");
    }

    #[test]
    fn fold_empty_input_produces_nothing() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        append_folded_body(&mut out, "", "  ", 80, 2, &theme.palette);
        assert!(out.is_empty());
    }

    #[test]
    fn fold_exact_max_no_indicator() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        // 入力 2 行ちょうど、max=2 → 切り詰め無し、indicator なし。
        append_folded_body(&mut out, "x\ny", "  ", 80, 2, &theme.palette);
        assert_eq!(out.len(), 2);
        assert_eq!(body_text_of(&out[1]), "y");
    }

    #[test]
    fn fold_indicator_does_not_overflow_body_line() {
        let theme = test_palette();
        let mut out: Vec<Line<'static>> = Vec::new();
        // 1 行目は body_width に収まる長さ、2 行目 (= 切り詰め最終行) は
        // 本文を indicator 文字分減らした幅で truncate されるはず。
        let long = "abcdefghijklmnopqrstuvwxyz"; // 26 chars
        let input = format!("first\n{long}\n3\n4");
        // body_width = 10、indicator = " [+2 行]" は 7 chars 想定。
        // 本文側は 10 - 7 = 3 chars に truncate → "a…"(2) + indicator 7 = 合計
        // 9 cells で 10 以下に収まる。
        append_folded_body(&mut out, &input, "  ", 10, 2, &theme.palette);
        assert_eq!(out.len(), 2);
        let last = body_text_of(&out[1]);
        assert!(
            last.contains("[+2 行]"),
            "last line must keep indicator regardless of truncate: {last:?}"
        );
        // 本文側が短すぎて全部 truncate されたとしても indicator は残る。
    }
}
