//! TUI 全体のアプリ状態。
//!
//! - **タイムライン**: `notes` を `id` 降順 (= 新しい順) で保持。SSE で来た
//!   `note.created` は先頭に挿入する。
//! - **focus**: タイムライン / 投稿エディタ / ヘルプ画面の 3 値。
//! - **`status_line`**: 一時メッセージ (投稿成功、エラー、ロード中) を 1 行表示。
//! - **scroll**: タイムラインの先頭から表示開始するインデックス (`top`)。
//! - **selected**: ハイライトされている note の index (`top` 以上)。

use std::time::{Duration, Instant};

use crate::client::{NoteCreatedPayload, TimelineNote, Whoami};
use crate::compose::Compose;
use crate::image_cache::ImageCache;
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

/// 入力フォーカス。マウスクリックでも切り替えられる ([`crate::event`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Timeline,
    Compose,
    Help,
}

#[derive(Debug, Clone)]
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
}

impl App {
    pub fn new(theme: Theme, whoami: Whoami, socket_label: String, images: ImageCache) -> Self {
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
        }
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
        }
    }

    fn new_app() -> App {
        App::new(
            Theme::default(),
            whoami(),
            "test".into(),
            ImageCache::new(None),
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
    fn status_expires_after_ttl() {
        let mut app = new_app();
        app.set_status("hi", StatusKind::Info, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        app.tick();
        assert!(app.status.is_none());
    }
}
