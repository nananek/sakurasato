//! 絵文字管理画面。`:emojis` で開く ([`crate::command::Command::OpenEmojiAdmin`])。
//!
//! [`crate::lists::ListsScreen`] と同じ「単一 `Focus`、内部 state でタブ /
//! サブ画面を切替」設計。2 タブ (すべて `Focus::EmojiAdmin` のまま):
//!
//! - **Local** (既定): 自分のローカル絵文字一覧 (`GET /api/v1/emojis`)。
//!   `i` でファイルピッカを開き Misskey 形式 zip をインポート
//!   (`POST /api/v1/emojis/import`)。
//! - **Remote** (`t` で切替): DB にキャッシュ済みのリモート絵文字
//!   (`EmojiReact` 受信で自動学習済み) を検索 (`GET /api/v1/emojis/remote`)。
//!   `Enter` で選択中の 1 件を即座にローカルへコピーする
//!   (`POST /api/v1/emojis/local/from-remote`、shortcode はリネームせず
//!   元のまま ── [`crate::follow_requests::FollowRequestsScreen`] の
//!   `a` (approve) と同じ「確認プロンプト無しの即時実行」パターン)。
//!
//! 両タブ共通: `j`/`k` 移動、`/` で検索窓 (`query_input`) を開き `Enter` で
//! 確定検索、`r` で再取得、`Esc`/`q` で閉じる。

use crate::client::{EmojiItem, RemoteEmojiItem};

/// 現在表示中のタブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmojiAdminTab {
    #[default]
    Local,
    Remote,
}

/// 絵文字管理画面の state。[`crate::app::App::emoji_admin`] が保持する。
#[derive(Debug, Clone, Default)]
pub struct EmojiAdminScreen {
    pub tab: EmojiAdminTab,
    pub local_items: Vec<EmojiItem>,
    pub local_cursor: usize,
    pub local_top: usize,
    pub remote_items: Vec<RemoteEmojiItem>,
    pub remote_cursor: usize,
    pub remote_top: usize,
    /// `r` や開いた直後の再取得中フラグ。[`crate::follow_requests::FollowRequestsScreen`]
    /// と同じ使い方。
    pub fetching: bool,
    /// `Some` のとき下部に 1 行検索窓 overlay を表示中。
    pub query_input: Option<QueryInput>,
    /// 直近に確定した検索クエリ。`r` (再取得) はこれを使って再実行する。
    pub committed_query: String,
}

impl EmojiAdminScreen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            EmojiAdminTab::Local => EmojiAdminTab::Remote,
            EmojiAdminTab::Remote => EmojiAdminTab::Local,
        };
    }

    pub fn replace_local(&mut self, items: Vec<EmojiItem>) {
        if self.local_cursor >= items.len() {
            self.local_cursor = items.len().saturating_sub(1);
        }
        if self.local_top >= items.len() {
            self.local_top = items.len().saturating_sub(1);
        }
        self.local_items = items;
        self.fetching = false;
    }

    pub fn replace_remote(&mut self, items: Vec<RemoteEmojiItem>) {
        if self.remote_cursor >= items.len() {
            self.remote_cursor = items.len().saturating_sub(1);
        }
        if self.remote_top >= items.len() {
            self.remote_top = items.len().saturating_sub(1);
        }
        self.remote_items = items;
        self.fetching = false;
    }

    pub fn select_next(&mut self) {
        match self.tab {
            EmojiAdminTab::Local => {
                if !self.local_items.is_empty() {
                    self.local_cursor = (self.local_cursor + 1).min(self.local_items.len() - 1);
                }
            }
            EmojiAdminTab::Remote => {
                if !self.remote_items.is_empty() {
                    self.remote_cursor = (self.remote_cursor + 1).min(self.remote_items.len() - 1);
                }
            }
        }
    }

    pub fn select_prev(&mut self) {
        match self.tab {
            EmojiAdminTab::Local => self.local_cursor = self.local_cursor.saturating_sub(1),
            EmojiAdminTab::Remote => self.remote_cursor = self.remote_cursor.saturating_sub(1),
        }
    }

    #[must_use]
    pub fn current_remote(&self) -> Option<&RemoteEmojiItem> {
        self.remote_items.get(self.remote_cursor)
    }

    /// リモートコピー成功時、Local タブの一覧に反映する (再 fetch を避ける)。
    /// 同名 shortcode が既にあれば上書き (サーバ側 upsert と同じ意味論)。
    pub fn upsert_local(&mut self, item: EmojiItem) {
        if let Some(existing) = self
            .local_items
            .iter_mut()
            .find(|e| e.shortcode == item.shortcode)
        {
            *existing = item;
        } else {
            self.local_items.push(item);
        }
    }

    /// カーソルが viewport から外れていたら `top` を追従させる。
    /// [`crate::follow_requests::FollowRequestsScreen::ensure_visible`] と
    /// 同じ実装をタブ別に適用する。
    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        match self.tab {
            EmojiAdminTab::Local => {
                if self.local_cursor < self.local_top {
                    self.local_top = self.local_cursor;
                } else if self.local_cursor >= self.local_top + v {
                    self.local_top = self.local_cursor + 1 - v;
                }
            }
            EmojiAdminTab::Remote => {
                if self.remote_cursor < self.remote_top {
                    self.remote_top = self.remote_cursor;
                } else if self.remote_cursor >= self.remote_top + v {
                    self.remote_top = self.remote_cursor + 1 - v;
                }
            }
        }
    }
}

/// 検索窓の 1 行入力。[`crate::lists::ListsInput`] と同じ `buffer` パターン。
#[derive(Debug, Clone, Default)]
pub struct QueryInput {
    pub buffer: String,
}

impl QueryInput {
    /// [`crate::alt_prompt::AltPrompt::MAX_CHARS`] と同じ考え方。
    pub const MAX_CHARS: usize = 200;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        if self.buffer.chars().count() < Self::MAX_CHARS {
            self.buffer.push(c);
        }
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    #[must_use]
    pub fn value(&self) -> &str {
        self.buffer.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EmojiKind;

    fn local_item(shortcode: &str) -> EmojiItem {
        EmojiItem {
            kind: EmojiKind::Custom,
            shortcode: shortcode.to_string(),
            url: format!("https://example.test/media/{shortcode}.webp"),
            media_type: "image/webp".into(),
            category: None,
            aliases: vec![],
            codepoint: None,
        }
    }

    fn remote_item(id: i64, shortcode: &str) -> RemoteEmojiItem {
        RemoteEmojiItem {
            id,
            shortcode: shortcode.to_string(),
            host: "misskey.example".into(),
            url: format!("https://example.test/media/remote/{shortcode}.webp"),
            media_type: "image/webp".into(),
            category: None,
            aliases: vec![],
        }
    }

    #[test]
    fn select_next_prev_saturate_within_current_tab() {
        let mut s = EmojiAdminScreen::new();
        s.replace_local(vec![local_item("a"), local_item("b"), local_item("c")]);
        assert_eq!(s.local_cursor, 0);
        s.select_next();
        s.select_next();
        s.select_next(); // saturates at len-1
        assert_eq!(s.local_cursor, 2);
        s.select_prev();
        s.select_prev();
        s.select_prev(); // saturates at 0
        assert_eq!(s.local_cursor, 0);
    }

    #[test]
    fn select_next_on_empty_list_is_noop() {
        let mut s = EmojiAdminScreen::new();
        s.select_next();
        assert_eq!(s.local_cursor, 0);
    }

    #[test]
    fn toggle_tab_keeps_each_tabs_cursor_independent() {
        let mut s = EmojiAdminScreen::new();
        s.replace_local(vec![local_item("a"), local_item("b")]);
        s.replace_remote(vec![remote_item(1, "x"), remote_item(2, "y")]);
        s.select_next(); // local_cursor -> 1
        assert_eq!(s.local_cursor, 1);
        s.toggle_tab();
        assert_eq!(s.tab, EmojiAdminTab::Remote);
        assert_eq!(s.remote_cursor, 0, "remote cursor untouched by local moves");
        s.select_next(); // remote_cursor -> 1
        assert_eq!(s.remote_cursor, 1);
        s.toggle_tab();
        assert_eq!(
            s.local_cursor, 1,
            "local cursor preserved after switching back"
        );
    }

    #[test]
    fn current_remote_returns_selected_item() {
        let mut s = EmojiAdminScreen::new();
        s.tab = EmojiAdminTab::Remote;
        s.replace_remote(vec![remote_item(1, "x"), remote_item(2, "y")]);
        s.select_next();
        assert_eq!(s.current_remote().map(|i| i.id), Some(2));
    }

    #[test]
    fn upsert_local_overwrites_same_shortcode_else_pushes() {
        let mut s = EmojiAdminScreen::new();
        s.replace_local(vec![local_item("a")]);
        let mut updated = local_item("a");
        updated.media_type = "image/png".into();
        s.upsert_local(updated);
        assert_eq!(s.local_items.len(), 1);
        assert_eq!(s.local_items[0].media_type, "image/png");

        s.upsert_local(local_item("b"));
        assert_eq!(s.local_items.len(), 2);
    }

    #[test]
    fn ensure_visible_scrolls_within_current_tab_only() {
        let mut s = EmojiAdminScreen::new();
        s.replace_local(vec![local_item("a"), local_item("b"), local_item("c")]);
        s.local_cursor = 2;
        s.ensure_visible(2);
        assert_eq!(s.local_top, 1);
    }

    #[test]
    fn query_input_respects_max_chars_and_trims_value() {
        let mut q = QueryInput::new();
        q.insert_char(' ');
        q.insert_char('a');
        q.insert_char(' ');
        assert_eq!(q.value(), "a");
        q.backspace();
        q.backspace();
        assert_eq!(q.buffer, " ");
    }
}
