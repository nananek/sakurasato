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
    /// Issue #115: `FollowList` 画面の一覧領域 (= `ensure_visible` / `PageDown` 用)。
    /// 非表示時は zero rect。
    pub follow_list: Rect,
}

/// タイムラインのスクロール可能領域内に並んだ note の行位置をビット圧縮せず
/// `Vec` に積む。MouseClick 解決時に上から線形探索する (高々 80 件)。
pub type ScrollHits = hit::ScrollHits;

/// 1 frame 分を描画。返り値は次の `MouseClick` を解決するためのレイアウト矩形。
pub fn draw(frame: &mut Frame<'_>, app: &App) -> PanelRects {
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
    let mut follow_list_rect = Rect::default();
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
    } else {
        render_timeline(frame, timeline_area, app)
    };
    render_compose(frame, compose_area, app);
    render_status(frame, status_area, app);

    let help_area = if app.focus == Focus::Help {
        Some(render_help(frame, area, &app.theme))
    } else {
        None
    };

    // M7: ピッカは画面中央 overlay。Help と同じ層に出すので Help と排他的に
    // しなくてもいいが、両方同時に出ると操作が混乱するので Picker focus 時
    // は Help は描かない設計 (= 上で focus == Help のときだけ render_help)。
    let picker_list = if app.focus == Focus::Picker {
        render_picker(frame, area, app)
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
        follow_list: follow_list_rect,
    }
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

            app.images.ensure(&item.url, img_area);
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
            "  Enter=run  Esc=cancel",
            Style::default().fg(palette.muted),
        ),
    ]);
    let p = Paragraph::new(line).style(Style::default().bg(palette.background));
    frame.render_widget(p, area);
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
            avatar_overlays.push((visible_top, url));
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
    let summary_lines: Vec<String> = profile
        .actor
        .summary
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
    out.push(Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(format!("[{time}]"), Style::default().fg(palette.muted)),
        Span::raw("  "),
        Span::styled(
            format!("({})", note.visibility),
            Style::default().fg(palette.muted),
        ),
    ]));
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
    for body_line in note.content.lines() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate_for_width(body_line, body_width),
                Style::default().fg(palette.foreground),
            ),
        ]));
    }
    if note.content.is_empty() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("(empty)", Style::default().fg(palette.muted)),
        ]));
    }
    if !note.reactions.is_empty() {
        out.push(reaction_line(note, palette, ""));
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

    let local_published = note.published_at.with_timezone(&Local);
    let time = local_published.format("%H:%M").to_string();
    let handle = format_handle(note);
    let header = Line::from(vec![
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
    ]);
    out.push(header);

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
    for body_line in note.content.lines() {
        out.push(Line::from(vec![
            Span::raw(format!("{pad}  ")),
            Span::styled(
                truncate_for_width(body_line, body_width),
                Style::default().fg(palette.foreground),
            ),
        ]));
    }
    if note.content.is_empty() {
        out.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("  (empty)", Style::default().fg(palette.muted)),
        ]));
    }
    if !note.reactions.is_empty() {
        out.push(reaction_line(note, palette, &pad));
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
        Focus::EmojiSearch => "emoji",
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
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
fn render_help(frame: &mut Frame<'_>, area: Rect, theme: &Theme) -> Rect {
    let palette = &theme.palette;
    // 中央に max(60, area.width * 0.6) x min(20, area.height - 4) を浮かべる。
    let w = area.width.clamp(40, 60);
    let h = area.height.saturating_sub(6).clamp(10, 18);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    let block = Block::default()
        .title(Span::styled(
            "  help — keymap  ",
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
        help_entry(palette, ":q / :quit", "exit TUI"),
        Line::from(""),
        Line::from(Span::styled("follow requests", help_section(palette))),
        help_entry(palette, "j / k", "next / prev request"),
        help_entry(palette, "a", "approve selected"),
        help_entry(palette, "x", "reject selected"),
        help_entry(palette, "r", "refresh list"),
        help_entry(palette, "Esc / q", "back to timeline"),
        Line::from(""),
        Line::from(Span::styled("compose alt submit", help_section(palette))),
        help_entry(palette, "F2", "send (always works)"),
        help_entry(palette, "Ctrl-Enter", "send (Kitty/WezTerm/Alacritty 等)"),
        Line::from(""),
        Line::from(Span::styled(
            "press ? again to close",
            Style::default().fg(palette.muted),
        )),
    ];
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
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

    // 全画面 overlay (= 端末の 8 割) を使う。
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

    render_picker_list(frame, list_area, picker, palette);
    render_picker_preview(frame, preview_area, picker, app, palette);

    list_area
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
}
