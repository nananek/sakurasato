//! M13 PR5 (Issue #79): `FollowList` 画面の状態保持。
//!
//! 自分の following / followers を `accepted` で絞った一覧 (= server 側の
//! `GET /api/v1/following` / `/followers` 応答)。pending 管理は本画面の対象外
//! ── 鍵アカ承認待ち (#66) は `follow-requests` 系統 (= 別 UI) に倒す。
//!
//! ## モード
//!
//! 同じ画面で `t` トグルすると Following <-> Followers が切り替わる。両方の
//! データを別々に持たせて、トグル時に再 fetch しない (= 一覧件数が少ない前提
//! のお一人様サーバ向け最適化)。`r` (refresh) でその時点で開いているモード
//! だけを再取得する。
//!
//! ## 画面遷移
//!
//! Timeline → `:following` (or `:followers`) → `FollowList` → `Enter` → Profile
//! → `Esc` → `FollowList`。Profile は [`crate::profile::ProfileScreen`] と同じ
//! `profile_stack` に積み、`FollowList` を抜けずに戻れるようにする。

use crate::client::FollowListEntry;

/// 表示中タブ。サーバ API が `/following` と `/followers` に分かれているため、
/// 両者は別エンドポイント呼び出しで取り直す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowListMode {
    /// 自分が follow している側。`FollowListScreen` を空 init で組み立てたとき
    /// (= テストや `:following` 既定経路) の初期タブ。
    #[default]
    Following,
    /// 自分を follow している側。
    Followers,
}

impl FollowListMode {
    /// 表示用ラベル (ヘッダ + ステータス文言で使う)。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Followers => "followers",
        }
    }

    /// `t` トグルで反対モードに変える。
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Following => Self::Followers,
            Self::Followers => Self::Following,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ModePage {
    pub entries: Vec<FollowListEntry>,
    pub next_before_id: Option<i64>,
    pub exhausted: bool,
    /// 1 度でも fetch を試みたか (= 空配列の表示を「未取得」と「フォロワー 0」で
    /// 出し分けるため)。
    pub fetched: bool,
}

impl ModePage {
    pub fn replace(&mut self, entries: Vec<FollowListEntry>, next_before_id: Option<i64>) {
        self.exhausted = entries.is_empty();
        self.entries = entries;
        self.next_before_id = next_before_id;
        self.fetched = true;
    }

    pub fn append(&mut self, mut more: Vec<FollowListEntry>, next_before_id: Option<i64>) {
        if more.is_empty() {
            self.exhausted = true;
            return;
        }
        self.entries.append(&mut more);
        self.next_before_id = next_before_id;
    }
}

#[derive(Debug, Default, Clone)]
pub struct FollowListScreen {
    pub mode: FollowListMode,
    pub following: ModePage,
    pub followers: ModePage,
    pub selected: usize,
    pub top: usize,
}

impl FollowListScreen {
    #[must_use]
    pub fn new(mode: FollowListMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// 現在表示中タブの page を借りる。
    #[must_use]
    pub fn current(&self) -> &ModePage {
        match self.mode {
            FollowListMode::Following => &self.following,
            FollowListMode::Followers => &self.followers,
        }
    }

    /// 現在表示中タブの page を可変で借りる。
    pub fn current_mut(&mut self) -> &mut ModePage {
        match self.mode {
            FollowListMode::Following => &mut self.following,
            FollowListMode::Followers => &mut self.followers,
        }
    }

    /// `t` トグル ── 反対モードに切り替えてカーソルをリセット。
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.flipped();
        self.selected = 0;
        self.top = 0;
    }

    /// 現在表示中の Entry を返す (Enter で Profile push する候補)。
    #[must_use]
    pub fn current_entry(&self) -> Option<&FollowListEntry> {
        self.current().entries.get(self.selected)
    }

    pub fn select_next(&mut self) {
        let len = self.current().entries.len();
        if len == 0 {
            return;
        }
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// `PageDown` ── `viewport_items` 件分だけ下へ。末尾で clamp。
    /// `runtime` から viewport 件数を渡す ([`crate::app::App::page_down`] と同設計)。
    pub fn select_page_down(&mut self, viewport_items: usize) {
        let len = self.current().entries.len();
        if len == 0 {
            return;
        }
        let step = viewport_items.max(1);
        self.selected = (self.selected + step).min(len - 1);
    }

    /// `PageUp` ── `viewport_items` 件分だけ上へ。0 で clamp。
    pub fn select_page_up(&mut self, viewport_items: usize) {
        let step = viewport_items.max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + v {
            self.top = self.selected + 1 - v;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::client::{ActorProfile, FollowListEntry};

    fn actor(id: i64, user: &str, host: &str) -> ActorProfile {
        ActorProfile {
            id,
            ap_id: format!("https://{host}/users/{user}"),
            preferred_username: user.into(),
            host: host.into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            moved_to_ap_id: None,
            is_local: false,
            actor_type: "Person".into(),
            manually_approves_followers: false,
        }
    }

    fn entry(follow_id: i64, actor_id: i64, user: &str) -> FollowListEntry {
        FollowListEntry {
            follow_id,
            follow_state: "accepted".into(),
            follow_created_at: Utc::now(),
            actor: actor(actor_id, user, "remote.test"),
        }
    }

    #[test]
    fn toggle_mode_resets_cursor() {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        s.following
            .replace(vec![entry(1, 2, "bob"), entry(3, 4, "carol")], None);
        s.select_next();
        assert_eq!(s.selected, 1);
        s.toggle_mode();
        assert_eq!(s.mode, FollowListMode::Followers);
        assert_eq!(s.selected, 0);
        assert_eq!(s.top, 0);
        s.toggle_mode();
        assert_eq!(s.mode, FollowListMode::Following);
    }

    #[test]
    fn current_entry_returns_selected() {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        s.following
            .replace(vec![entry(1, 2, "bob"), entry(3, 4, "carol")], None);
        assert_eq!(s.current_entry().map(|e| e.follow_id), Some(1));
        s.select_next();
        assert_eq!(s.current_entry().map(|e| e.follow_id), Some(3));
        // 末尾を超えても clamp。
        s.select_next();
        assert_eq!(s.current_entry().map(|e| e.follow_id), Some(3));
    }

    #[test]
    fn append_marks_exhausted_on_empty() {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        s.following.replace(vec![entry(5, 6, "bob")], Some(5));
        s.following.append(vec![], None);
        assert!(s.following.exhausted);
        assert_eq!(s.following.entries.len(), 1);
    }

    fn screen_with(n: i64) -> FollowListScreen {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        let entries: Vec<_> = (1..=n)
            .map(|i| entry(i, i + 100, &format!("u{i}")))
            .collect();
        s.following.replace(entries, None);
        s
    }

    #[test]
    fn ensure_visible_scrolls_top_when_cursor_below_window() {
        let mut s = screen_with(20);
        s.selected = 7;
        // viewport=3, cursor=7 → top = 7+1-3 = 5.
        s.ensure_visible(3);
        assert_eq!(s.top, 5);
    }

    #[test]
    fn ensure_visible_scrolls_top_up_when_cursor_above_window() {
        let mut s = screen_with(20);
        s.top = 10;
        s.selected = 2;
        s.ensure_visible(3);
        assert_eq!(s.top, 2);
    }

    #[test]
    fn ensure_visible_no_op_when_cursor_inside_window() {
        let mut s = screen_with(20);
        s.top = 4;
        s.selected = 5;
        s.ensure_visible(3);
        assert_eq!(s.top, 4);
    }

    #[test]
    fn ensure_visible_treats_zero_viewport_as_one() {
        let mut s = screen_with(20);
        s.selected = 5;
        s.ensure_visible(0);
        // viewport=0 → 1 として扱い、selected=5 が見える top=5。
        assert_eq!(s.top, 5);
    }

    #[test]
    fn ensure_visible_on_empty_list_does_nothing() {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        // entries 空 / cursor=0 / top=0 で no-op、特に panic しないこと。
        s.ensure_visible(10);
        assert_eq!(s.top, 0);
    }

    #[test]
    fn select_page_down_clamps_at_end() {
        let mut s = screen_with(10);
        s.select_page_down(4);
        assert_eq!(s.selected, 4);
        s.select_page_down(4);
        assert_eq!(s.selected, 8);
        s.select_page_down(4);
        // 末尾 (= len - 1 = 9) で clamp。
        assert_eq!(s.selected, 9);
    }

    #[test]
    fn select_page_up_clamps_at_zero() {
        let mut s = screen_with(10);
        s.selected = 7;
        s.select_page_up(3);
        assert_eq!(s.selected, 4);
        s.select_page_up(10);
        // 0 で clamp (saturating_sub)。
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn select_page_down_on_empty_list_does_nothing() {
        let mut s = FollowListScreen::new(FollowListMode::Following);
        s.select_page_down(5);
        assert_eq!(s.selected, 0);
    }
}
