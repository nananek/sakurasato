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
    /// 開いた時点での起動元 Focus。`Esc` で閉じたとき復帰先を決める。
    /// 現状は Timeline からしか開けないので実用上常に `Timeline` だが、
    /// 将来 Profile などから開いたときの戻り先誤りを防ぐため記録しておく。
    pub origin: crate::app::Focus,
}

impl NoteDetailScreen {
    /// `note` の snapshot を取って開く。`sensitive == true` の Note は添付
    /// 全件 blur 状態で開き、`s` で個別 reveal させる。
    #[must_use]
    pub fn new(note: TimelineNote, origin: crate::app::Focus) -> Self {
        let initial_reveal = !note.sensitive;
        let n = note.attachments.len();
        Self {
            note,
            scroll: 0,
            revealed: vec![initial_reveal; n],
            selected_attachment: 0,
            origin,
        }
    }

    /// 1 行スクロール下。`scroll` がコンテンツ末尾を超えると空白画面に
    /// なるため、Note の構成要素から見積もった行数で上限を掛ける ──
    /// wrap 後の正確な行数は描画前に分からないが、推定値より深く潜る
    /// ことは無いはず。
    pub fn scroll_down(&mut self) {
        let max = self.estimated_max_scroll();
        self.scroll = self.scroll.saturating_add(1).min(max);
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// `scroll` の上限見積もり。Note の構成要素 (時刻 / permalink / CW /
    /// 本文行 / リアクション行 / 絵文字行 / 添付一覧) を unwrap した素の
    /// 行数で算出 ── 実描画では terminal 幅で wrap が増えるが、wrap を
    /// 過剰に多く見積もると無効スクロールが発生するので「下界」側の
    /// 見積もりに留める。
    ///
    /// TODO (PR #154 round-2 review P4): 長 URL や CJK 長文を含む Note では
    /// wrap で物理行数が増え、推定値が小さすぎて最終行に到達できないことが
    /// ある。render 側で実描画の行数を `state` にフィードバックする仕組みを
    /// 別 issue で検討する。
    fn estimated_max_scroll(&self) -> usize {
        // 時刻 1 + permalink 0/1
        let mut n: usize = 1 + usize::from(self.note.url.is_some());
        // CW 行 + 区切り 1
        if self.note.summary.as_deref().is_some_and(|s| !s.is_empty()) {
            n += 2;
        }
        // 本文 (HTML strip 済み行数)
        let body = crate::content::to_plain_text(&self.note.content)
            .lines()
            .count()
            .max(1); // 空文でも `(empty)` 1 行
        n += body;
        // リアクション行 (区切り + 本体)
        if !self.note.reactions.is_empty() {
            n += 2;
        }
        // 絵文字行: ヘッダ 1 + emoji 件数を 1 行あたり 8 個と見積もった行数。
        // round-4 review F3: 大量の emoji を持つ Note で過小推定にならない
        // ようにする (= 折りたたみ後の wrap で行数が増えるため)。
        if !self.note.emojis.is_empty() {
            let emoji_lines = self.note.emojis.len().div_ceil(8).max(1);
            n += 1 + emoji_lines;
        }
        // 添付ヘッダ + 各 1 行
        if !self.note.attachments.is_empty() {
            n += 2 + self.note.attachments.len();
        }
        n.saturating_sub(1)
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

    use crate::app::Focus;

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
        let s = NoteDetailScreen::new(make_note(true, 2), Focus::Timeline);
        assert_eq!(s.revealed, vec![false, false]);
    }

    #[test]
    fn new_non_sensitive_starts_revealed() {
        let s = NoteDetailScreen::new(make_note(false, 3), Focus::Timeline);
        assert_eq!(s.revealed, vec![true, true, true]);
    }

    #[test]
    fn toggle_flips_current_only() {
        let mut s = NoteDetailScreen::new(make_note(true, 3), Focus::Timeline);
        s.selected_attachment = 1;
        s.toggle_reveal();
        assert_eq!(s.revealed, vec![false, true, false]);
        s.toggle_reveal();
        assert_eq!(s.revealed, vec![false, false, false]);
    }

    #[test]
    fn select_next_wraps() {
        let mut s = NoteDetailScreen::new(make_note(false, 3), Focus::Timeline);
        s.select_next_attachment();
        s.select_next_attachment();
        s.select_next_attachment();
        assert_eq!(s.selected_attachment, 0); // wrapped
    }

    #[test]
    fn select_prev_wraps_backward() {
        let mut s = NoteDetailScreen::new(make_note(false, 3), Focus::Timeline);
        s.select_prev_attachment();
        assert_eq!(s.selected_attachment, 2); // wrapped to last
    }

    #[test]
    fn empty_attachments_select_is_noop() {
        let mut s = NoteDetailScreen::new(make_note(false, 0), Focus::Timeline);
        s.select_next_attachment();
        s.select_prev_attachment();
        s.toggle_reveal();
        assert_eq!(s.selected_attachment, 0);
        assert!(s.revealed.is_empty());
    }

    #[test]
    fn scroll_saturates_at_zero() {
        let mut s = NoteDetailScreen::new(make_note(false, 0), Focus::Timeline);
        s.scroll_up();
        assert_eq!(s.scroll, 0);
        s.scroll_down();
        // 短い note (body 1 行) では max_scroll=1。さらに down しても capped。
        s.scroll_down();
        s.scroll_down();
        assert!(s.scroll <= s.estimated_max_scroll());
    }

    #[test]
    fn scroll_down_caps_at_estimated_max() {
        // round-1 review Finding 2: 末尾を超えても空白画面にならないよう
        // `scroll_down` で見積もり上限にクランプする。
        let note = make_note(false, 0);
        let mut s = NoteDetailScreen::new(note, Focus::Timeline);
        // 1000 回押しても見積もり上限を超えない。
        for _ in 0..1000 {
            s.scroll_down();
        }
        assert_eq!(s.scroll, s.estimated_max_scroll());
        // scroll_up は素直に減る。
        s.scroll_up();
        assert_eq!(s.scroll, s.estimated_max_scroll().saturating_sub(1));
    }

    #[test]
    fn estimated_max_scroll_grows_with_content() {
        let small = NoteDetailScreen::new(make_note(false, 0), Focus::Timeline);
        let big = NoteDetailScreen::new(make_note(false, 4), Focus::Timeline);
        assert!(big.estimated_max_scroll() > small.estimated_max_scroll());
    }
}
