//! Issue #101: 絵文字 shortcode サジェスト popup の state。
//!
//! reaction prompt で `:` を打った瞬間に [`crate::runtime`] が
//! `list_emojis("", LIMIT)` を 1 回叩いて [`EmojiSuggestState`] に詰め、
//! 以降 popup 表示中は **client-side filter** で prefix 絞り込みをする
//! (= API 再呼び出ししないことで入力レイテンシをゼロにする)。
//!
//! prefix が初回取得時より長くなって候補 0 件になるケースは「shortcode が
//! `LIMIT` を超えて存在し、初回取得に入っていなかった」可能性がある。お一人様
//! サーバでこの状況になるのは稀 (= shortcode は手動 import で数十個程度)、
//! 必要なら refetch のキー (例: `Ctrl-R`) を後追いで足す。
//!
//! 候補 0 件のときは popup を閉じる方針 (= 邪魔にならない)。
//!
//! ## キー操作 (popup 表示中)
//!
//! - `↑` / `↓` / `Ctrl-P` / `Ctrl-N` ── カーソル移動
//! - `Tab` / `Enter` ── 選択中 shortcode を `:foo:` で挿入し popup 閉じる
//! - `Esc` ── popup 閉じる (テキスト挿入なし)
//! - 通常文字入力 ── prefix 更新 + 再フィルタ
//! - `:` ── popup 閉じる (= ユーザが自分で shortcode を書き終わった)

use crate::client::EmojiItem;

/// popup 1 ページに表示する最大候補数。多すぎると視界が埋まる。
pub const VISIBLE_MAX: usize = 8;
/// API 初回 fetch の上限。server 側 `MAX_LIMIT` = 100 と揃える。
pub const FETCH_LIMIT: i64 = 100;

#[derive(Debug, Clone, Default)]
pub struct EmojiSuggestState {
    /// 初回 fetch で取得した全候補 (= filter 対象の母集団)。
    pub all: Vec<EmojiItem>,
    /// `all` を `prefix` で絞った可視リスト。
    pub filtered: Vec<EmojiItem>,
    /// カーソル位置 (`filtered` のインデックス)。
    pub cursor: usize,
    /// 現在の prefix (ASCII-lowercase)。
    pub prefix: String,
}

impl EmojiSuggestState {
    /// 初回 fetch 結果で開く。
    #[must_use]
    pub fn open(items: Vec<EmojiItem>, prefix: &str) -> Self {
        let mut s = Self {
            all: items,
            filtered: Vec::new(),
            cursor: 0,
            prefix: String::new(),
        };
        s.set_prefix(prefix);
        s
    }

    /// 現在選択中の候補。`filtered` が空なら None。
    #[must_use]
    pub fn current(&self) -> Option<&EmojiItem> {
        self.filtered.get(self.cursor)
    }

    /// `prefix` を更新して `filtered` を再計算。cursor は 0 に戻す。
    ///
    /// `shortcode` 側も `to_ascii_lowercase` してから比較する ── server から
    /// 返る `shortcode` は元のケースを保つ可能性がある (`is_valid_shortcode`
    /// が大文字混じりを許容するため)。
    pub fn set_prefix(&mut self, prefix: &str) {
        let lc = prefix.to_ascii_lowercase();
        self.filtered = self
            .all
            .iter()
            .filter(|e| e.shortcode.to_ascii_lowercase().starts_with(&lc))
            .cloned()
            .collect();
        self.prefix = lc;
        self.cursor = 0;
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

    /// 候補があるかどうか。空なら popup を閉じる判定に使う。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
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

    #[test]
    fn opens_with_all_visible_when_prefix_empty() {
        let s = EmojiSuggestState::open(vec![item("happy"), item("sad")], "");
        assert_eq!(s.filtered.len(), 2);
        assert_eq!(s.current().unwrap().shortcode, "happy");
    }

    #[test]
    fn prefix_filters_case_insensitive() {
        // server から大文字混じりで返ってきても、prefix は ASCII-lowercase で
        // 比較するので拾える。
        let mut s = EmojiSuggestState::open(vec![item("Happy"), item("sad"), item("hand")], "");
        s.set_prefix("Ha");
        let codes: Vec<&str> = s.filtered.iter().map(|e| e.shortcode.as_str()).collect();
        assert_eq!(codes, vec!["Happy", "hand"]);
    }

    #[test]
    fn cursor_wraps() {
        let mut s = EmojiSuggestState::open(vec![item("a"), item("b"), item("c")], "");
        s.select_next();
        s.select_next();
        s.select_next();
        assert_eq!(s.cursor, 0);
        s.select_prev();
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn empty_filter_is_reflected() {
        let mut s = EmojiSuggestState::open(vec![item("happy")], "");
        s.set_prefix("zzz");
        assert!(s.is_empty());
        assert!(s.current().is_none());
    }
}
