//! Note 詳細モーダルの state。Issue #133 (3)。
//!
//! `Focus::NoteDetail` の間だけ [`crate::app::App::note_detail`] に `Some`。
//! Timeline で `Enter` を押した瞬間の Note を snapshot として持ち、モーダル
//! 中に Timeline が refresh で動いても表示が動かないようにする ──
//! ユーザが「今この note を読んでいる」という UI 状態を尊重する。
//!
//! 折りたたみは無し (= [`crate::ui::render_note_detail`] が `max_body=0`
//! で全本文を出す)。スクロールはモーダル内独立。
//!
//! 添付ごとの sensitive blur 解除フラグ (`revealed`) は配列 index 別に
//! 持つ ── `s` で現在カーソルの添付だけ reveal する操作を可能にする。

use crate::client::TimelineNote;

/// Note 詳細モーダル全体の state。
#[derive(Debug, Clone)]
pub struct NoteDetailScreen {
    /// 表示中の Note (Timeline から開いた時点の clone)。
    pub note: TimelineNote,
    /// モーダル本文のスクロール位置 (上から何行スキップして描画するか)。
    pub scroll: usize,
    /// 添付ごとの blur 解除フラグ (initial = `!note.sensitive` で個別 toggle)。
    /// 添付 index と対応する。`note.sensitive == false` なら全 true 初期値。
    pub revealed: Vec<bool>,
    /// プレビュー対象の添付 index ── 上下キーで切替。範囲外は無視。
    pub selected_attachment: usize,
}

impl NoteDetailScreen {
    /// `note` の snapshot を取って開く。`sensitive == true` の Note は添付
    /// 全件 blur 状態で開き、`s` で個別 reveal させる。
    #[must_use]
    pub fn new(note: TimelineNote) -> Self {
        let initial_reveal = !note.sensitive;
        let n = note.attachments.len();
        Self {
            note,
            scroll: 0,
            revealed: vec![initial_reveal; n],
            selected_attachment: 0,
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// 現在カーソルにある添付の reveal を toggle する。
    pub fn toggle_reveal(&mut self) {
        if let Some(flag) = self.revealed.get_mut(self.selected_attachment) {
            *flag = !*flag;
        }
    }

    pub fn select_next_attachment(&mut self) {
        let len = self.note.attachments.len();
        if len > 0 {
            self.selected_attachment = (self.selected_attachment + 1) % len;
        }
    }

    pub fn select_prev_attachment(&mut self) {
        let len = self.note.attachments.len();
        if len > 0 {
            self.selected_attachment = (self.selected_attachment + len - 1) % len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Attachment, TimelineNote};
    use chrono::TimeZone;

    fn make_note(sensitive: bool, n_attach: usize) -> TimelineNote {
        TimelineNote {
            id: 1,
            ap_id: "https://e.example/notes/1".into(),
            url: None,
            actor_id: 1,
            actor_ap_id: "https://e.example/users/x".into(),
            actor_preferred_username: "x".into(),
            actor_display_name: None,
            actor_icon_url: None,
            content: "hi".into(),
            summary: None,
            language: None,
            visibility: "public".into(),
            sensitive,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            published_at: chrono::Utc.timestamp_opt(0, 0).unwrap(),
            is_local: true,
            reactions: vec![],
            attachments: (0..n_attach)
                .map(|i| Attachment {
                    url: format!("https://e.example/m/{i}"),
                    media_type: Some("image/webp".into()),
                    alt: None,
                    width: None,
                    height: None,
                })
                .collect(),
            emojis: vec![],
        }
    }

    #[test]
    fn new_sensitive_starts_blurred() {
        let s = NoteDetailScreen::new(make_note(true, 2));
        assert_eq!(s.revealed, vec![false, false]);
    }

    #[test]
    fn new_non_sensitive_starts_revealed() {
        let s = NoteDetailScreen::new(make_note(false, 3));
        assert_eq!(s.revealed, vec![true, true, true]);
    }

    #[test]
    fn toggle_flips_current_only() {
        let mut s = NoteDetailScreen::new(make_note(true, 3));
        s.selected_attachment = 1;
        s.toggle_reveal();
        assert_eq!(s.revealed, vec![false, true, false]);
        s.toggle_reveal();
        assert_eq!(s.revealed, vec![false, false, false]);
    }

    #[test]
    fn select_next_wraps() {
        let mut s = NoteDetailScreen::new(make_note(false, 3));
        s.select_next_attachment();
        s.select_next_attachment();
        s.select_next_attachment();
        assert_eq!(s.selected_attachment, 0); // wrapped
    }

    #[test]
    fn select_prev_wraps_backward() {
        let mut s = NoteDetailScreen::new(make_note(false, 3));
        s.select_prev_attachment();
        assert_eq!(s.selected_attachment, 2); // wrapped to last
    }

    #[test]
    fn empty_attachments_select_is_noop() {
        let mut s = NoteDetailScreen::new(make_note(false, 0));
        s.select_next_attachment();
        s.select_prev_attachment();
        s.toggle_reveal();
        assert_eq!(s.selected_attachment, 0);
        assert!(s.revealed.is_empty());
    }

    #[test]
    fn scroll_saturates_at_zero() {
        let mut s = NoteDetailScreen::new(make_note(false, 0));
        s.scroll_up();
        assert_eq!(s.scroll, 0);
        s.scroll_down();
        s.scroll_down();
        s.scroll_up();
        assert_eq!(s.scroll, 1);
    }
}
