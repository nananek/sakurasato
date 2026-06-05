//! #206 PR3 ── in-app 通知一覧画面。`:notifications` / `n` で開く
//! ([`crate::command::Command::OpenNotifications`])。
//!
//! 画面は単純な縦リスト ── 各行に「未読マーカー / 種別アイコン / notifier /
//! プレビュー」を並べる。キー操作は [`crate::event::translate_notifications_key`]:
//!
//! - `j` / `k`  ── カーソル移動
//! - `m`        ── 全件既読化 (mark-all-read)
//! - `r`        ── 再取得
//! - `Esc`/`q`  ── 閉じる (= Timeline に戻る)
//!
//! 状態は in-memory。背後の通知本体は server 側 `notification` テーブル
//! ([[notifications]] backend #206 PR1) で、`crate::client::list_notifications`
//! 経由で取得する。[`crate::follow_requests::FollowRequestsScreen`] と同じ
//! スクロール / カーソル実装。

use crate::client::NotificationItem;

/// 一覧画面の state。[`crate::app::App::notifications`] が保持する。
#[derive(Debug, Clone, Default)]
pub struct NotificationsScreen {
    pub items: Vec<NotificationItem>,
    pub cursor: usize,
    /// スクロール offset (描画の先頭 index)。[`Self::ensure_visible`] が更新する。
    pub top: usize,
    /// 再取得中フラグ。
    pub fetching: bool,
    /// 未読件数 (= server から返る `unread_count`)。status バーのバッジ用。
    pub unread_count: i64,
}

impl NotificationsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, items: Vec<NotificationItem>, unread_count: i64) {
        if self.cursor >= items.len() {
            self.cursor = items.len().saturating_sub(1);
        }
        if self.top >= items.len() {
            self.top = items.len().saturating_sub(1);
        }
        self.items = items;
        self.unread_count = unread_count;
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

    /// ローカル state を「全既読」に倒す (= mark-all-read 成功時に再 fetch を
    /// 待たず即反映)。
    pub fn mark_all_read_local(&mut self) {
        for it in &mut self.items {
            it.is_read = true;
        }
        self.unread_count = 0;
    }

    /// カーソルが viewport から外れていたら [`Self::top`] を追従させる。
    /// [`crate::follow_requests::FollowRequestsScreen::ensure_visible`] と同実装。
    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + v {
            self.top = self.cursor + 1 - v;
        }
    }
}

/// 通知種別を 1 文字アイコン + 動詞ラベルに落とす (= 行頭の視覚的手がかり)。
/// 色は theme から取るので本関数は文字だけ返す。
#[must_use]
pub fn event_glyph_label(event_type: &str) -> (&'static str, &'static str) {
    match event_type {
        "reaction" => ("♥", "reacted"),
        "renote" => ("🔁", "renoted"),
        "quote" => ("❝", "quoted"),
        "follow" => ("+", "followed you"),
        "follow_request" => ("?", "requested to follow"),
        "mention" => ("@", "mentioned you"),
        "direct" => ("✉", "sent a DM"),
        _ => ("•", "notified"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, read: bool) -> NotificationItem {
        NotificationItem {
            id,
            event_type: "reaction".into(),
            is_read: read,
            created_at: "2026-06-05T00:00:00Z".into(),
            notifier_acct: Some("bob@x.test".into()),
            notifier_display_name: Some("Bob".into()),
            note_id: Some(1),
            note_preview: Some("hi".into()),
            reaction: Some("👍".into()),
        }
    }

    #[test]
    fn replace_clamps_cursor_and_sets_unread() {
        let mut s = NotificationsScreen::new();
        s.replace(vec![item(3, false), item(2, false), item(1, true)], 2);
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 2);
        s.replace(vec![item(9, false)], 1);
        assert_eq!(s.cursor, 0, "cursor clamps to new length");
        assert_eq!(s.unread_count, 1);
    }

    #[test]
    fn select_clamps_at_bounds() {
        let mut s = NotificationsScreen::new();
        s.replace(vec![item(1, false), item(2, false)], 2);
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 1);
        s.select_prev();
        s.select_prev();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn mark_all_read_local_clears_unread_and_flags() {
        let mut s = NotificationsScreen::new();
        s.replace(vec![item(1, false), item(2, false)], 2);
        s.mark_all_read_local();
        assert_eq!(s.unread_count, 0);
        assert!(s.items.iter().all(|i| i.is_read));
    }

    #[test]
    fn ensure_visible_follows_cursor() {
        let mut s = NotificationsScreen::new();
        s.replace((1..=10).map(|i| item(i, false)).collect(), 10);
        s.cursor = 5;
        s.ensure_visible(3);
        assert_eq!(s.top, 3);
        s.cursor = 1;
        s.ensure_visible(3);
        assert_eq!(s.top, 1);
    }
}
