//! 絵文字検索モーダルの state。
//!
//! reaction prompt や compose で `Ctrl-E` を押すと開く。モーダル内に独立した
//! search buffer を持ち、全文字を受理する。検索は **部分一致** (lowercase
//! substring) で、前方一致のものを上位に並べる軽い重み付けを行う。
//!
//! ## キー操作 (モーダル open 中)
//!
//! - 通常文字 / Backspace ── search buffer を編集
//! - `↑` / `↓` (or `Ctrl-P` / `Ctrl-N`) ── 候補移動
//! - `Enter` ── 選択中 shortcode を `:foo:` で挿入し閉じる
//! - `Esc` ── 何も挿入せず閉じる
//!
//! 候補 0 件でも閉じない (= search buffer を消せば全候補が戻る)。

use crate::client::EmojiItem;

/// 候補表示の最大件数 (viewport から溢れる ぶんはスクロール)。
pub const VISIBLE_MAX: usize = 8;
/// 初回 fetch で取得する件数。server 側 `MAX_LIMIT = 100` と揃える。
pub const FETCH_LIMIT: i64 = 100;

#[derive(Debug, Clone, Default)]
pub struct EmojiSuggestState {
    /// 初回 fetch で取得した全候補 (= filter 対象の母集団)。
    pub all: Vec<EmojiItem>,
    /// `query` を `all` に当てた可視リスト (前方一致 → 部分一致の順)。
    pub filtered: Vec<EmojiItem>,
    /// `filtered` 内のカーソル位置。
    pub cursor: usize,
    /// 検索 buffer。reaction prompt buffer とは独立で、モーダル内専用。
    pub query: String,
}

impl EmojiSuggestState {
    #[must_use]
    pub fn open(items: Vec<EmojiItem>) -> Self {
        let mut s = Self {
            all: items,
            filtered: Vec::new(),
            cursor: 0,
            query: String::new(),
        };
        s.recompute();
        s
    }

    /// 現在選択中の候補。`filtered` が空なら None。
    #[must_use]
    pub fn current(&self) -> Option<&EmojiItem> {
        self.filtered.get(self.cursor)
    }

    /// search buffer に 1 文字追加。
    pub fn insert_char(&mut self, c: char) {
        self.query.push(c);
        self.recompute();
    }

    /// search buffer から末尾 1 文字削除 (`pop()`)。
    pub fn backspace(&mut self) {
        self.query.pop();
        self.recompute();
    }

    pub fn select_next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.filtered.len();
    }

    pub fn select_prev(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.cursor == 0 {
            self.cursor = self.filtered.len() - 1;
        } else {
            self.cursor -= 1;
        }
    }

    /// query から `filtered` を再計算。前方一致をまとめて先頭に置き、その後に
    /// 部分一致 (前方一致でないもの) を続ける。重複は無し (前方一致は部分
    /// 一致を含む集合なので、後段で除外)。
    fn recompute(&mut self) {
        let q = self.query.to_ascii_lowercase();
        if q.is_empty() {
            self.filtered = self.all.clone();
            self.cursor = 0;
            return;
        }
        let mut prefix_hits: Vec<EmojiItem> = Vec::new();
        let mut substr_hits: Vec<EmojiItem> = Vec::new();
        for e in &self.all {
            let lc = e.shortcode.to_ascii_lowercase();
            if lc.starts_with(&q) {
                prefix_hits.push(e.clone());
            } else if lc.contains(&q) {
                substr_hits.push(e.clone());
            } else {
                // aliases にもマッチするなら部分一致扱いで救済。
                for alias in &e.aliases {
                    if alias.to_ascii_lowercase().contains(&q) {
                        substr_hits.push(e.clone());
                        break;
                    }
                }
            }
        }
        prefix_hits.extend(substr_hits);
        self.filtered = prefix_hits;
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(shortcode: &str) -> EmojiItem {
        EmojiItem {
            shortcode: shortcode.into(),
            url: format!("https://x.test/media/emoji/local/{shortcode}.webp"),
            media_type: "image/webp".into(),
            category: None,
            aliases: vec![],
        }
    }

    fn item_with_alias(shortcode: &str, alias: &str) -> EmojiItem {
        EmojiItem {
            shortcode: shortcode.into(),
            url: format!("https://x.test/media/emoji/local/{shortcode}.webp"),
            media_type: "image/webp".into(),
            category: None,
            aliases: vec![alias.into()],
        }
    }

    #[test]
    fn open_with_empty_query_shows_all() {
        let s = EmojiSuggestState::open(vec![item("a"), item("b"), item("c")]);
        assert_eq!(s.filtered.len(), 3);
    }

    #[test]
    fn insert_char_filters_by_substring() {
        let mut s = EmojiSuggestState::open(vec![
            item("happy"),
            item("sad"),
            item("bonfire"),
            item("firework"),
        ]);
        s.insert_char('f');
        s.insert_char('i');
        s.insert_char('r');
        // `firework` (前方一致) → `bonfire` (部分一致) の順。
        let codes: Vec<&str> = s.filtered.iter().map(|e| e.shortcode.as_str()).collect();
        assert_eq!(codes, vec!["firework", "bonfire"]);
    }

    #[test]
    fn backspace_restores_candidates() {
        let mut s = EmojiSuggestState::open(vec![item("happy"), item("sad")]);
        s.insert_char('z');
        assert!(s.filtered.is_empty());
        s.backspace();
        assert_eq!(s.filtered.len(), 2);
    }

    #[test]
    fn case_insensitive_match() {
        let mut s = EmojiSuggestState::open(vec![item("Happy"), item("HOORAY"), item("sad")]);
        s.insert_char('h');
        let codes: Vec<&str> = s.filtered.iter().map(|e| e.shortcode.as_str()).collect();
        assert_eq!(codes, vec!["Happy", "HOORAY"]);
    }

    #[test]
    fn aliases_hit_as_substring() {
        let mut s = EmojiSuggestState::open(vec![
            item_with_alias("partying-face", "celebrate"),
            item("sad"),
        ]);
        s.insert_char('c');
        s.insert_char('e');
        s.insert_char('l');
        assert_eq!(s.filtered.len(), 1);
        assert_eq!(s.filtered[0].shortcode, "partying-face");
    }

    #[test]
    fn cursor_wraps() {
        let mut s = EmojiSuggestState::open(vec![item("a"), item("b"), item("c")]);
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 0);
        s.select_prev();
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn empty_result_does_not_panic_on_navigation() {
        let mut s = EmojiSuggestState::open(vec![item("happy")]);
        s.insert_char('z');
        assert!(s.filtered.is_empty());
        s.select_next();
        s.select_prev();
        assert!(s.current().is_none());
    }
}
