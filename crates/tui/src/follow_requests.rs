//! M12 (Issue #66): 鍵アカ運用の承認待ち follow を見て / 承認 / 拒否する
//! 専用画面。`:requests` で開く ([`crate::command::Command::OpenRequests`])。
//!
//! 画面は単純な縦リスト ── 1 エントリ 2 行固定 (= 1 行目: display name +
//! acct + `[id]` + `received_at`、2 行目: summary を 1 行化、無ければ空行)。
//! キー操作は [`crate::event::translate_requests_key`] 参照:
//!
//! - `j` / `k` ── カーソル移動
//! - `a`      ── 選択行を approve
//! - `x`      ── 選択行を reject
//! - `r`      ── 再取得
//! - `Esc`/`q` ── 閉じる (= Timeline に戻る)
//!
//! 状態は in-memory のみ。Lock 解除しても pending は auto-accept されない
//! 仕様 (= Mastodon と同じ作法 / CLAUDE.md §5.1) のため、ユーザが明示的に
//! 操作するための画面という位置付け。

use crate::client::PendingFollow;

/// 一覧画面の state。[`crate::app::App::follow_requests`] が保持する。
#[derive(Debug, Clone, Default)]
pub struct FollowRequestsScreen {
    pub items: Vec<PendingFollow>,
    pub cursor: usize,
    /// スクロール offset (描画の先頭 index)。[`Self::ensure_visible`] が
    /// `viewport_items` を見て更新する。[`crate::follow_list::FollowListScreen`]
    /// と同じパターン。
    pub top: usize,
    /// `r` や開いた直後の再取得中フラグ。UI は spinner を出さず
    /// `loading…` を出す程度の使い方。
    pub fetching: bool,
}

impl FollowRequestsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, items: Vec<PendingFollow>) {
        if self.cursor >= items.len() {
            self.cursor = items.len().saturating_sub(1);
        }
        if self.top >= items.len() {
            self.top = items.len().saturating_sub(1);
        }
        self.items = items;
        self.fetching = false;
    }

    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1).min(self.items.len() - 1);
    }

    pub fn select_prev(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    #[must_use]
    pub fn current(&self) -> Option<&PendingFollow> {
        self.items.get(self.cursor)
    }

    /// 指定 id を一覧から消す。approve / reject 成功時にローカル state を
    /// 同期するため (= 再 fetch コストを避ける) 使う。
    pub fn remove_id(&mut self, id: i64) {
        if let Some(pos) = self.items.iter().position(|p| p.id == id) {
            self.items.remove(pos);
            if self.cursor >= self.items.len() {
                self.cursor = self.items.len().saturating_sub(1);
            }
            if self.top >= self.items.len() {
                self.top = self.items.len().saturating_sub(1);
            }
        }
    }

    /// カーソルが viewport から外れていたら [`Self::top`] を追従させる。
    /// [`crate::follow_list::FollowListScreen::ensure_visible`] と同じ実装。
    /// main loop 側で毎フレーム呼ばれる前提 (= `last_rects` で確定した行数を渡す)。
    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + v {
            self.top = self.cursor + 1 - v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64) -> PendingFollow {
        PendingFollow {
            id,
            ap_id: format!("https://x.test/users/{id}/Follow"),
            follower_ap_id: format!("https://x.test/users/u{id}"),
            follower_acct: format!("u{id}@x.test"),
            follower_display_name: None,
            follower_summary: None,
            received_at: "2026-06-01T00:00:00Z".into(),
            state: "pending".into(),
        }
    }

    #[test]
    fn cursor_clamps_after_replace() {
        let mut s = FollowRequestsScreen::new();
        s.replace(vec![item(1), item(2), item(3)]);
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 2);
        s.replace(vec![item(10)]);
        assert_eq!(s.cursor, 0, "cursor must clamp to new length");
    }

    #[test]
    fn select_next_stops_at_end() {
        let mut s = FollowRequestsScreen::new();
        s.replace(vec![item(1), item(2)]);
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn select_prev_stops_at_zero() {
        let mut s = FollowRequestsScreen::new();
        s.replace(vec![item(1), item(2)]);
        s.select_prev();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn remove_id_shrinks_and_clamps() {
        let mut s = FollowRequestsScreen::new();
        s.replace(vec![item(1), item(2), item(3)]);
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 2);
        s.remove_id(3);
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.cursor, 1, "cursor moves up when last row deleted");
    }

    #[test]
    fn current_is_none_when_empty() {
        let s = FollowRequestsScreen::new();
        assert!(s.current().is_none());
    }

    #[test]
    fn ensure_visible_scrolls_top_when_cursor_below_window() {
        let mut s = FollowRequestsScreen::new();
        s.replace((1..=10).map(item).collect());
        // viewport=3, cursor=5 → top should follow to 3 (= 5+1-3).
        s.cursor = 5;
        s.ensure_visible(3);
        assert_eq!(s.top, 3);
    }

    #[test]
    fn ensure_visible_scrolls_top_up_when_cursor_above_window() {
        let mut s = FollowRequestsScreen::new();
        s.replace((1..=10).map(item).collect());
        s.top = 6;
        s.cursor = 2;
        s.ensure_visible(3);
        // cursor 2 < top 6 → top = cursor = 2.
        assert_eq!(s.top, 2);
    }

    #[test]
    fn ensure_visible_no_op_when_cursor_inside_window() {
        let mut s = FollowRequestsScreen::new();
        s.replace((1..=10).map(item).collect());
        s.top = 2;
        s.cursor = 3;
        s.ensure_visible(3);
        assert_eq!(s.top, 2);
    }
}
