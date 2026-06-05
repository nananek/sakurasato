//! TUI 全体のアプリ状態。
//!
//! - **タイムライン**: `notes` を `id` 降順 (= 新しい順) で保持。SSE で来た
//!   `note.created` は先頭に挿入する。
//! - **focus**: タイムライン / 投稿エディタ / ヘルプ画面の 3 値。
//! - **`status_line`**: 一時メッセージ (投稿成功、エラー、ロード中) を 1 行表示。
//! - **scroll**: タイムラインの先頭から表示開始するインデックス (`top`)。
//! - **selected**: ハイライトされている note の index (`top` 以上)。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::client::{NoteCreatedPayload, TimelineNote, Whoami};
use crate::compose::{Compose, LastComposeDefaults};
use crate::image_cache::ImageCache;
use crate::suppression::ImageSuppression;
use crate::theme::Theme;

/// status line に表示するメッセージの種別。テーマの色 (success / warning /
/// error / accent) と対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub text: String,
    pub kind: StatusKind,
    pub shown_at: Instant,
    /// 何秒後に自動消去するか。`None` だと残り続ける。
    pub ttl: Option<Duration>,
}

impl StatusLine {
    pub fn expired(&self) -> bool {
        match self.ttl {
            Some(ttl) => self.shown_at.elapsed() > ttl,
            None => false,
        }
    }
}

/// Help overlay のスクロール state。content が overlay 高さを超えるとき、
/// `scroll` で先頭から何行スキップして描画するかを覚える。
///
/// `last_total_lines` / `last_inner_height` は renderer が毎フレーム書き込み、
/// 次回イベント (= `PgDn` / `G` / 末尾クランプ) で参照する ── overlay が描画
/// される前にキーが来ても破綻しないよう default はすべて `0`。`page_step()`
/// 側で `.max(1)` を入れ、`last_inner_height = 0` でも 1 行は進めるようにする。
#[derive(Debug, Clone, Copy, Default)]
pub struct HelpState {
    /// 上から何行スキップして描画するか。
    pub scroll: u16,
    /// 直近 render 時のコンテンツ総行数 (= `lines.len()`)。
    pub last_total_lines: u16,
    /// 直近 render 時の overlay inner 高さ (`PgDn` の step に使う)。
    pub last_inner_height: u16,
}

impl HelpState {
    /// 上限内に `scroll` を保ちつつ `n` 行下にスクロール。
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll());
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn scroll_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// 描画時に viewport / total を反映し、scroll をクランプする。
    pub fn sync_geometry(&mut self, total_lines: u16, inner_height: u16) {
        self.last_total_lines = total_lines;
        self.last_inner_height = inner_height;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    #[must_use]
    pub fn max_scroll(&self) -> u16 {
        self.last_total_lines.saturating_sub(self.last_inner_height)
    }

    /// `PgDn` / `Space` のステップ。viewport 全部だと文脈を失うので 1 行残す。
    #[must_use]
    pub fn page_step(&self) -> u16 {
        self.last_inner_height.saturating_sub(1).max(1)
    }
}

/// 入力フォーカス。マウスクリックでも切り替えられる ([`crate::event`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Timeline,
    Compose,
    Help,
    /// M7: ファイルピッカ。`App::picker` が `Some` のときだけ取りうる。
    Picker,
    /// M9 PR2: 視覚刺激抑制トグル overlay。各要素 (avatar / attachment /
    /// emoji / preview / animation) を on/off できる。
    Suppression,
    /// M13 PR6: 添付アップロード時の alt text 入力プロンプト。
    /// `App::alt_prompt` が `Some` のときのみ取りうる。
    AltPrompt,
    /// M13 PR4 (Issue #79): リモート / ローカル actor の Profile 画面。
    /// `App::profile_stack` の末尾が描画対象。空 stack で Profile に
    /// 入ったままになることは無い (= push と focus 切替を 1 セットで行う)。
    Profile,
    /// M13 PR5 (Issue #79): 自分の following / followers 一覧画面。
    /// `App::follow_list` が `Some` のときのみ取りうる。タブは画面内 `t` で
    /// 切替。Enter で Profile を push して [`Self::Profile`] に遷移する。
    FollowList,
    /// M13 PR5 (Issue #79): vim 風コマンドプロンプト `:` 入力中。
    /// `App::command` が `Some` のときのみ取りうる。
    Command,
    /// M12 (Issue #66): 鍵アカ運用の承認待ち follow 一覧画面。
    /// `:requests` で開く。`App::follow_requests` が `Some` のときのみ取りうる。
    Requests,
    /// #206 PR3: in-app 通知一覧画面。`:notifications` / `n` で開く。
    /// `App::notifications` が `Some` のときのみ取りうる。Esc / q で閉じる。
    Notifications,
    /// Issue #118 (Issue #101 後継): 絵文字検索モーダル。Timeline `e` で「選択
    /// 中 Note への即リアクション」モード、Compose `Ctrl-E` で「本文への
    /// 挿入」モードとして起動。`App::emoji_suggest` が `Some` のときのみ
    /// 取りうる。Esc で起動元に戻る (= Timeline か Compose、`mode` 由来)。
    EmojiSearch,
    /// Issue #133 (3): Note 詳細モーダル。Timeline `Enter` で開く。
    /// `App::note_detail` が `Some` のときのみ取りうる。Esc / q で閉じる。
    NoteDetail,
}

#[derive(Debug)]
pub struct App {
    pub theme: Theme,
    pub whoami: Whoami,
    pub notes: Vec<TimelineNote>,
    pub selected: usize,
    /// 表示開始 index。スクロール時に動かす。
    pub top: usize,
    /// `before_id` ベースのカーソル。次ページ取得に使う。
    pub next_before_id: Option<i64>,
    /// 追加読み込みが終わったかどうか (= サーバから空配列が返ったら true)。
    pub timeline_exhausted: bool,
    pub focus: Focus,
    pub compose: Compose,
    pub status: Option<StatusLine>,
    /// True なら次の draw loop で抜ける。
    pub should_quit: bool,
    /// 接続先 socket (status bar 表示用)。
    pub socket_label: String,
    /// 画像 (アバター) キャッシュ。`Picker` 取得失敗時は無効化された Cache が
    /// 入る (= ensure / get が no-op になり、UI もテキスト専用に落ちる)。
    pub images: ImageCache,
    /// M7: ファイルピッカ。`Focus::Picker` 中のみ表示される。
    pub picker: Option<crate::picker::FilePicker>,
    /// M7: ローカル画像プレビューキャッシュ (picker 表示時に使用)。
    pub previews: crate::preview::PreviewCache,
    /// M7: 進行中のアップロードジョブ数。0 でも picker を閉じてよい。
    /// UI のステータスバーに `↑ N` として出す。
    pub pending_uploads: u32,
    /// M9 PR2: 視覚刺激抑制 (要素別 on/off)。`avatar`/`attachment`/`emoji`/
    /// `preview`/`animation` のフラグセット。`ImageCache` / `PreviewCache`
    /// の `enabled()` と組で見られる ── どちらかが off なら描画パスを抜く。
    pub suppression: ImageSuppression,
    /// M9 PR2: suppression overlay 上のカーソル位置。`Focus::Suppression` で
    /// 開く。`Element::all()` の index。
    pub suppression_cursor: usize,
    /// M13 PR6: 添付アップロード時の alt text 入力プロンプト。
    /// `Focus::AltPrompt` 中のみ表示。
    pub alt_prompt: Option<crate::alt_prompt::AltPrompt>,
    /// M13 PR6: 自分が直近に付けたリアクションの id を `note_id` 別に覚える。
    /// `u` (undo reaction) で `DELETE /api/v1/reactions/{id}` に渡す。
    /// TUI 再起動で消える ── 永続性は不要 (= サーバが真実、TUI はキャッシュ
    /// に過ぎない)。
    pub last_reaction_ids: HashMap<i64, i64>,
    /// #151: 自分が直近に renote した announce の id を `note_id` 別に覚える。
    /// `B` (undo renote) は server 側 `(note_id, local_actor.id)` で引けるので
    /// この map 自体は必須ではないが、status bar に「renote 済み」を一目で
    /// 出す UI hint として保持する (= `last_reaction_ids` と同パターン)。
    /// `TimelineNote.viewer_renoted` が真実源で、起動直後はサーバ応答が反映
    /// されるまで空。
    pub last_renote_ids: HashMap<i64, i64>,
    /// M13 PR4 (Issue #79): Profile 画面 stack。末尾が現在描画中の Profile。
    /// `p` で push、`Esc`/`q` で pop。空 stack + `Focus::Profile` は許されない
    /// (= `apply_action` 側で焦点を Timeline に戻す責務)。
    pub profile_stack: Vec<crate::profile::ProfileScreen>,
    /// M13 PR5 (Issue #79): `FollowList` 画面 state。`:following` / `:followers`
    /// で開く。`Esc` / `q` で `None` に戻し、Profile 同様 stack 風に扱える。
    /// PR5 では following と followers が排他なので 1 件で足りる。
    pub follow_list: Option<crate::follow_list::FollowListScreen>,
    /// M13 PR5 (Issue #79): `:` プロンプトの入力 state。`Focus::Command` の
    /// あいだだけ `Some`。Esc キャンセル / Enter で実行。
    pub command: Option<crate::command::CommandPrompt>,
    /// M12 (Issue #66): 承認待ち follow 一覧画面の state。`:requests` で開く。
    /// `Focus::Requests` のあいだだけ `Some`。
    pub follow_requests: Option<crate::follow_requests::FollowRequestsScreen>,
    /// #206 PR3: in-app 通知一覧画面の state。`:notifications` / `n` で開く。
    /// `Focus::Notifications` のあいだだけ `Some`。
    pub notifications: Option<crate::notifications::NotificationsScreen>,
    /// Issue #118 (Issue #101 後継): 絵文字検索モーダルの state。Timeline `e`
    /// では `Mode::ReactToNote(note_id)`、Compose `Ctrl-E` では
    /// `Mode::InsertIntoCompose` で開く。閉じたときの戻り先 Focus は
    /// `mode` から決まる (= `ReactToNote` → Timeline、`InsertIntoCompose`
    /// → Compose)。検索 buffer は独立。
    pub emoji_suggest: Option<crate::emoji_suggest::EmojiSuggestState>,
    /// Issue #133 (3): Note 詳細モーダルの state。Timeline で `Enter` を
    /// 押した瞬間の Note snapshot を保持する。`Focus::NoteDetail` の
    /// あいだだけ `Some`。`Esc` / `q` で `None` に戻す。
    pub note_detail: Option<crate::note_detail::NoteDetailScreen>,
    /// Help overlay の scroll 状態。`Focus::Help` の入り口で `scroll = 0` に
    /// リセットされる ── 毎回先頭から読めるようにする。
    pub help_state: HelpState,
    /// Issue #131: 現在進行中の async ネットワーク操作の数。`> 0` のとき
    /// `render_status` が左端に spinner を出す。各 async ハンドラの冒頭で
    /// [`crate::in_flight::InFlightGuard::new`] を構築して
    /// increment、関数を抜けるとき (= guard drop 時) に自動 decrement。
    /// `Arc` なので `app.in_flight.clone()` で guard に渡せる (= `&mut App`
    /// borrow を await またぎで保持できない Rust の制約への対応)。
    pub in_flight: Arc<AtomicUsize>,
    /// Issue #93: 同一 TUI セッションで「直前に送信した投稿の意図」を覚える。
    /// `submit_note` 成功時にのみ更新され、`Compose::clear` 後の再シードに使う。
    /// Esc 離脱 / POST 失敗では更新しない (= ユーザの「うっかり戻し」事故を防ぐ)。
    /// 起動時の既定値は [`LastComposeDefaults::default`] (= public / sensitive
    /// off / CW 無し)。TUI 再起動を跨ぐ永続化は別 Issue で扱う。
    pub last_compose_defaults: LastComposeDefaults,
}

impl App {
    pub fn new(
        theme: Theme,
        whoami: Whoami,
        socket_label: String,
        images: ImageCache,
        previews: crate::preview::PreviewCache,
        suppression: ImageSuppression,
    ) -> Self {
        Self {
            theme,
            whoami,
            notes: Vec::new(),
            selected: 0,
            top: 0,
            next_before_id: None,
            timeline_exhausted: false,
            focus: Focus::Timeline,
            compose: Compose::new(),
            status: None,
            should_quit: false,
            socket_label,
            images,
            picker: None,
            previews,
            pending_uploads: 0,
            suppression,
            suppression_cursor: 0,
            alt_prompt: None,
            last_reaction_ids: HashMap::new(),
            last_renote_ids: HashMap::new(),
            profile_stack: Vec::new(),
            follow_list: None,
            command: None,
            follow_requests: None,
            notifications: None,
            emoji_suggest: None,
            note_detail: None,
            help_state: HelpState::default(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            last_compose_defaults: LastComposeDefaults::default(),
        }
    }

    /// Issue #131: 現在 in-flight な async 操作数。`render_status` が `> 0`
    /// のとき spinner を出すために読む。`Ordering::Relaxed` で読むのは
    /// 「正確な瞬間値」より「最終的に 0 に戻る」ことの方が重要だから。
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// 現在開いている Profile 画面 (= stack 末尾) への可変参照。
    pub fn current_profile_mut(&mut self) -> Option<&mut crate::profile::ProfileScreen> {
        self.profile_stack.last_mut()
    }

    /// 現在開いている Profile 画面 (= stack 末尾) への不変参照。
    #[must_use]
    pub fn current_profile(&self) -> Option<&crate::profile::ProfileScreen> {
        self.profile_stack.last()
    }

    /// 初回 / 手動更新で取ったタイムラインで上書きする。
    pub fn replace_timeline(&mut self, notes: Vec<TimelineNote>, next_before_id: Option<i64>) {
        self.timeline_exhausted = notes.is_empty();
        self.notes = notes;
        self.next_before_id = next_before_id;
        self.selected = 0;
        self.top = 0;
    }

    /// 続きページを末尾に追記する。
    pub fn append_older(&mut self, mut more: Vec<TimelineNote>, next_before_id: Option<i64>) {
        if more.is_empty() {
            self.timeline_exhausted = true;
            return;
        }
        self.notes.append(&mut more);
        self.next_before_id = next_before_id;
    }

    /// SSE からの `note.created` を反映する。重複 (= 自分の POST が SSE で返って
    /// くる場合 / 連投の race) は `id` で dedupe する。
    pub fn ingest_note_created(&mut self, payload: NoteCreatedPayload) {
        let mut note = payload.into_timeline_note();
        // SSE 越しでは `is_local` が分からないので whoami と突き合わせて補正。
        if note.actor_ap_id == self.whoami.ap_id {
            note.is_local = true;
        }
        // 既にある note の更新は無視 (= timeline は append-only)。
        if self.notes.iter().any(|n| n.id == note.id) {
            return;
        }
        // 先頭挿入。selected を 0 に保ちたいので何もしないと自動で「いま選択中の
        // note」がずれる。直感的には「新着でカーソルだけ 1 つ下に移る」が安全。
        self.notes.insert(0, note);
        if !self.notes.is_empty() {
            self.selected = self.selected.saturating_add(1).min(self.notes.len() - 1);
            self.top = self.top.saturating_add(1).min(self.notes.len() - 1);
        }
    }

    /// 1 件下に選択。タイムライン末尾を超えると no-op。
    pub fn select_next(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        if self.selected + 1 < self.notes.len() {
            self.selected += 1;
        }
    }

    /// 1 件上に選択。
    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// ページ単位の上下。viewport 高さは `runtime` から渡す。
    pub fn page_down(&mut self, viewport_rows: usize) {
        if self.notes.is_empty() {
            return;
        }
        let step = viewport_rows.max(1);
        self.selected = (self.selected + step).min(self.notes.len() - 1);
    }

    pub fn page_up(&mut self, viewport_rows: usize) {
        let step = viewport_rows.max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    /// マウスクリックで idx を直接選択。
    pub fn select_index(&mut self, idx: usize) {
        if idx < self.notes.len() {
            self.selected = idx;
        }
    }

    /// `selected` が viewport から外れていたら `top` を調整して入れる。
    /// `viewport_rows` は 1 件 1 行換算でなく「件数」(タイムラインを N 件表示)。
    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + v {
            self.top = self.selected + 1 - v;
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>, kind: StatusKind, ttl: Option<Duration>) {
        self.status = Some(StatusLine {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
            ttl,
        });
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    pub fn tick(&mut self) {
        if let Some(s) = &self.status
            && s.expired()
        {
            self.status = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::client::Whoami;

    fn note(id: i64, actor: &str) -> TimelineNote {
        TimelineNote {
            id,
            ap_id: format!("https://x.test/notes/{id}"),
            url: None,
            actor_id: 1,
            actor_ap_id: actor.into(),
            actor_preferred_username: "me".into(),
            actor_display_name: None,
            actor_icon_url: None,
            content: format!("note #{id}"),
            summary: None,
            language: None,
            visibility: "public".into(),
            sensitive: false,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            published_at: Utc::now(),
            is_local: actor == "https://x.test/users/me",
            reactions: Vec::new(),
            attachments: Vec::new(),
            emojis: Vec::new(),
            announce_count: 0,
            viewer_renoted: false,
        }
    }

    fn new_app() -> App {
        App::new(
            Theme::default(),
            whoami(),
            "test".into(),
            ImageCache::new(None, None),
            crate::preview::PreviewCache::new(None),
            ImageSuppression::default(),
        )
    }

    fn whoami() -> Whoami {
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
        }
    }

    #[test]
    fn ingest_note_created_dedupes_by_id() {
        let mut app = new_app();
        app.replace_timeline(vec![note(5, "https://x.test/users/me")], None);
        let payload = NoteCreatedPayload {
            id: 5,
            ap_id: "https://x.test/notes/5".into(),
            actor_id: 1,
            actor_ap_id: "https://x.test/users/me".into(),
            actor_preferred_username: "me".into(),
            actor_display_name: None,
            actor_icon_url: None,
            content: "dup".into(),
            summary: None,
            visibility: "public".into(),
            sensitive: false,
            url: None,
            published_at: Utc::now(),
        };
        app.ingest_note_created(payload);
        assert_eq!(app.notes.len(), 1);
        assert_eq!(app.notes[0].content, "note #5");
    }

    #[test]
    fn ingest_note_created_marks_local_from_whoami() {
        let mut app = new_app();
        let payload = NoteCreatedPayload {
            id: 9,
            ap_id: "https://x.test/notes/9".into(),
            actor_id: 1,
            actor_ap_id: "https://x.test/users/me".into(),
            actor_preferred_username: "me".into(),
            actor_display_name: None,
            actor_icon_url: None,
            content: "x".into(),
            summary: None,
            visibility: "public".into(),
            sensitive: false,
            url: None,
            published_at: Utc::now(),
        };
        app.ingest_note_created(payload);
        assert!(app.notes[0].is_local);
    }

    #[test]
    fn select_next_clamped_at_end() {
        let mut app = new_app();
        app.replace_timeline(
            vec![
                note(3, "https://x.test/users/me"),
                note(2, "https://x.test/users/me"),
            ],
            None,
        );
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn ensure_visible_scrolls_top() {
        let mut app = new_app();
        app.replace_timeline(
            (0..50)
                .map(|i| note(50 - i, "https://x.test/users/me"))
                .collect(),
            None,
        );
        app.selected = 20;
        app.ensure_visible(10);
        assert_eq!(app.top, 11); // 20-10+1
        app.selected = 5;
        app.ensure_visible(10);
        assert_eq!(app.top, 5);
    }

    #[test]
    fn profile_stack_round_trips() {
        use crate::client::{ActorProfile, Relationship};
        use crate::profile::ProfileScreen;
        let mut app = new_app();
        assert!(app.current_profile().is_none());
        let actor = ActorProfile {
            id: 42,
            ap_id: "https://x.test/users/alice".into(),
            preferred_username: "alice".into(),
            host: "x.test".into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            moved_to_ap_id: None,
            is_local: false,
            actor_type: "Person".into(),
            manually_approves_followers: false,
        };
        let rel = Relationship::neutral();
        app.profile_stack
            .push(ProfileScreen::new(actor.clone(), rel, vec![], None));
        assert_eq!(app.current_profile().map(|p| p.actor.id), Some(42));
        // 2 段目を push して deep-stack 形態を確認。
        app.profile_stack.push(ProfileScreen::new(
            actor,
            Relationship::neutral(),
            vec![],
            None,
        ));
        assert_eq!(app.profile_stack.len(), 2);
        app.profile_stack.pop();
        assert_eq!(app.profile_stack.len(), 1);
    }

    #[test]
    fn status_expires_after_ttl() {
        let mut app = new_app();
        app.set_status("hi", StatusKind::Info, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        app.tick();
        assert!(app.status.is_none());
    }

    // Help overlay の scroll state を検証。
    //
    // `HelpState` は renderer が毎フレーム `sync_geometry` で書き戻す ──
    // テストは renderer 抜きで「ジオメトリが既知のときに scroll が
    // どう振る舞うか」を確かめる。`last_inner_height = 10` / `last_total_lines
    // = 30` のとき `max_scroll = 20`、`page_step = 9` を期待する。

    #[test]
    fn help_state_scroll_down_clamps_at_max() {
        let mut s = HelpState::default();
        s.sync_geometry(30, 10);
        s.scroll_down(5);
        assert_eq!(s.scroll, 5);
        s.scroll_down(100);
        assert_eq!(s.scroll, 20, "clamped to max_scroll = 30 - 10");
    }

    #[test]
    fn help_state_scroll_up_saturates_at_zero() {
        let mut s = HelpState::default();
        s.sync_geometry(30, 10);
        s.scroll_down(8);
        s.scroll_up(3);
        assert_eq!(s.scroll, 5);
        s.scroll_up(100);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn help_state_scroll_top_and_bottom() {
        let mut s = HelpState::default();
        s.sync_geometry(30, 10);
        s.scroll_bottom();
        assert_eq!(s.scroll, 20);
        s.scroll_top();
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn help_state_page_step_leaves_one_line_of_context() {
        let mut s = HelpState::default();
        s.sync_geometry(80, 10);
        assert_eq!(s.page_step(), 9, "viewport-1 step");
    }

    #[test]
    fn help_state_no_scroll_when_content_fits() {
        let mut s = HelpState::default();
        s.sync_geometry(10, 18); // content shorter than viewport
        assert_eq!(s.max_scroll(), 0);
        s.scroll_down(5);
        assert_eq!(s.scroll, 0, "no scroll possible");
    }

    #[test]
    fn help_state_sync_geometry_clamps_existing_scroll() {
        let mut s = HelpState::default();
        s.sync_geometry(80, 10); // max_scroll = 70
        s.scroll_down(50);
        assert_eq!(s.scroll, 50);
        // overlay 高さが伸びて max_scroll が縮むケース (resize)。
        s.sync_geometry(80, 60); // max_scroll = 20
        assert_eq!(s.scroll, 20, "resize clamps scroll into new range");
    }

    #[test]
    fn help_state_page_step_min_one_even_when_viewport_unknown() {
        // 描画前 (= sync_geometry がまだ呼ばれていない) でも 1 行は進む。
        let s = HelpState::default();
        assert_eq!(s.page_step(), 1);
    }
}
