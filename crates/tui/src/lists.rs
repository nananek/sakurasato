//! リスト機能 (Mastodon/Misskey 互換) の TUI 画面。`:lists` で開く
//! ([`crate::command::Command::OpenLists`])。
//!
//! 3 段構成 (すべて `Focus::Lists` のまま、内部 state で切り替える):
//!
//! - **一覧** (既定): 全リストを `id` / `title` / `member_count` で表示。
//!   `j`/`k` 移動、`Enter` でそのリストのタイムラインに切替 (= 画面を閉じて
//!   `App::current_timeline` を差し替え)、`m` でメンバー一覧へ、`n` で新規
//!   作成、`R` でリネーム、`d` で削除、`r` で再取得、`Esc`/`q` で閉じる。
//! - **メンバー一覧** (`members: Some(..)`): 選択中リストのメンバー
//!   ([`crate::client::ActorProfile`]) を表示。`j`/`k` 移動、`a` で acct 入力
//!   → 追加、`x` で選択中メンバーを削除、`Esc` で一覧に戻る。
//! - **タイトル入力** (`input: Some(..)`): `n`/`R`/`a` から入る 1 行入力
//!   overlay。[`crate::alt_prompt::AltPrompt`] と同じ `buffer` パターン。
//!   `Enter` で確定、`Esc` でキャンセルして 1 段戻る。

use crate::client::{ActorProfile, ListSummary};

/// 一覧画面の state。[`crate::app::App::lists`] が保持する。
#[derive(Debug, Clone, Default)]
pub struct ListsScreen {
    pub items: Vec<ListSummary>,
    pub cursor: usize,
    pub top: usize,
    pub fetching: bool,
    /// `Some` のときメンバー一覧サブ画面を表示中。
    pub members: Option<MembersView>,
    /// `Some` のとき 1 行入力 overlay を表示中 (作成 / リネーム / メンバー追加)。
    pub input: Option<ListsInput>,
}

impl ListsScreen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, items: Vec<ListSummary>) {
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
    pub fn current(&self) -> Option<&ListSummary> {
        self.items.get(self.cursor)
    }

    /// 削除成功時、ローカル state から即座に消す (再 fetch を避ける)。
    pub fn remove_id(&mut self, id: i64) {
        if let Some(pos) = self.items.iter().position(|l| l.id == id) {
            self.items.remove(pos);
            if self.cursor >= self.items.len() {
                self.cursor = self.items.len().saturating_sub(1);
            }
            if self.top >= self.items.len() {
                self.top = self.items.len().saturating_sub(1);
            }
        }
    }

    /// 作成 / リネーム成功時、該当行を挿入 or 上書きする。
    pub fn upsert(&mut self, updated: ListSummary) {
        if let Some(existing) = self.items.iter_mut().find(|l| l.id == updated.id) {
            *existing = updated;
        } else {
            self.items.push(updated);
        }
    }

    /// [`crate::follow_requests::FollowRequestsScreen::ensure_visible`] と
    /// 同じ実装。
    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + v {
            self.top = self.cursor + 1 - v;
        }
    }
}

/// メンバー一覧サブ画面の state。
#[derive(Debug, Clone)]
pub struct MembersView {
    pub list_id: i64,
    pub title: String,
    pub items: Vec<ActorProfile>,
    pub cursor: usize,
    pub top: usize,
}

impl MembersView {
    #[must_use]
    pub fn new(list_id: i64, title: String, items: Vec<ActorProfile>) -> Self {
        Self {
            list_id,
            title,
            items,
            cursor: 0,
            top: 0,
        }
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
    pub fn current(&self) -> Option<&ActorProfile> {
        self.items.get(self.cursor)
    }

    pub fn remove_actor(&mut self, actor_id: i64) {
        if let Some(pos) = self.items.iter().position(|a| a.id == actor_id) {
            self.items.remove(pos);
            if self.cursor >= self.items.len() {
                self.cursor = self.items.len().saturating_sub(1);
            }
            if self.top >= self.items.len() {
                self.top = self.items.len().saturating_sub(1);
            }
        }
    }

    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + v {
            self.top = self.cursor + 1 - v;
        }
    }
}

/// 1 行入力 overlay が何のために開かれたか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListsInputKind {
    /// 新規リスト作成 (タイトル入力)。
    Create,
    /// 選択中リストのリネーム (タイトル入力、`list_id`)。
    Rename(i64),
    /// メンバー追加 (acct 入力、`list_id`)。
    AddMember(i64),
}

/// [`crate::alt_prompt::AltPrompt`] と同じ `buffer` パターンの 1 行入力。
#[derive(Debug, Clone)]
pub struct ListsInput {
    pub kind: ListsInputKind,
    pub buffer: String,
}

impl ListsInput {
    /// タイトル / acct いずれも 1 行入力なのでこれくらいで十分
    /// ([`crate::alt_prompt::AltPrompt::MAX_CHARS`] と同じ考え方)。
    pub const MAX_CHARS: usize = 200;

    #[must_use]
    pub fn new(kind: ListsInputKind) -> Self {
        Self {
            kind,
            buffer: String::new(),
        }
    }

    #[must_use]
    pub fn with_initial(kind: ListsInputKind, initial: &str) -> Self {
        Self {
            kind,
            buffer: initial.to_string(),
        }
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

    fn summary(id: i64, member_count: i64) -> ListSummary {
        ListSummary {
            id,
            title: format!("list{id}"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            member_count,
        }
    }

    #[test]
    fn cursor_clamps_after_replace() {
        let mut s = ListsScreen::new();
        s.replace(vec![summary(1, 0), summary(2, 0), summary(3, 0)]);
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 2);
        s.replace(vec![summary(10, 0)]);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn remove_id_shrinks_and_clamps() {
        let mut s = ListsScreen::new();
        s.replace(vec![summary(1, 0), summary(2, 0), summary(3, 0)]);
        s.cursor = 2;
        s.remove_id(3);
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn upsert_replaces_existing_by_id() {
        let mut s = ListsScreen::new();
        s.replace(vec![summary(1, 0)]);
        let mut updated = summary(1, 5);
        updated.title = "renamed".into();
        s.upsert(updated);
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].title, "renamed");
        assert_eq!(s.items[0].member_count, 5);
    }

    #[test]
    fn upsert_appends_when_new() {
        let mut s = ListsScreen::new();
        s.replace(vec![summary(1, 0)]);
        s.upsert(summary(2, 0));
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn input_insert_and_backspace() {
        let mut input = ListsInput::new(ListsInputKind::Create);
        input.insert_char('a');
        input.insert_char('b');
        assert_eq!(input.value(), "ab");
        input.backspace();
        assert_eq!(input.value(), "a");
    }

    #[test]
    fn input_value_trims_whitespace() {
        let mut input = ListsInput::new(ListsInputKind::Create);
        for c in "  hi  ".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.value(), "hi");
    }
}
