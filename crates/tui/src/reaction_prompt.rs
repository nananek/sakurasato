//! リアクション送信用の 1 行入力 (M8 PR3)。
//!
//! `App.reaction_prompt: Option<ReactionPrompt>` で保持する。`Some` の間は
//! `Focus::ReactionPrompt` になり、キー入力は本構造の `buffer` に流れる。
//! Enter で `Action::SendReaction { note_id, content }` を発火し、runtime が
//! `POST /api/v1/reactions` を叩いてからプロンプトを閉じる。
//!
//! UI 上は timeline と status line の間に 1 行 overlay として表示する
//! (`render_reaction_prompt`)。content には Unicode emoji と `:shortcode:`
//! 両方を受け付け、`:shortcode@host:` (= remote 絵文字直指定) はサーバ側で
//! 400 が返るのでメッセージはそのまま status line に出る。

/// 入力中のリアクション。`note_id` は対象 Note (= timeline の `selected` 時点
/// で確定)、`buffer` は逐次更新される入力。
#[derive(Debug, Clone)]
pub struct ReactionPrompt {
    pub note_id: i64,
    pub buffer: String,
}

impl ReactionPrompt {
    /// `content` の最大長。サーバ側 (`local_api::reactions::CONTENT_MAX`) と
    /// 揃える ── ユーザに「打ち過ぎ」を画面で見せて止める。
    pub const MAX_CHARS: usize = 256;

    pub fn new(note_id: i64) -> Self {
        Self {
            note_id,
            buffer: String::new(),
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

    pub fn is_empty(&self) -> bool {
        self.buffer.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_caps_at_max() {
        let mut p = ReactionPrompt::new(1);
        for _ in 0..(ReactionPrompt::MAX_CHARS + 10) {
            p.insert_char('x');
        }
        assert_eq!(p.buffer.chars().count(), ReactionPrompt::MAX_CHARS);
    }

    #[test]
    fn backspace_pops_one_char() {
        let mut p = ReactionPrompt::new(1);
        p.insert_char(':');
        p.insert_char('a');
        p.backspace();
        assert_eq!(p.buffer, ":");
    }

    #[test]
    fn unicode_grapheme_counted_as_chars() {
        // 絵文字 1 文字は char としても 1 (single codepoint emoji)。サロゲートペア
        // や合字は将来検討。
        let mut p = ReactionPrompt::new(1);
        p.insert_char('👍');
        assert_eq!(p.buffer.chars().count(), 1);
    }

    #[test]
    fn is_empty_treats_whitespace_as_empty() {
        let mut p = ReactionPrompt::new(1);
        assert!(p.is_empty());
        p.insert_char(' ');
        assert!(p.is_empty());
        p.insert_char('a');
        assert!(!p.is_empty());
    }
}
