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
    /// M8 PR3: タイムラインで選択中の Note に対するリアクション送信プロンプトを
    /// 開く。`note_id` を後段で確定するため、ペイロードは載せない。
    OpenReactionPrompt,
    /// プロンプト中の文字入力。
    ReactionPromptInsertChar(char),
    /// プロンプト中の Backspace。
    ReactionPromptBackspace,
    /// プロンプト中の Enter ── 入力済み content を `POST /api/v1/reactions` へ。
    ReactionPromptSubmit,
    /// プロンプト中の Esc ── キャンセル。
    ReactionPromptCancel,
    /// M9 PR2: 視覚刺激抑制 overlay を開く / 閉じる。
    ToggleSuppression,
    /// suppression overlay 上のカーソル移動。
    SuppressionNext,
    SuppressionPrev,
    /// 現在カーソルが指す要素を toggle (= space / Enter)。
    SuppressionToggle,
    /// 全要素を一括 off (= `!`)。
    SuppressionDisableAll,
    /// 全要素を一括 on (= 復帰用、= `*`)。
    SuppressionEnableAll,
    /// Esc ── overlay を閉じる。
    SuppressionClose,
    /// M13 PR6: 選択中の Note への返信を開始 (= Compose に `in_reply_to` をセット)。
    ReplyToSelected,
    /// M13 PR6: 選択中の Note への自分の直近リアクションを取り消す。
    UndoReactionOnSelected,
    /// M13 PR6: alt text 入力中の文字入力。
    AltPromptInsertChar(char),
    /// M13 PR6: alt text プロンプトの Backspace。
    AltPromptBackspace,
    /// M13 PR6: Enter ── alt text 確定で upload を kick。
    AltPromptSubmit,
    /// M13 PR6: Esc ── alt text プロンプトをキャンセル (= アップロードを破棄)。
    AltPromptCancel,
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
        Focus::ReactionPrompt => translate_reaction_prompt_key(k),
        Focus::Suppression => translate_suppression_key(k),
        Focus::AltPrompt => translate_alt_prompt_key(k),
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
        // M8 PR3: e = react ─ 選択中の Note にリアクションを付けるための
        // プロンプトを開く。
        (KeyCode::Char('e'), m) if m.is_empty() => Action::OpenReactionPrompt,
        // M9 PR2: i = 視覚刺激抑制 overlay を開く ("images" の頭文字)。
        // Compose 中は `i` が本文に挿入されるので timeline focus 限定。
        (KeyCode::Char('i'), m) if m.is_empty() => Action::ToggleSuppression,
        // M13 PR6: R = 返信 (大文字 ── refresh `r` と衝突しないように)。
        (KeyCode::Char('R'), _) => Action::ReplyToSelected,
        // M13 PR6: u = 自分が直近に付けた reaction を取り消し。
        (KeyCode::Char('u'), m) if m.is_empty() => Action::UndoReactionOnSelected,
        _ => Action::Noop,
    }
}

fn translate_alt_prompt_key(k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => Action::AltPromptCancel,
        KeyCode::Enter => Action::AltPromptSubmit,
        KeyCode::Backspace => Action::AltPromptBackspace,
        KeyCode::Char(c) if !ctrl => Action::AltPromptInsertChar(c),
        _ => Action::Noop,
    }
}

fn translate_suppression_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q' | 'i'), m) if m.is_empty() => Action::SuppressionClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SuppressionNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SuppressionPrev,
        (KeyCode::Char(' ') | KeyCode::Enter, _) => Action::SuppressionToggle,
        (KeyCode::Char('!'), m) if m.is_empty() => Action::SuppressionDisableAll,
        (KeyCode::Char('*'), m) if m.is_empty() => Action::SuppressionEnableAll,
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

fn translate_reaction_prompt_key(k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => Action::ReactionPromptCancel,
        KeyCode::Enter => Action::ReactionPromptSubmit,
        KeyCode::Backspace => Action::ReactionPromptBackspace,
        KeyCode::Char(c) if !ctrl => Action::ReactionPromptInsertChar(c),
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
    fn timeline_e_opens_reaction_prompt() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('e'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::OpenReactionPrompt,
        ));
    }

    #[test]
    fn reaction_prompt_keys_route_correctly() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)),
                Focus::ReactionPrompt,
            ),
            Action::ReactionPromptCancel,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::ReactionPrompt,
            ),
            Action::ReactionPromptSubmit,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)),
                Focus::ReactionPrompt,
            ),
            Action::ReactionPromptInsertChar('a'),
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Backspace, KeyModifiers::NONE)),
                Focus::ReactionPrompt,
            ),
            Action::ReactionPromptBackspace,
        ));
        // Ctrl-C は ReactionPrompt focus でも Quit に勝つ。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Focus::ReactionPrompt,
            ),
            Action::Quit,
        ));
    }

    #[test]
    fn timeline_i_opens_suppression_overlay() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('i'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::ToggleSuppression,
        ));
    }

    #[test]
    fn suppression_focus_keys_route_correctly() {
        for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('i')] {
            assert!(matches!(
                translate(
                    Event::Key(key(code, KeyModifiers::NONE)),
                    Focus::Suppression,
                ),
                Action::SuppressionClose,
            ));
        }
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
                Focus::Suppression,
            ),
            Action::SuppressionToggle,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('!'), KeyModifiers::NONE)),
                Focus::Suppression,
            ),
            Action::SuppressionDisableAll,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('*'), KeyModifiers::NONE)),
                Focus::Suppression,
            ),
            Action::SuppressionEnableAll,
        ));
    }

    #[test]
    fn timeline_capital_r_replies() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('R'), KeyModifiers::SHIFT)),
                Focus::Timeline,
            ),
            Action::ReplyToSelected,
        ));
    }

    #[test]
    fn timeline_u_undoes_reaction() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('u'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::UndoReactionOnSelected,
        ));
    }

    #[test]
    fn alt_prompt_keys_route_correctly() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)),
                Focus::AltPrompt,
            ),
            Action::AltPromptCancel,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::AltPrompt,
            ),
            Action::AltPromptSubmit,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)),
                Focus::AltPrompt,
            ),
            Action::AltPromptInsertChar('a'),
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Backspace, KeyModifiers::NONE)),
                Focus::AltPrompt,
            ),
            Action::AltPromptBackspace,
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
