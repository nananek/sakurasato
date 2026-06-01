//! M12 (Issue #66): 鍵アカ運用の承認待ち follow を見て / 承認 / 拒否する
//! 専用画面。`:requests` で開く ([`crate::command::Command::OpenRequests`])。
//!
//! 画面は単純な縦リスト ── 各行に `id` / `follower_ap_id` / `received_at` を
//! 並べる。キー操作は [`crate::event::translate_requests_key`] 参照:
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
    /// `r` や開いた直後の再取得中フラグ。UI は spinner を出さず status line に
    /// "loading…" を出す程度の使い方。
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
}
