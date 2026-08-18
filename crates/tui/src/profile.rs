//! M13 PR4 (Issue #79): Profile 画面の状態保持。
//!
//! `Focus::Profile` 中だけ表示される。1 つの actor について「DB から引いた
//! プロフィール」+「local actor との `Relationship`」+「直近の Note 一覧」を
//! 抱える。
//!
//! ## 画面遷移モデル
//!
//! Timeline → Profile (push) → Esc/q → Timeline (pop) の単純 1 段で動かす。
//! 内部は `Vec<ProfileScreen>` 形式で持たせており、PR5 で `:open @user` から
//! の連鎖や `FollowList` → Profile → `FollowList` の往復に拡張できる。
//!
//! ## API 非同期と楽観表示
//!
//! Profile push 時は actor JSON が返ってから switch する (= relationship と
//! 一緒に確定状態で渡る)。Note 一覧は遅延 fetch で空配列開始でも問題ないが、
//! PR4 では「actor + notes 両方そろってから表示」を取り、ローディング体験を
//! 単純化する (= 部分表示を許すと clamp / カーソル整合性が増える)。
//!
//! follow toggle 時は relationship のみ再取得して上書きする。

use crate::client::{ActorProfile, Relationship, TimelineNote};

/// Profile 画面 1 枚分の state。
#[derive(Debug, Clone)]
pub struct ProfileScreen {
    pub actor: ActorProfile,
    pub relationship: Relationship,
    pub notes: Vec<TimelineNote>,
    /// `actor/{id}/notes` ページネーション用 (= 次取得時の `before_id`)。
    /// `None` で末尾到達。
    pub next_before_id: Option<i64>,
    /// notes が空配列で返って「もう続きが無い」と確定したか。
    pub notes_exhausted: bool,
    /// notes 一覧で選択中の index。0 = 先頭。空配列のときも 0 (= 範囲外でも
    /// 表示時にチェックする)。
    pub selected_note: usize,
    /// note 一覧のスクロール先頭 index。
    pub note_top: usize,
}

impl ProfileScreen {
    #[must_use]
    pub fn new(
        actor: ActorProfile,
        relationship: Relationship,
        notes: Vec<TimelineNote>,
        next_before_id: Option<i64>,
    ) -> Self {
        let notes_exhausted = notes.is_empty();
        Self {
            actor,
            relationship,
            notes,
            next_before_id,
            notes_exhausted,
            selected_note: 0,
            note_top: 0,
        }
    }

    /// 表示用 acct (`@user@host`)。
    #[must_use]
    pub fn acct(&self) -> String {
        format!("@{}@{}", self.actor.preferred_username, self.actor.host)
    }

    /// 表示用フルネーム (`display_name` があれば先頭、なければ preferred username)。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.actor
            .display_name
            .clone()
            .unwrap_or_else(|| self.actor.preferred_username.clone())
    }

    /// 現在の follow 状態に基づく短いラベル (status bar / banner 用)。
    /// follow toggle 後の挙動 (Follow を送るか Unfollow を送るか) も同期して判断する。
    #[must_use]
    pub fn relationship_label(&self) -> &'static str {
        match (
            self.relationship.following,
            self.relationship.follow_state.as_deref(),
        ) {
            (true, _) => "following",
            (_, Some("pending")) => "follow requested",
            (_, Some("rejected")) => "follow rejected",
            _ => "not following",
        }
    }

    /// `f` キーで「Unfollow を撃つべきか (= follow row が存在する)」を判断する。
    /// pending / accepted のとき true。rejected / 行無しのときは「follow を撃つ」。
    #[must_use]
    pub fn has_active_follow(&self) -> bool {
        matches!(
            self.relationship.follow_state.as_deref(),
            Some("pending" | "accepted")
        )
    }

    /// `b` キー (ユーザーブロック PR6) で「Unblock を撃つべきか」を判断する。
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.relationship.is_blocked
    }

    /// notes リストでのキャレット 1 件下移動。空配列 / 末尾では no-op。
    pub fn select_next_note(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        if self.selected_note + 1 < self.notes.len() {
            self.selected_note += 1;
        }
    }

    /// notes リストでのキャレット 1 件上移動。
    pub fn select_prev_note(&mut self) {
        if self.selected_note > 0 {
            self.selected_note -= 1;
        }
    }

    /// notes ページ追記。空配列なら `notes_exhausted = true`。
    pub fn append_older_notes(&mut self, mut more: Vec<TimelineNote>, next_before_id: Option<i64>) {
        if more.is_empty() {
            self.notes_exhausted = true;
            return;
        }
        self.notes.append(&mut more);
        self.next_before_id = next_before_id;
    }

    /// follow toggle 後の更新。`Relationship` を入れ替えるだけ。
    pub fn update_relationship(&mut self, rel: Relationship) {
        self.relationship = rel;
    }

    /// viewport に `selected_note` を入れる。`viewport_rows` は note 概算
    /// 1 件 ≒ 3 行で割った件数想定。最低 1 件。
    pub fn ensure_note_visible(&mut self, viewport_items: usize) {
        let v = viewport_items.max(1);
        if self.selected_note < self.note_top {
            self.note_top = self.selected_note;
        } else if self.selected_note >= self.note_top + v {
            self.note_top = self.selected_note + 1 - v;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

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

    fn rel(following: bool, state: Option<&str>) -> Relationship {
        Relationship {
            following,
            follow_state: state.map(ToOwned::to_owned),
            followed_by: false,
            follow_id: None,
            is_blocked: false,
            block_id: None,
            is_blocked_by: false,
        }
    }

    fn note(id: i64) -> TimelineNote {
        TimelineNote {
            id,
            ap_id: format!("https://x.test/notes/{id}"),
            url: None,
            actor_id: 1,
            actor_ap_id: "https://x.test/users/bob".into(),
            actor_preferred_username: "bob".into(),
            actor_display_name: None,
            actor_icon_url: None,
            content: format!("hi #{id}"),
            summary: None,
            language: None,
            visibility: "public".into(),
            sensitive: false,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            published_at: Utc::now(),
            is_local: false,
            reactions: Vec::new(),
            attachments: Vec::new(),
            emojis: Vec::new(),
            announce_count: 0,
            viewer_renoted: false,
            renote: None,
        }
    }

    #[test]
    fn relationship_label_follows_state() {
        let a = actor(1, "bob", "example.test");
        let s = ProfileScreen::new(a.clone(), rel(true, Some("accepted")), vec![], None);
        assert_eq!(s.relationship_label(), "following");
        let s = ProfileScreen::new(a.clone(), rel(false, Some("pending")), vec![], None);
        assert_eq!(s.relationship_label(), "follow requested");
        let s = ProfileScreen::new(a.clone(), rel(false, Some("rejected")), vec![], None);
        assert_eq!(s.relationship_label(), "follow rejected");
        let s = ProfileScreen::new(a, rel(false, None), vec![], None);
        assert_eq!(s.relationship_label(), "not following");
    }

    #[test]
    fn has_active_follow_for_pending_and_accepted() {
        let a = actor(1, "bob", "example.test");
        let s = ProfileScreen::new(a.clone(), rel(true, Some("accepted")), vec![], None);
        assert!(s.has_active_follow());
        let s = ProfileScreen::new(a.clone(), rel(false, Some("pending")), vec![], None);
        assert!(s.has_active_follow());
        let s = ProfileScreen::new(a.clone(), rel(false, Some("rejected")), vec![], None);
        assert!(!s.has_active_follow());
        let s = ProfileScreen::new(a, rel(false, None), vec![], None);
        assert!(!s.has_active_follow());
    }

    #[test]
    fn note_navigation_clamped() {
        let a = actor(1, "bob", "example.test");
        let mut s = ProfileScreen::new(
            a,
            rel(false, None),
            vec![note(3), note(2), note(1)],
            Some(1),
        );
        assert_eq!(s.selected_note, 0);
        s.select_next_note();
        assert_eq!(s.selected_note, 1);
        s.select_next_note();
        s.select_next_note();
        assert_eq!(s.selected_note, 2);
        s.select_prev_note();
        assert_eq!(s.selected_note, 1);
    }

    #[test]
    fn append_older_notes_marks_exhausted_on_empty() {
        let a = actor(1, "bob", "example.test");
        let mut s = ProfileScreen::new(a, rel(false, None), vec![note(3)], Some(3));
        s.append_older_notes(vec![], None);
        assert!(s.notes_exhausted);
        assert_eq!(s.notes.len(), 1);
    }

    #[test]
    fn ensure_note_visible_scrolls() {
        let a = actor(1, "bob", "example.test");
        let notes: Vec<TimelineNote> = (0..20).map(|i| note(20 - i)).collect();
        let mut s = ProfileScreen::new(a, rel(false, None), notes, None);
        s.selected_note = 10;
        s.ensure_note_visible(4);
        assert_eq!(s.note_top, 7); // 10 - 4 + 1
        s.selected_note = 2;
        s.ensure_note_visible(4);
        assert_eq!(s.note_top, 2);
    }
}
