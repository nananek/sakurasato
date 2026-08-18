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
    /// Issue #286: 全画面を再送する (`terminal.clear()` を挟んで redraw)。
    /// `Ctrl-L` で手動起動。tmux 復帰やモーダル閉じで残った画像残像を消す。
    ForceRedraw,
    SelectNext,
    SelectPrev,
    PageDown,
    PageUp,
    RefreshTimeline,
    LoadMore,
    EnterCompose,
    FocusTimeline,
    ToggleHelp,
    /// Help overlay の 1 行下スクロール (`j` / `↓`)。
    HelpScrollDown,
    /// Help overlay の 1 行上スクロール (`k` / `↑`)。
    HelpScrollUp,
    /// Help overlay の 1 ページ下 (`Space` / `PgDn`)。
    HelpPageDown,
    /// Help overlay の 1 ページ上 (`PgUp`)。
    HelpPageUp,
    /// Help overlay の先頭へ (`g`)。
    HelpScrollTop,
    /// Help overlay の末尾へ (`G`)。
    HelpScrollBottom,
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
    /// `/` ── パス直接入力モードに入る。
    PickerPathOpen,
    /// パス入力中の文字入力。
    PickerPathChar(char),
    /// パス入力中の Backspace。
    PickerPathBackspace,
    /// Tab ── パス補完。
    PickerPathComplete,
    /// Enter ── 入力パスを確定 (descend / select)。
    PickerPathSubmit,
    /// Esc ── パス入力をキャンセルし、通常ブラウズに戻る。
    PickerPathCancel,
    /// M7: 直近の添付を 1 件外す (compose focus 中)。
    PopAttachment,
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
    /// #151: 選択中の Note を renote / boost する。`b` キー / `:renote` コマンド。
    RenoteSelected,
    /// #151: 選択中の Note への自分の renote を取り消す。`B` (Shift-B) /
    /// `:unrenote` コマンド。
    UndoRenoteOnSelected,
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
    /// ユーザーブロック PR6: Profile 画面で `b` ── block / unblock を toggle。
    /// block 実行前は確認オーバーレイ ([`crate::confirm::ConfirmPrompt`]) を
    /// 挟む (破壊的操作、計画書 §5.9)。unblock は即座に実行する。
    ProfileToggleBlock,
    /// ユーザーブロック PR6 / 連合ドメインブロック PR7 共用: 確認オーバーレイ
    /// で `y`/`Enter` ── 確定。
    ConfirmYes,
    /// 確認オーバーレイで `n`/`Esc` ── キャンセル。
    ConfirmNo,
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
    /// Issue #116: コマンドプロンプト中の Tab ── head の前方一致補完。
    /// 1 件なら確定、複数なら最長共通接頭辞まで埋めて候補を表示する。
    CommandComplete,
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
    /// Issue #115: `FollowList` で `PageDown` (or `Ctrl-D`) ── viewport 件数分下へ。
    FollowListPageDown,
    /// Issue #115: `FollowList` で `PageUp` (or `Ctrl-U`) ── viewport 件数分上へ。
    FollowListPageUp,
    /// Issue #118 (Issue #101 後継): 絵文字検索モーダルを開く。
    /// Timeline `e` (= 選択中 Note に即リアクション送信) と Compose `Ctrl-E`
    /// (= 本文 buffer に `:shortcode:` / Unicode 1 字を挿入) の両起動経路で
    /// 共通の Action。モード判定は runtime 側が現在 Focus から行う。
    OpenEmojiSearch,
    /// 絵文字検索モーダル中の `↓` (or `Ctrl-N`) ── 次候補へ。
    EmojiSearchDown,
    /// 絵文字検索モーダル中の `↑` (or `Ctrl-P`) ── 前候補へ。
    EmojiSearchUp,
    /// 絵文字検索モーダル中の `Enter` ── モードに応じて確定。
    /// `Mode::ReactToNote(_)` なら即リアクション送信、`Mode::InsertIntoCompose`
    /// なら compose 本文に `:shortcode:` / Unicode 1 字を挿入する。
    EmojiSearchConfirm,
    /// 絵文字検索モーダル中の `Esc` ── 何もせず閉じる。
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
    /// #206 PR3: `:notifications` / `n` ── in-app 通知一覧画面を push。
    OpenNotifications,
    /// #206 PR3: 通知一覧でカーソル下移動。
    NotificationsSelectNext,
    /// #206 PR3: 通知一覧でカーソル上移動。
    NotificationsSelectPrev,
    /// #206 PR3: 通知一覧で `m` ── 全件既読化。
    NotificationsMarkAllRead,
    /// #206 PR3: 通知一覧で `r` ── 再取得。
    NotificationsRefresh,
    /// #206 PR3: 通知一覧で `Esc` / `q` ── 画面を閉じる。
    NotificationsClose,
    /// Issue #133 (3): Timeline で選択中の Note の詳細モーダルを開く。
    OpenNoteDetail,
    /// Issue #133 (3): 詳細モーダルを閉じる (Esc / q)。
    NoteDetailClose,
    /// Issue #133 (3): モーダル本文を 1 行下スクロール (j / Down)。
    NoteDetailScrollDown,
    /// Issue #133 (3): モーダル本文を 1 行上スクロール (k / Up)。
    NoteDetailScrollUp,
    /// Issue #133 (4): モーダル内で次の添付プレビューに切替 (n / Right)。
    NoteDetailNextAttachment,
    /// Issue #133 (4): モーダル内で前の添付プレビューに切替 (p / Left)。
    NoteDetailPrevAttachment,
    /// Issue #133 (4): モーダル内で現在の添付の sensitive blur を toggle (s)。
    NoteDetailToggleReveal,
    /// リスト機能: `:lists` ── 一覧画面を push。
    OpenLists,
    /// リスト機能: `:home` ── 表示中タイムラインを home に戻す。
    HomeTimeline,
    /// リスト機能: 一覧 / メンバー一覧でカーソル下移動 (`j`/`Down`)。
    ListsSelectNext,
    /// リスト機能: 一覧 / メンバー一覧でカーソル上移動 (`k`/`Up`)。
    ListsSelectPrev,
    /// リスト機能: 一覧で `Enter` ── 選択中リストのタイムラインに切替。
    ListsEnter,
    /// リスト機能: 一覧で `m` ── 選択中リストのメンバー一覧を開く。
    ListsOpenMembers,
    /// リスト機能: 一覧で `n` ── 新規リスト作成 (タイトル入力 overlay)。
    ListsNew,
    /// リスト機能: 一覧で `R` ── 選択中リストをリネーム (タイトル入力 overlay)。
    ListsRename,
    /// リスト機能: 一覧で `d` ── 選択中リストを削除。
    ListsDelete,
    /// リスト機能: メンバー一覧で `a` ── acct 入力 overlay を開く。
    ListsMemberAdd,
    /// リスト機能: メンバー一覧で `x` ── 選択中メンバーを削除。
    ListsMemberRemove,
    /// リスト機能: `r` ── 再取得 (一覧 / メンバー一覧どちらでも)。
    ListsRefresh,
    /// リスト機能: `Esc`/`q` ── メンバー一覧なら一覧へ戻る、一覧なら画面を閉じる。
    ListsClose,
    /// リスト機能: タイトル/acct 入力 overlay 中の文字入力。
    ListsInputChar(char),
    /// リスト機能: 入力 overlay 中の Backspace。
    ListsInputBackspace,
    /// リスト機能: 入力 overlay の確定 (`Enter`)。
    ListsInputSubmit,
    /// リスト機能: 入力 overlay のキャンセル (`Esc`)。
    ListsInputCancel,
    /// 絵文字管理画面: `:emojis` ── 画面を push。
    OpenEmojiAdmin,
    /// 絵文字管理画面: `t` ── Local/Remote タブ切替。
    EmojiAdminToggleTab,
    /// 絵文字管理画面: `j`/`Down` ── 現在タブでカーソル下移動。
    EmojiAdminSelectNext,
    /// 絵文字管理画面: `k`/`Up` ── 現在タブでカーソル上移動。
    EmojiAdminSelectPrev,
    /// 絵文字管理画面: `r` ── 現在タブを再取得。
    EmojiAdminRefresh,
    /// 絵文字管理画面 (Local タブ): `i` ── zip インポート用ファイルピッカを開く。
    EmojiAdminStartImport,
    /// 絵文字管理画面 (Remote タブ): `Enter` ── 選択中のリモート絵文字を
    /// 即座にローカルへコピー (確認プロンプト無し、リネーム無し)。
    EmojiAdminCopySelected,
    /// 絵文字管理画面: `Esc`/`q` ── 画面を閉じる。
    EmojiAdminClose,
    /// 絵文字管理画面: `/` ── 検索窓を開く。
    EmojiAdminSearchOpen,
    /// 絵文字管理画面: 検索窓中の文字入力。
    EmojiAdminSearchChar(char),
    /// 絵文字管理画面: 検索窓中の Backspace。
    EmojiAdminSearchBackspace,
    /// 絵文字管理画面: 検索窓の確定 (`Enter`)。
    EmojiAdminSearchSubmit,
    /// 絵文字管理画面: 検索窓のキャンセル (`Esc`)。
    EmojiAdminSearchCancel,
    /// 連合ドメインブロック PR7: `:domains` ── ドメイン一覧画面を開く。
    OpenDomainAdmin,
    /// `DomainAdmin` 一覧で `j`/`Down` ── カーソル下移動。
    DomainAdminSelectNext,
    /// `DomainAdmin` 一覧で `k`/`Up` ── カーソル上移動。
    DomainAdminSelectPrev,
    /// `DomainAdmin` 一覧で `Enter` ── 選択中ホストの詳細画面を開く。
    DomainAdminOpenSelected,
    /// `DomainAdmin` 一覧で `r` ── 再取得。
    DomainAdminRefresh,
    /// `DomainAdmin` 一覧で `Esc`/`q` ── 画面を閉じる。
    DomainAdminClose,
    /// `DomainDetail` で `j`/`Down` ── 現在タブでカーソル下移動。
    DomainDetailSelectNext,
    /// `DomainDetail` で `k`/`Up` ── 現在タブでカーソル上移動。
    DomainDetailSelectPrev,
    /// `DomainDetail` で `t` ── following/followers タブ切替。
    DomainDetailToggleTab,
    /// `DomainDetail` で `Enter` ── 選択中 actor の Profile を push。
    DomainDetailOpenSelected,
    /// `DomainDetail` で `s` ── silence を toggle (既に silence なら unset)。
    DomainDetailToggleSilence,
    /// `DomainDetail` で `x` ── suspend を実行 (確認オーバーレイを挟む)。
    DomainDetailSuspend,
    /// `DomainDetail` で `u` ── 措置解除。
    DomainDetailUnset,
    /// `DomainDetail` で `r` ── 再取得。
    DomainDetailRefresh,
    /// `DomainDetail` で `Esc`/`q` ── `DomainAdmin` 一覧へ戻る。
    DomainDetailClose,
}

/// crossterm イベント → Action。
#[allow(
    clippy::needless_pass_by_value,
    reason = "値で渡す `Event` を tests でも自然に書きたい"
)]
pub fn translate(event: Event, focus: Focus) -> Action {
    translate_with_context(event, focus, false, false, false)
}

/// [`translate`] の拡張版。`lists_input_active` は `Focus::Lists` 中に
/// [`crate::lists::ListsScreen::input`] が `Some` かどうか (= タイトル/acct
/// 入力 overlay 中は文字キーを全部テキスト入力として扱う必要があり、
/// `translate_lists_key` だけ呼び出し側の app 状態を要求するため)。
/// `picker_path_input_active` は同じ理由で `Focus::Picker` 中に
/// [`crate::picker::FilePicker::path_input`] が `Some` かどうか (=
/// `/` で開いたパス直接入力中は文字キーを全部テキスト入力として扱う)。
/// `emoji_admin_search_active` は `Focus::EmojiAdmin` 中に
/// [`crate::emoji_admin::EmojiAdminScreen::query_input`] が `Some` かどうか。
/// それぞれ対応する focus 以外では無視される。
#[allow(
    clippy::needless_pass_by_value,
    reason = "値で渡す `Event` を tests でも自然に書きたい"
)]
pub fn translate_with_context(
    event: Event,
    focus: Focus,
    lists_input_active: bool,
    picker_path_input_active: bool,
    emoji_admin_search_active: bool,
) -> Action {
    match event {
        Event::Key(k) => translate_key(
            k,
            focus,
            lists_input_active,
            picker_path_input_active,
            emoji_admin_search_active,
        ),
        Event::Mouse(m) => translate_mouse(m),
        Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
            Action::Noop
        }
    }
}

fn translate_key(
    k: KeyEvent,
    focus: Focus,
    lists_input_active: bool,
    picker_path_input_active: bool,
    emoji_admin_search_active: bool,
) -> Action {
    if k.kind == KeyEventKind::Release {
        return Action::Noop;
    }
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    // Issue #286: Ctrl-L はどの focus でも全画面再描画 (端末が汚れたときの
    // 手動回復)。printable でないので compose のテキスト入力とも衝突しない。
    if k.code == KeyCode::Char('l') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::ForceRedraw;
    }
    match focus {
        Focus::Timeline => translate_timeline_key(k),
        Focus::Compose => translate_compose_key(k),
        Focus::Help => translate_help_key(k),
        Focus::Picker => translate_picker_key(k, picker_path_input_active),
        Focus::Suppression => translate_suppression_key(k),
        Focus::AltPrompt => translate_alt_prompt_key(k),
        Focus::Profile => translate_profile_key(k),
        Focus::FollowList => translate_follow_list_key(k),
        Focus::Command => translate_command_key(k),
        Focus::Requests => translate_requests_key(k),
        Focus::Notifications => translate_notifications_key(k),
        Focus::EmojiSearch => translate_emoji_search_key(k),
        Focus::NoteDetail => translate_note_detail_key(k),
        Focus::Lists => translate_lists_key(k, lists_input_active),
        Focus::EmojiAdmin => translate_emoji_admin_key(k, emoji_admin_search_active),
        Focus::ConfirmPrompt => translate_confirm_key(k),
        Focus::DomainAdmin => translate_domain_admin_key(k),
        Focus::DomainDetail => translate_domain_detail_key(k),
    }
}

/// Issue #133 (3): Note 詳細モーダル中のキー操作。
fn translate_note_detail_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::NoteDetailClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::NoteDetailClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::NoteDetailScrollDown,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::NoteDetailScrollUp,
        // 添付プレビュー切替: n / p は添付が無い note では noop。Right / Left
        // も同義に割り当てる ── 矢印キーだけで一通り操作できる。
        (KeyCode::Char('n') | KeyCode::Right, m) if m.is_empty() => {
            Action::NoteDetailNextAttachment
        }
        (KeyCode::Char('p') | KeyCode::Left, m) if m.is_empty() => Action::NoteDetailPrevAttachment,
        // sensitive blur 解除 toggle。
        (KeyCode::Char('s'), m) if m.is_empty() => Action::NoteDetailToggleReveal,
        _ => Action::Noop,
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

/// リスト機能のキー操作。`input_active` (= [`crate::lists::ListsScreen::input`]
/// が `Some`) のときはタイトル/acct 入力 overlay 中なので、`q`/`n`/`d` などの
/// 文字も全部テキスト入力として扱う (= コマンドキーとして横取りしない)。
fn translate_lists_key(k: KeyEvent, input_active: bool) -> Action {
    if input_active {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        return match k.code {
            KeyCode::Esc => Action::ListsInputCancel,
            KeyCode::Enter => Action::ListsInputSubmit,
            KeyCode::Backspace => Action::ListsInputBackspace,
            KeyCode::Char(c) if !ctrl => Action::ListsInputChar(c),
            _ => Action::Noop,
        };
    }
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::ListsClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::ListsClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::ListsSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::ListsSelectPrev,
        (KeyCode::Enter, _) => Action::ListsEnter,
        (KeyCode::Char('m'), m) if m.is_empty() => Action::ListsOpenMembers,
        (KeyCode::Char('n'), m) if m.is_empty() => Action::ListsNew,
        (KeyCode::Char('R'), _) => Action::ListsRename,
        (KeyCode::Char('d'), m) if m.is_empty() => Action::ListsDelete,
        (KeyCode::Char('a'), m) if m.is_empty() => Action::ListsMemberAdd,
        (KeyCode::Char('x'), m) if m.is_empty() => Action::ListsMemberRemove,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::ListsRefresh,
        _ => Action::Noop,
    }
}

/// 絵文字管理画面のキー操作。`search_active` (=
/// [`crate::emoji_admin::EmojiAdminScreen::query_input`] が `Some`) のときは
/// 検索窓入力中なので、`j`/`k`/`t`/`r` などの文字も全部テキスト入力として
/// 扱う (= `translate_lists_key` と同じ設計)。
fn translate_emoji_admin_key(k: KeyEvent, search_active: bool) -> Action {
    if search_active {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        return match k.code {
            KeyCode::Esc => Action::EmojiAdminSearchCancel,
            KeyCode::Enter => Action::EmojiAdminSearchSubmit,
            KeyCode::Backspace => Action::EmojiAdminSearchBackspace,
            KeyCode::Char(c) if !ctrl => Action::EmojiAdminSearchChar(c),
            _ => Action::Noop,
        };
    }
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::EmojiAdminClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::EmojiAdminClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::EmojiAdminSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::EmojiAdminSelectPrev,
        (KeyCode::Char('t'), m) if m.is_empty() => Action::EmojiAdminToggleTab,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::EmojiAdminRefresh,
        (KeyCode::Char('i'), m) if m.is_empty() => Action::EmojiAdminStartImport,
        (KeyCode::Char('/'), m) if m.is_empty() => Action::EmojiAdminSearchOpen,
        (KeyCode::Enter, _) => Action::EmojiAdminCopySelected,
        _ => Action::Noop,
    }
}

/// #206 PR3: 通知一覧画面のキー操作。
fn translate_notifications_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::NotificationsClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::NotificationsClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::NotificationsSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::NotificationsSelectPrev,
        (KeyCode::Char('m'), m) if m.is_empty() => Action::NotificationsMarkAllRead,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::NotificationsRefresh,
        _ => Action::Noop,
    }
}

fn translate_follow_list_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::FollowListClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::FollowListClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::FollowListSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::FollowListSelectPrev,
        (KeyCode::PageDown, _) => Action::FollowListPageDown,
        (KeyCode::PageUp, _) => Action::FollowListPageUp,
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => Action::FollowListPageDown,
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => Action::FollowListPageUp,
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
        // Issue #116: Tab で head の前方一致補完。
        KeyCode::Tab => Action::CommandComplete,
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
        (KeyCode::Char('b'), m) if m.is_empty() => Action::ProfileToggleBlock,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::ProfileRefresh,
        _ => Action::Noop,
    }
}

/// ユーザーブロック PR6 / 連合ドメインブロック PR7 共用の確認オーバーレイ。
fn translate_confirm_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Char('y'), m) | (KeyCode::Enter, m) if m.is_empty() => Action::ConfirmYes,
        (KeyCode::Char('n'), m) if m.is_empty() => Action::ConfirmNo,
        (KeyCode::Esc, _) => Action::ConfirmNo,
        _ => Action::Noop,
    }
}

/// 連合ドメインブロック PR7: ドメイン一覧画面。
fn translate_domain_admin_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::DomainAdminClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::DomainAdminClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::DomainAdminSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::DomainAdminSelectPrev,
        (KeyCode::Enter, m) if m.is_empty() => Action::DomainAdminOpenSelected,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::DomainAdminRefresh,
        _ => Action::Noop,
    }
}

/// 連合ドメインブロック PR7: ドメイン詳細画面。
fn translate_domain_detail_key(k: KeyEvent) -> Action {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => Action::DomainDetailClose,
        (KeyCode::Char('q'), m) if m.is_empty() => Action::DomainDetailClose,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::DomainDetailSelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::DomainDetailSelectPrev,
        (KeyCode::Char('t'), m) if m.is_empty() => Action::DomainDetailToggleTab,
        (KeyCode::Enter, m) if m.is_empty() => Action::DomainDetailOpenSelected,
        (KeyCode::Char('s'), m) if m.is_empty() => Action::DomainDetailToggleSilence,
        (KeyCode::Char('x'), m) if m.is_empty() => Action::DomainDetailSuspend,
        (KeyCode::Char('u'), m) if m.is_empty() => Action::DomainDetailUnset,
        (KeyCode::Char('r'), m) if m.is_empty() => Action::DomainDetailRefresh,
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
        // #206 PR3: `N` で通知一覧を開く (`n` は compose なので大文字)。
        (KeyCode::Char('N'), _) => Action::OpenNotifications,
        (KeyCode::Char('t'), m) if m.is_empty() => Action::CycleTheme,
        // M7: A = avatar, H = header, ; = attachment ─ いずれもファイル
        // ピッカを当該モードで開く。小文字キーは timeline ナビと衝突
        // するため Shift 付き / `;` を採用。
        (KeyCode::Char('A'), _) => Action::OpenPicker(PickerMode::Avatar),
        (KeyCode::Char('H'), _) => Action::OpenPicker(PickerMode::Header),
        (KeyCode::Char(';'), m) if m.is_empty() => Action::OpenPicker(PickerMode::Attachment),
        // Issue #118: e = 絵文字検索モーダルを直起動 (= 旧 reaction prompt
        // 経路は廃止)。選択中 Note への即リアクションを `ReactToNote` mode
        // で送る。
        (KeyCode::Char('e'), m) if m.is_empty() => Action::OpenEmojiSearch,
        // M9 PR2: i = 視覚刺激抑制 overlay を開く ("images" の頭文字)。
        // Compose 中は `i` が本文に挿入されるので timeline focus 限定。
        (KeyCode::Char('i'), m) if m.is_empty() => Action::ToggleSuppression,
        // M13 PR6: R = 返信 (大文字 ── refresh `r` と衝突しないように)。
        (KeyCode::Char('R'), _) => Action::ReplyToSelected,
        // M13 PR6: u = 自分が直近に付けた reaction を取り消し。
        (KeyCode::Char('u'), m) if m.is_empty() => Action::UndoReactionOnSelected,
        // #151: b = renote (boost / 引用なし), B = undo renote。
        // reaction の `e` / `u` と並ぶキー設計。`B` は Shift 必須で誤爆を抑える。
        (KeyCode::Char('b'), m) if m.is_empty() => Action::RenoteSelected,
        (KeyCode::Char('B'), _) => Action::UndoRenoteOnSelected,
        // M13 PR4: p = 選択中の Note の author の Profile 画面を push。
        (KeyCode::Char('p'), m) if m.is_empty() => Action::OpenProfileFromSelected,
        // M13 PR5: `:` でコマンドプロンプトを開く (vim 風)。
        (KeyCode::Char(':'), m) if m.is_empty() => Action::OpenCommand,
        // Issue #133 (3): Enter で選択中 Note の詳細モーダルを開く。
        (KeyCode::Enter, m) if m.is_empty() => Action::OpenNoteDetail,
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
    // 単独 `g` で先頭、Shift+`g` (= `G`) で末尾 ── less / vim 慣習。
    // `?` / `q` / Esc は従来どおり overlay クローズ。
    match (k.code, k.modifiers) {
        (KeyCode::Esc | KeyCode::Char('?' | 'q'), _) => Action::ToggleHelp,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::HelpScrollDown,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::HelpScrollUp,
        (KeyCode::Char(' ') | KeyCode::PageDown, _) => Action::HelpPageDown,
        (KeyCode::PageUp, _) => Action::HelpPageUp,
        (KeyCode::Char('g'), m) if !m.contains(KeyModifiers::SHIFT) => Action::HelpScrollTop,
        (KeyCode::Char('G'), _) => Action::HelpScrollBottom,
        _ => Action::Noop,
    }
}

fn translate_picker_key(k: KeyEvent, path_input_active: bool) -> Action {
    if path_input_active {
        // [`crate::lists`] のタイトル/acct 入力と同じパターン: 開いている
        // 間は文字キーを全部テキスト入力として扱い、j/k 等の一覧ナビゲーション
        // には回さない。
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        return match k.code {
            KeyCode::Esc => Action::PickerPathCancel,
            KeyCode::Enter => Action::PickerPathSubmit,
            KeyCode::Backspace => Action::PickerPathBackspace,
            KeyCode::Tab => Action::PickerPathComplete,
            KeyCode::Char(c) if !ctrl => Action::PickerPathChar(c),
            _ => Action::Noop,
        };
    }
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
        // `/` でパス直接入力モードへ。
        (KeyCode::Char('/'), m) if m.is_empty() => Action::PickerPathOpen,
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
    fn timeline_e_opens_emoji_search() {
        // Issue #118: 旧 reaction prompt 経路は廃止、`e` で直接モーダル起動。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('e'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::OpenEmojiSearch,
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
    fn timeline_b_renotes_selected() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('b'), KeyModifiers::NONE)),
                Focus::Timeline,
            ),
            Action::RenoteSelected,
        ));
    }

    #[test]
    fn timeline_capital_b_undoes_renote() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('B'), KeyModifiers::SHIFT)),
                Focus::Timeline,
            ),
            Action::UndoRenoteOnSelected,
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
                Event::Key(key(KeyCode::Char('b'), KeyModifiers::NONE)),
                Focus::Profile,
            ),
            Action::ProfileToggleBlock,
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
        // Issue #116: Tab → CommandComplete。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Tab, KeyModifiers::NONE)),
                Focus::Command,
            ),
            Action::CommandComplete,
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
        // Issue #115: PageDown / PageUp + Ctrl-D / Ctrl-U で page 単位ジャンプ。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::PageDown, KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListPageDown,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::PageUp, KeyModifiers::NONE)),
                Focus::FollowList,
            ),
            Action::FollowListPageUp,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                Focus::FollowList,
            ),
            Action::FollowListPageDown,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
                Focus::FollowList,
            ),
            Action::FollowListPageUp,
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

    // Help overlay 上の scroll キー routing。
    // less / vim 慣習に倣う ── j/k で 1 行、Space/PgDn で 1 ページ、g/G で
    // 先頭/末尾。Esc / ? / q は従来どおり close。

    #[test]
    fn help_j_scrolls_down_one_line() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpScrollDown,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Down, KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpScrollDown,
        ));
    }

    #[test]
    fn help_k_scrolls_up_one_line() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('k'), KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpScrollUp,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Up, KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpScrollUp,
        ));
    }

    #[test]
    fn help_space_and_pgdn_page_down() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpPageDown,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::PageDown, KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpPageDown,
        ));
    }

    #[test]
    fn help_pgup_pages_up() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::PageUp, KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpPageUp,
        ));
    }

    #[test]
    fn help_g_goes_to_top_capital_g_goes_to_bottom() {
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('g'), KeyModifiers::NONE)),
                Focus::Help,
            ),
            Action::HelpScrollTop,
        ));
        // crossterm は Shift+'g' を `Char('G')` + SHIFT で配ってくる。
        // SHIFT 修飾子の有無に依存しない判定 (= Char('G') を見る) に倒している。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
                Focus::Help,
            ),
            Action::HelpScrollBottom,
        ));
    }

    #[test]
    fn help_q_question_esc_still_close() {
        for code in [KeyCode::Esc, KeyCode::Char('?'), KeyCode::Char('q')] {
            assert!(matches!(
                translate(Event::Key(key(code, KeyModifiers::NONE)), Focus::Help),
                Action::ToggleHelp,
            ));
        }
    }

    #[test]
    fn ctrl_l_forces_redraw_in_any_focus() {
        // Issue #286: Ctrl-L はどの focus でも全画面再描画に倒す (端末が汚れた
        // ときの手動回復)。compose 中でも printable でないのでテキストと衝突しない。
        for focus in [Focus::Timeline, Focus::Compose, Focus::NoteDetail] {
            assert!(matches!(
                translate(
                    Event::Key(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
                    focus,
                ),
                Action::ForceRedraw,
            ));
        }
    }

    // ─── picker: 通常ブラウズ / パス直接入力 ────────────────────────────

    #[test]
    fn picker_slash_opens_path_input() {
        assert!(matches!(
            translate_picker_key(key(KeyCode::Char('/'), KeyModifiers::NONE), false),
            Action::PickerPathOpen,
        ));
    }

    #[test]
    fn picker_normal_mode_keys_unchanged() {
        assert!(matches!(
            translate_picker_key(key(KeyCode::Char('j'), KeyModifiers::NONE), false),
            Action::PickerNext,
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Enter, KeyModifiers::NONE), false),
            Action::PickerActivate,
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Esc, KeyModifiers::NONE), false),
            Action::PickerCancel,
        ));
    }

    #[test]
    fn picker_path_input_mode_captures_text_keys() {
        // path input 中は j/k のようなナビゲーションキーもテキストとして
        // 入力される (= lists のタイトル入力と同じ挙動)。
        assert!(matches!(
            translate_picker_key(key(KeyCode::Char('j'), KeyModifiers::NONE), true),
            Action::PickerPathChar('j'),
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Char('/'), KeyModifiers::NONE), true),
            Action::PickerPathChar('/'),
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Backspace, KeyModifiers::NONE), true),
            Action::PickerPathBackspace,
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Tab, KeyModifiers::NONE), true),
            Action::PickerPathComplete,
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Enter, KeyModifiers::NONE), true),
            Action::PickerPathSubmit,
        ));
        assert!(matches!(
            translate_picker_key(key(KeyCode::Esc, KeyModifiers::NONE), true),
            Action::PickerPathCancel,
        ));
    }

    #[test]
    fn picker_path_input_mode_ignores_ctrl_chars() {
        // Ctrl-C はグローバルガードで Quit に化けるので translate_picker_key
        // には来ないが、他の Ctrl 組み合わせ (テキストではない) は Noop。
        assert!(matches!(
            translate_picker_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL), true),
            Action::Noop,
        ));
    }

    #[test]
    fn translate_with_context_routes_picker_path_input_flag() {
        // `translate` (= context 無し) は常に false 扱いで通常ブラウズになる。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::Picker,
            ),
            Action::PickerNext,
        ));
        assert!(matches!(
            translate_with_context(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::Picker,
                false,
                true,
                false,
            ),
            Action::PickerPathChar('j'),
        ));
    }

    #[test]
    fn translate_with_context_routes_emoji_admin_search_flag() {
        // 検索窓非アクティブ: `j` はカーソル移動。
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::EmojiAdmin,
            ),
            Action::EmojiAdminSelectNext,
        ));
        // 検索窓アクティブ: `j` はテキスト入力。
        assert!(matches!(
            translate_with_context(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::EmojiAdmin,
                false,
                false,
                true,
            ),
            Action::EmojiAdminSearchChar('j'),
        ));
    }

    #[test]
    fn confirm_prompt_keys_route_correctly() {
        for code in [KeyCode::Char('y'), KeyCode::Enter] {
            assert!(matches!(
                translate(
                    Event::Key(key(code, KeyModifiers::NONE)),
                    Focus::ConfirmPrompt,
                ),
                Action::ConfirmYes,
            ));
        }
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            assert!(matches!(
                translate(
                    Event::Key(key(code, KeyModifiers::NONE)),
                    Focus::ConfirmPrompt,
                ),
                Action::ConfirmNo,
            ));
        }
    }

    #[test]
    fn domain_admin_keys_route_correctly() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(matches!(
                translate(
                    Event::Key(key(code, KeyModifiers::NONE)),
                    Focus::DomainAdmin,
                ),
                Action::DomainAdminClose,
            ));
        }
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('j'), KeyModifiers::NONE)),
                Focus::DomainAdmin,
            ),
            Action::DomainAdminSelectNext,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::DomainAdmin,
            ),
            Action::DomainAdminOpenSelected,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('r'), KeyModifiers::NONE)),
                Focus::DomainAdmin,
            ),
            Action::DomainAdminRefresh,
        ));
    }

    #[test]
    fn domain_detail_keys_route_correctly() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(matches!(
                translate(
                    Event::Key(key(code, KeyModifiers::NONE)),
                    Focus::DomainDetail,
                ),
                Action::DomainDetailClose,
            ));
        }
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('t'), KeyModifiers::NONE)),
                Focus::DomainDetail,
            ),
            Action::DomainDetailToggleTab,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('s'), KeyModifiers::NONE)),
                Focus::DomainDetail,
            ),
            Action::DomainDetailToggleSilence,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
                Focus::DomainDetail,
            ),
            Action::DomainDetailSuspend,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Char('u'), KeyModifiers::NONE)),
                Focus::DomainDetail,
            ),
            Action::DomainDetailUnset,
        ));
        assert!(matches!(
            translate(
                Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Focus::DomainDetail,
            ),
            Action::DomainDetailOpenSelected,
        ));
    }
}
