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

    let rows = render_timeline(frame, timeline_area, app);
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

    PanelRects {
        timeline: timeline_area,
        timeline_rows: rows,
        compose: compose_area,
        help: help_area,
        picker_list,
    }
}

fn compose_height(app: &App) -> u16 {
    // 投稿フォーカス中は 5 行、それ以外は 3 行 (header + 入力 1 行 + spacer)。
    if app.focus == Focus::Compose { 7 } else { 3 }
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

    let avatar_enabled = app.images.enabled();
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
        let consumed = u16::try_from(block_lines.len()).unwrap_or(u16::MAX);
        let visible_top = inner.y + row_cursor;
        let visible_height = consumed.min(inner.height - row_cursor);
        hits.push(idx, visible_top, visible_height);

        if avatar_enabled
            && visible_height >= 1
            && let Some(url) = note.actor_icon_url.as_deref()
        {
            avatar_overlays.push((visible_top, url));
        }

        for l in block_lines {
            if row_cursor >= inner.height {
                break;
            }
            lines.push(l);
            row_cursor += 1;
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
    out.push(Line::from(""));
    out
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
        let (row, col) = if app.compose.editing_cw() {
            (0, "CW> ".chars().count() + app.compose.cw().chars().count())
        } else {
            let (r, c) = app.compose.cursor_row_col();
            // CW 行が乗っている場合は +1。
            let offset = u16::from(app.compose.editing_cw());
            (r + usize::from(offset), c)
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
            if current.is_dir {
                "  (directory)".to_string()
            } else {
                match std::fs::metadata(&current.path) {
                    Ok(m) => format!("  size: {} bytes", m.len()),
                    Err(err) => format!("  stat error: {err}"),
                }
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

    if !app.previews.enabled() {
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
}
