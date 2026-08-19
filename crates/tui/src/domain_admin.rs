//! 連合ドメインブロック PR7 (計画書 §6.7): ドメイン管理画面の状態保持。
//!
//! 一覧画面 ([`DomainAdminScreen`]、`:domains` で開く) → 詳細画面
//! ([`DomainDetailScreen`]、`Enter` で push) の 2 段構成。既存
//! `follow_requests.rs` (単純な縦リスト) / `follow_list.rs` (タブ切替) の
//! パターンを踏襲する。server 側は host あたりのフォロー件数が少数の
//! お一人様サーバ規模を前提にページネーション無しで一括返すため、
//! `follow_list::ModePage` のような追加ページ取得の仕組みは持たない。

use crate::client::{DomainDetailResponse, DomainFollowEntry, DomainSummary};

/// 一覧画面の state。[`crate::app::App::domain_admin`] が保持する。
#[derive(Debug, Clone, Default)]
pub struct DomainAdminScreen {
    pub items: Vec<DomainSummary>,
    pub cursor: usize,
    pub top: usize,
    pub fetching: bool,
}

impl DomainAdminScreen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, items: Vec<DomainSummary>) {
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
    pub fn current(&self) -> Option<&DomainSummary> {
        self.items.get(self.cursor)
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

/// `DomainDetailScreen` 内のタブ。`follow_list::FollowListMode` と同型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DomainFollowTab {
    #[default]
    Following,
    Followers,
}

impl DomainFollowTab {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Followers => "followers",
        }
    }

    /// `t` トグルで反対タブに変える。
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Following => Self::Followers,
            Self::Followers => Self::Following,
        }
    }
}

/// 詳細画面の state。[`crate::app::App::domain_detail`] が保持する。
#[derive(Debug, Clone)]
pub struct DomainDetailScreen {
    pub host: String,
    pub severity: Option<String>,
    pub reason: Option<String>,
    pub known_actor_count: i64,
    pub accepted_following_count: i64,
    pub accepted_followers_count: i64,
    pub pending_following_count: i64,
    pub pending_followers_count: i64,
    pub following: Vec<DomainFollowEntry>,
    pub followers: Vec<DomainFollowEntry>,
    pub tab: DomainFollowTab,
    pub selected: usize,
    pub top: usize,
}

impl DomainDetailScreen {
    #[must_use]
    pub fn from_response(resp: DomainDetailResponse) -> Self {
        Self {
            host: resp.host,
            severity: resp.severity,
            reason: resp.reason,
            known_actor_count: resp.known_actor_count,
            accepted_following_count: resp.accepted_following_count,
            accepted_followers_count: resp.accepted_followers_count,
            pending_following_count: resp.pending_following_count,
            pending_followers_count: resp.pending_followers_count,
            following: resp.following,
            followers: resp.followers,
            tab: DomainFollowTab::default(),
            selected: 0,
            top: 0,
        }
    }

    /// 現在タブのエントリ一覧。
    #[must_use]
    pub fn current_entries(&self) -> &[DomainFollowEntry] {
        match self.tab {
            DomainFollowTab::Following => &self.following,
            DomainFollowTab::Followers => &self.followers,
        }
    }

    /// 現在選択中の Entry (`Enter` で Profile push する候補)。
    #[must_use]
    pub fn current_entry(&self) -> Option<&DomainFollowEntry> {
        self.current_entries().get(self.selected)
    }

    /// `t` トグル ── 反対タブに切り替えてカーソルをリセット。
    pub fn toggle_tab(&mut self) {
        self.tab = self.tab.flipped();
        self.selected = 0;
        self.top = 0;
    }

    pub fn select_next(&mut self) {
        let len = self.current_entries().len();
        if len == 0 {
            return;
        }
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn ensure_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + v {
            self.top = self.selected + 1 - v;
        }
    }

    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.severity.as_deref() == Some("suspend")
    }

    #[must_use]
    pub fn is_silenced(&self) -> bool {
        self.severity.as_deref() == Some("silence")
    }

    /// 一覧 / status bar 表示用の短いラベル。
    #[must_use]
    pub fn state_label(&self) -> &'static str {
        match self.severity.as_deref() {
            Some("suspend") => "SUSPENDED",
            Some("silence") => "SILENCED",
            _ => "-",
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::client::ActorProfile;

    fn summary(host: &str) -> DomainSummary {
        DomainSummary {
            host: host.into(),
            actor_count: 1,
            severity: None,
        }
    }

    #[test]
    fn admin_cursor_clamps_after_replace() {
        let mut s = DomainAdminScreen::new();
        s.replace(vec![
            summary("a.test"),
            summary("b.test"),
            summary("c.test"),
        ]);
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 2);
        s.replace(vec![summary("z.test")]);
        assert_eq!(s.cursor, 0, "cursor must clamp to new length");
    }

    #[test]
    fn admin_select_next_stops_at_end() {
        let mut s = DomainAdminScreen::new();
        s.replace(vec![summary("a.test"), summary("b.test")]);
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn admin_current_is_none_when_empty() {
        let s = DomainAdminScreen::new();
        assert!(s.current().is_none());
    }

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

    fn follow_entry(follow_id: i64, actor_id: i64, user: &str, host: &str) -> DomainFollowEntry {
        DomainFollowEntry {
            follow_id,
            follow_state: "accepted".into(),
            follow_created_at: Utc::now(),
            actor: actor(actor_id, user, host),
        }
    }

    fn detail() -> DomainDetailScreen {
        DomainDetailScreen::from_response(DomainDetailResponse {
            host: "example.test".into(),
            severity: None,
            reason: None,
            known_actor_count: 2,
            accepted_following_count: 1,
            accepted_followers_count: 1,
            pending_following_count: 0,
            pending_followers_count: 0,
            following: vec![follow_entry(1, 10, "alice", "example.test")],
            followers: vec![follow_entry(2, 11, "bob", "example.test")],
        })
    }

    #[test]
    fn toggle_tab_resets_cursor() {
        let mut s = detail();
        s.select_next();
        s.toggle_tab();
        assert_eq!(s.tab, DomainFollowTab::Followers);
        assert_eq!(s.selected, 0);
        assert_eq!(s.top, 0);
    }

    #[test]
    fn current_entry_matches_tab() {
        let s = detail();
        assert_eq!(
            s.current_entry()
                .map(|e| e.actor.preferred_username.as_str()),
            Some("alice")
        );
    }

    #[test]
    fn state_label_reflects_severity() {
        let mut s = detail();
        assert_eq!(s.state_label(), "-");
        s.severity = Some("silence".into());
        assert_eq!(s.state_label(), "SILENCED");
        assert!(s.is_silenced());
        s.severity = Some("suspend".into());
        assert_eq!(s.state_label(), "SUSPENDED");
        assert!(s.is_suspended());
    }
}
