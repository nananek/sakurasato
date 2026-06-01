//! Unicode 絵文字 ↔ shortcode テーブル。
//!
//! TUI の絵文字検索モーダルで、カスタム絵文字 (server `/api/v1/emojis` 由来)
//! と並べて検索・選択できるようにするための static データ。`vendor/gemoji/emoji.json`
//! (MIT © 2019 GitHub, Inc.) を [`build.rs`](../build.rs) で `OUT_DIR` に展開し、
//! [`UNICODE_EMOJI`] という `&'static [UnicodeEmojiEntry]` として焼き込んでいる。
//!
//! ## AP / Misskey 互換
//!
//! TUI で Unicode emoji を選んでリアクションを送るときの AP `content` は
//! **`codepoint` フィールドの値 (= 1 emoji 分の UTF-8 シーケンス) をそのまま**
//! 流す。Misskey / Mastodon の Unicode リアクション形式と互換 ── shortcode は
//! あくまで TUI 内の入力支援であり、対外的には Unicode 1 字として扱う。
//!
//! ## なぜ DB ではなく static か
//!
//! 1. データ自体が version 管理されていて (gemoji + Unicode CLDR) 動的更新の
//!    必要が薄い。`emoji` テーブルは custom emoji 専用のままに保てる。
//! 2. picker open 時に `/api/v1/emojis` で 100 件 fetch する既存設計に対し、
//!    1800+ の Unicode を更に乗せるとレスポンスが膨らむ。client 側で持つほうが
//!    UDS 往復を増やさずに済む。
//! 3. core crate に置けば server / tui の両方から同一実装を参照でき、
//!    将来 server が Unicode emoji を別経路で扱う必要が出ても再利用できる。

/// 1 個の Unicode 絵文字エントリ。
#[derive(Debug, Clone, Copy)]
pub struct UnicodeEmojiEntry {
    /// 実 emoji の UTF-8 シーケンス。ZWJ (`U+200D`) や VS-16 (`U+FE0F`) を
    /// 含む multi-codepoint sequence もある (例: `😶‍🌫️`)。配信時はこの
    /// 文字列をそのまま AP `content` に乗せる。
    pub codepoint: &'static str,
    /// gemoji の `aliases[0]` ── primary shortcode。UI の表示および検索の
    /// 主キー (`:happy:` 形式で見せる)。
    pub shortcode: &'static str,
    /// gemoji の `aliases[1..]` + `tags` (重複除去後)。検索の追加マッチ対象。
    /// case-insensitive 比較する想定なので元のまま (小文字化しない) で保持する。
    pub aliases: &'static [&'static str],
    /// gemoji の category (例: `"Smileys & Emotion"`)。UI 上は補助情報として
    /// 候補行の末尾に表示する。
    pub category: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/unicode_emoji_data.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_populated() {
        // gemoji 現行版で 1800+ のエントリが想定される。下限は十分に緩めて
        // gemoji のリリースで多少増減しても CI が落ちないようにする。
        assert!(UNICODE_EMOJI.len() > 1000, "got {}", UNICODE_EMOJI.len());
    }

    #[test]
    fn every_entry_has_non_empty_fields() {
        for e in UNICODE_EMOJI {
            assert!(
                !e.codepoint.is_empty(),
                "empty codepoint for {:?}",
                e.shortcode
            );
            assert!(
                !e.shortcode.is_empty(),
                "empty shortcode for {:?}",
                e.codepoint
            );
        }
    }

    #[test]
    fn well_known_emoji_present() {
        // gemoji にあるはずの代表的な shortcode が引けることを確認。これに
        // 漏れがあると build.rs / vendor ファイルがおかしいことを示す。
        let has = |sc: &str| UNICODE_EMOJI.iter().any(|e| e.shortcode == sc);
        assert!(has("grinning"), "missing :grinning:");
        assert!(has("+1"), "missing :+1:");
        assert!(has("heart"), "missing :heart:");
    }
}
