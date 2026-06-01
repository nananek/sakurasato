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
    /// M13 PR4: Timeline で選択中の Note の author の Profile 画面を push。
    OpenProfileFromSelected,
    /// M13 PR4: Profile 画面でカーソル下移動 (note 一覧)。
    ProfileSelectNext,
    /// M13 PR4: Profile 画面でカーソル上移動。
    ProfileSelectPrev,
    /// M13 PR4: Profile 画面で `o` ── 古い note ページを追加取得。
    ProfileLoadMoreNotes,
    /// M13 PR4: Profile 画面で `f` ── follow / unfollow を toggle。
    ProfileToggleFollow,
    /// M13 PR4: Profile 画面で `Esc`/`q` ── stack を 1 段 pop。空になれば Timeline 復帰。
    ProfileBack,
    /// M13 PR4: Profile 画面で `r` ── actor + relationship + 直近 notes を再取得。
    ProfileRefresh,
    /// M13 PR5: コマンドプロンプトを開く (`:` キー)。
    OpenCommand,
    /// M13 PR5: コマンドプロンプト中の文字入力。
    CommandInsertChar(char),
    /// M13 PR5: コマンドプロンプト中の Backspace。
    CommandBackspace,
    /// M13 PR5: コマンドプロンプト中の Enter ── parse + 実行。
    CommandSubmit,
    /// M13 PR5: コマンドプロンプト中の Esc ── キャンセル。
    CommandCancel,
    /// M13 PR5: `FollowList` で次のエントリを選択。
    FollowListSelectNext,
    /// M13 PR5: `FollowList` で前のエントリを選択。
    FollowListSelectPrev,
    /// M13 PR5: `FollowList` で `t` ── following / followers タブ切替。
    FollowListToggleMode,
    /// M13 PR5: `FollowList` で `Enter` ── 選択行の actor の Profile を push。
    FollowListOpenSelected,
    /// M13 PR5: `FollowList` で `o` ── 古いページ追加取得。
    FollowListLoadMore,
    /// M13 PR5: `FollowList` で `r` ── 現在タブを再取得。
    FollowListRefresh,
    /// M13 PR5: `FollowList` で `Esc` / `q` ── 画面を閉じる。
    FollowListClose,
    /// Issue #101: 絵文字検索モーダルを開く (= reaction prompt / compose で
    /// `Ctrl-E`)。runtime が server `GET /api/v1/emojis` を叩いて母集団を
    /// 確保したあと `Focus::EmojiSearch` に切替える。
    OpenEmojiSearch,
    /// 絵文字検索モーダル中の `↓` (or `Ctrl-N`) ── 次候補へ。
    EmojiSearchDown,
    /// 絵文字検索モーダル中の `↑` (or `Ctrl-P`) ── 前候補へ。
    EmojiSearchUp,
    /// 絵文字検索モーダル中の `Enter` ── 選択中 shortcode を `:foo:` 形式で
    /// 戻り先 (`ReactionPrompt` / `Compose`) の buffer に挿入し閉じる。
    EmojiSearchConfirm,
    /// 絵文字検索モーダル中の `Esc` ── 何も挿入せず閉じる。
    EmojiSearchCancel,
    /// 絵文字検索モーダル中の文字入力。
    EmojiSearchInsertChar(char),
    /// 絵文字検索モーダル中の Backspace。
    EmojiSearchBackspace,
    /// M12 (#66): `:lock` ── 鍵アカ運用に切替 (`POST /api/v1/actor/lock`)。
    ActorLock,
    /// M12 (#66): `:unlock` ── 鍵アカ解除。
    ActorUnlock,
    /// M12 (#66): `:requests` ── 承認待ち follow 一覧画面を push。
    OpenFollowRequests,
    /// M12 (#66): 一覧画面でカーソル下移動。
    RequestsSelectNext,
    /// M12 (#66): 一覧画面でカーソル上移動。
    RequestsSelectPrev,
    /// M12 (#66): 一覧画面で `a` ── 選択行を approve。
    RequestsApproveSelected,
    /// M12 (#66): 一覧画面で `x` ── 選択行を reject。
    RequestsRejectSelected,
    /// M12 (#66): 一覧画面で `r` ── 再取得。
    RequestsRefresh,
    /// M12 (#66): 一覧画面で `Esc` / `q` ── 画面を閉じる。
    RequestsClose,
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
        Focus::Profile => translate_profile_key(k),
        Focus::FollowList => translate_follow_list_key(k),
        Focus::Command => translate_command_key(k),
        Focus::Requests => translate_requests_key(k),
        Focus::EmojiSearch => translate_emoji_search_key(k),
    }
}

/// Issue #101: 絵文字検索モーダルのキー操作。
fn translate_emoji_search_key(k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => Action::EmojiSearchCancel,
        KeyCode::Enter => Action::EmojiSearchConfirm,
        KeyCode::Up => Action::EmojiSearchUp,
        KeyCode::Down => Action::EmojiSearchDown,
        KeyCode::Char('p') if ctrl => Action::EmojiSearchUp,
        KeyCode::Char('n') if ctrl => Action::EmojiSearchDown,
        KeyCode::Backspace => Action::EmojiSearchBackspace,
        // 任意の通常文字 (= 大小文字 / 記号も含む) を検索 buffer に流す。
        // Ctrl 押下中の文字 (= shortcut の取り違え) は無視。
        KeyCode::Char(c) if !ctrl => Action::EmojiSearchInsertChar(c),
        _ => Action::Noop,
    }
}

fn translate_requests_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::RequestsClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::RequestsClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::RequestsSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::RequestsSelectPrev,
        (KeyCode::Char('a'), m) if m.is_empty() => Action::RequestsApproveSelected,
        (KeyCode::Char('x'), m) if m.is_empty() => Action::RequestsRejectSelected,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::RequestsRefresh,
        _ => Action::Noop,
    }
}

fn translate_follow_list_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::FollowListClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::FollowListClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::FollowListSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::FollowListSelectPrev,
        (KeyCode::Char('t'), m) if m.is_empty() => Action::FollowListToggleMode,
        (KeyCode::Enter, _) => Action::FollowListOpenSelected,
        (KeyCode::Char('o'), m) if m.is_empty() => Action::FollowListLoadMore,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::FollowListRefresh,
        _ => Action::Noop,
    }
}

fn translate_command_key(k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => Action::CommandCancel,
        KeyCode::Enter => Action::CommandSubmit,
        KeyCode::Backspace => Action::CommandBackspace,
        KeyCode::Char(c) if !ctrl => Action::CommandInsertChar(c),
        _ => Action::Noop,
    }
}

fn translate_profile_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::ProfileBack,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::ProfileBack,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::ProfileSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::ProfileSelectPrev,
        (KeyCode::Char('o'), m) if m.is_empty() => Action::ProfileLoadMoreNotes,
        (KeyCode::Char('f'), m) if m.is_empty() => Action::ProfileToggleFollow,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::ProfileRefresh,
        _ => Action::Noop,
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
        // M13 PR4: p = 選択中の Note の author の Profile 画面を push。
        (KeyCode::Char('p'), m) if m.is_empty() => Action::OpenProfileFromSelected,
        // M13 PR5: `:` でコマンドプロンプトを開く (vim 風)。
        (KeyCode::Char(':'), m) if m.is_empty() => Action::OpenCommand,
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
        // **代替送信キー**: `Ctrl-Enter` は Kitty keyboard protocol (CSI u) に
        // 対応した端末でしか modifier 付き KeyEvent として届かない。tmux 経由・
        // xterm / GNOME Terminal / Konsole 等の通常 VT 端末では `\r` のままで
        // 区別不可能なので、それらの環境のために `F2` を恒久的な送信キーとして
        // 用意する (function key はほぼどの端末でも素直に通る)。
        KeyCode::F(2) => Action::SubmitNote,
        KeyCode::Char('w') if ctrl => Action::ToggleCw,
        KeyCode::Char('s') if ctrl => Action::ToggleSensitive,
        KeyCode::Char('v') if ctrl => Action::CycleVisibility,
        // M7: Ctrl-A で添付ピッカを開く。`a` 単独は本文に挿入されるので Ctrl 必須。
        KeyCode::Char('a') if ctrl => Action::OpenPicker(PickerMode::Attachment),
        // M7: Ctrl-D で末尾の添付を 1 件外す (compose に居ながらの取り消し)。
        KeyCode::Char('d') if ctrl => Action::PopAttachment,
        // Issue #101: Ctrl-E で絵文字検索モーダル。compose 本文に :shortcode:
        // を挿入する用途。reaction prompt と同じバインド。
        KeyCode::Char('e') if ctrl => Action::OpenEmojiSearch,
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
        // Issue #101: 絵文字検索モーダルを Ctrl-E で起動。reaction prompt
        // の通常入力には影響させない (= モーダル内に独立した search buffer)。
        KeyCode::Char('e') if ctrl => Action::OpenEmojiSearch,
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

    /// Kitty keyboard protocol 非対応端末向けの代替送信キー。
    #[test]
    fn compose_f2_submits() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)),
                Focus::Compose,
            ),
            Action::SubmitNote,
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
    fn timeline_p_opens_profile() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('p'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::OpenProfileFromSelected,
        ));
    }

    #[test]
    fn profile_focus_keys_route_correctly() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(matches!(
                translate(Event::Key(key(code, KeyModifiers::NONE)), Focus::Profile),
                Action::ProfileBack,
            ));
        }
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileSelectNext,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('k'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileSelectPrev,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('f'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileToggleFollow,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('o'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileLoadMoreNotes,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('r'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileRefresh,
        ));
        // Ctrl-C は Profile でも Quit が勝つ。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Focus::Profile,
            ),
            Action::Quit,
        ));
    }

    #[test]
    fn timeline_colon_opens_command() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char(':'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::OpenCommand,
        ));
    }

    #[test]
    fn command_focus_keys_route_correctly() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)),
                Focus::Command,
            ),
            Action::CommandCancel,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::Command,
            ),
            Action::CommandSubmit,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('f'), KeyModifiers::NONE)),
                Focus::Command,
            ),
            Action::CommandInsertChar('f'),
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Backspace, KeyModifiers::NONE)),
                Focus::Command,
            ),
            Action::CommandBackspace,
        ));
    }

    #[test]
    fn follow_list_focus_keys_route_correctly() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(matches!(
                translate(Event::Key(key(code, KeyModifiers::NONE)), Focus::FollowList),
                Action::FollowListClose,
            ));
        }
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListSelectNext,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('k'), KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListSelectPrev,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('t'), KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListToggleMode,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListOpenSelected,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('o'), KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListLoadMore,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('r'), KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListRefresh,
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
