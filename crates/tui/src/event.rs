//! crossterm の生イベントを抽象アクションに翻訳する層。
//!
//! ループ本体 ([`crate::runtime`]) を「Action を `App` に適用する」だけに
//! 留めたいので、キー/マウスの解釈 (focus 切替・スクロール) はここに寄せる。
//! マウスクリックの「どのパネルか」の判定だけは、レイアウト矩形を知る runtime
//! 側に任せる ([`Action::MouseClick`] が `(col, row)` をそのまま渡す)。

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::Focus;
use crate::picker::PickerMode;

#[derive(Debug, Clone)]
pub enum Action {
    Noop,
    Quit,
    SelectNext,
    SelectPrev,
    PageDown,
    PageUp,
    RefreshTimeline,
    LoadMore,
    EnterCompose,
    FocusTimeline,
    ToggleHelp,
    CycleTheme,
    InsertChar(char),
    InsertNewline,
    Backspace,
    DeleteForward,
    MoveLeft,
    MoveRight,
    MoveLineStart,
    MoveLineEnd,
    /// 投稿サブミット (`Ctrl-Enter`)。
    SubmitNote,
    /// CW 行と本文行の切替 (`Ctrl-W`)。
    ToggleCw,
    /// `sensitive` トグル (`Ctrl-S`)。
    ToggleSensitive,
    /// visibility 循環 (`Ctrl-V`)。
    CycleVisibility,
    /// マウスホイール: 正で下方向、負で上方向、絶対値はステップ数。
    Scroll(i32),
    /// マウス左クリック。座標 (col, row) を渡し、runtime 側でパネル判定する。
    MouseClick(u16, u16),
    /// M7: ファイルピッカを開く。timeline / compose から発行。
    OpenPicker(PickerMode),
    /// M7: ピッカ内ナビゲーション。
    PickerNext,
    PickerPrev,
    PickerPageDown,
    PickerPageUp,
    /// Enter ── ディレクトリ降りる / ファイル選択 (= upload kick)。
    PickerActivate,
    /// Backspace ── 親に上る。
    PickerParent,
    /// `.` ── 隠しファイル表示トグル。
    PickerToggleHidden,
    /// Esc ── ピッカを閉じる。
    PickerCancel,
    /// M7: 直近の添付を 1 件外す (compose focus 中)。
    PopAttachment,
}

/// crossterm イベント → Action。
#[allow(
    clippy::needless_pass_by_value,
    reason = "値で渡す `Event` を tests でも自然に書きたい"
)]
pub fn translate(event: Event, focus: Focus) -> Action {
    match event {
        Event::Key(k) => translate_key(k, focus),
        Event::Mouse(m) => translate_mouse(m),
        Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
            Action::Noop
        }
    }
}

fn translate_key(k: KeyEvent, focus: Focus) -> Action {
    if k.kind == KeyEventKind::Release {
        return Action::Noop;
    }
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    match focus {
        Focus::Timeline => translate_timeline_key(k),
        Focus::Compose => translate_compose_key(k),
        Focus::Help => translate_help_key(k),
        Focus::Picker => translate_picker_key(k),
    }
}

fn translate_timeline_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Char('q'), m) if m.is_empty() => Action::Quit,
        (KeyCode::Esc, _) => Action::Quit,
        (KeyCode::Char('?'), _) => Action::ToggleHelp,
        (KeyCode::Char('h'), m) if m.contains(KeyModifiers::CONTROL) => Action::ToggleHelp,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::PageDown | KeyCode::Char(' '), _) => Action::PageDown,
        (KeyCode::PageUp, _) => Action::PageUp,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::RefreshTimeline,
        (KeyCode::Char('o'), m) if m.is_empty() => Action::LoadMore,
        (KeyCode::Char('n'), m) if m.is_empty() => Action::EnterCompose,
        (KeyCode::Char('t'), m) if m.is_empty() => Action::CycleTheme,
        // M7: A = avatar, H = header, ; = attachment ─ いずれもファイル
        // ピッカを当該モードで開く。小文字キーは timeline ナビと衝突
        // するため Shift 付き / `;` を採用。
        (KeyCode::Char('A'), _) => Action::OpenPicker(PickerMode::Avatar),
        (KeyCode::Char('H'), _) => Action::OpenPicker(PickerMode::Header),
        (KeyCode::Char(';'), m) if m.is_empty() => Action::OpenPicker(PickerMode::Attachment),
        _ => Action::Noop,
    }
}

fn translate_compose_key(k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => Action::FocusTimeline,
        KeyCode::Enter => {
            if ctrl {
                Action::SubmitNote
            } else {
                // 単独 Enter は改行 (Mastodon Web UI の振る舞いに揃える)。
                Action::InsertNewline
            }
        }
        KeyCode::Char('w') if ctrl => Action::ToggleCw,
        KeyCode::Char('s') if ctrl => Action::ToggleSensitive,
        KeyCode::Char('v') if ctrl => Action::CycleVisibility,
        // M7: Ctrl-A で添付ピッカを開く。`a` 単独は本文に挿入されるので Ctrl 必須。
        KeyCode::Char('a') if ctrl => Action::OpenPicker(PickerMode::Attachment),
        // M7: Ctrl-D で末尾の添付を 1 件外す (compose に居ながらの取り消し)。
        KeyCode::Char('d') if ctrl => Action::PopAttachment,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::DeleteForward,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,
        KeyCode::Home => Action::MoveLineStart,
        KeyCode::End => Action::MoveLineEnd,
        KeyCode::Char(c) if !ctrl => Action::InsertChar(c),
        _ => Action::Noop,
    }
}

fn translate_help_key(k: KeyEvent) -> Action {
    match k.code {
        KeyCode::Esc | KeyCode::Char('?' | 'q') => Action::ToggleHelp,
        _ => Action::Noop,
    }
}

fn translate_picker_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::PickerCancel,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::PickerCancel,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::PickerNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::PickerPrev,
        (KeyCode::PageDown, _) => Action::PickerPageDown,
        (KeyCode::PageUp, _) => Action::PickerPageUp,
        (KeyCode::Enter, _) => Action::PickerActivate,
        (KeyCode::Backspace, _) => Action::PickerParent,
        // `.` で隠しファイルトグル ── vim の :set hidden! 風。
        (KeyCode::Char('.'), m) if m.is_empty() => Action::PickerToggleHidden,
        _ => Action::Noop,
    }
}

fn translate_mouse(m: MouseEvent) -> Action {
    match m.kind {
        MouseEventKind::ScrollDown => Action::Scroll(3),
        MouseEventKind::ScrollUp => Action::Scroll(-3),
        MouseEventKind::Down(MouseButton::Left) => Action::MouseClick(m.column, m.row),
        _ => Action::Noop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn timeline_q_quits() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('q'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::Quit,
        ));
    }

    #[test]
    fn timeline_j_selects_next() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::SelectNext,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Down, KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::SelectNext,
        ));
    }

    #[test]
    fn ctrl_c_always_quits() {
        for focus in [Focus::Timeline, Focus::Compose, Focus::Help] {
            assert!(matches!(
                translate(
                    Event::Key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                    focus,
                ),
                Action::Quit,
            ));
        }
    }

    #[test]
    fn compose_ctrl_enter_submits() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::CONTROL)),
                Focus::Compose,
            ),
            Action::SubmitNote,
        ));
    }

    #[test]
    fn compose_plain_enter_inserts_newline() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::Compose,
            ),
            Action::InsertNewline,
        ));
    }

    #[test]
    fn compose_char_inserts() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)),
                Focus::Compose,
            ),
            Action::InsertChar('a'),
        ));
    }

    #[test]
    fn release_events_become_noop() {
        let k = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert!(matches!(
            translate(Event::Key(k), Focus::Timeline),
            Action::Noop,
        ));
    }

    #[test]
    fn mouse_scroll_translates() {
        let evt = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(translate_mouse(evt), Action::Scroll(n) if n > 0));
    }
}
