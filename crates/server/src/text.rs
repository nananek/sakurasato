//! ローカル投稿の plain text → AP `Note.content` (HTML) 変換。
//!
//! ## 背景
//!
//! `ActivityPub` の `Note.content` は **HTML** で配送される (`Mastodon` /
//! `Misskey` いずれも `<p>` / `<br>` / `<a>` 程度のサニタイズ済 subset)。一方
//! ローカル投稿は TUI / `MiAuth` クライアントが打った **plain text** なので、
//! そのまま `content` に載せると `<` が受信側の HTML パーサで未閉じタグと
//! 解釈され、本文が壊れる (= 「`<` 以降が全部タグに飲まれる」)。本モジュール
//! は plain text を最小限の安全な HTML へ変換する:
//!
//! - HTML 特殊文字 `&` / `<` / `>` を実体参照へ escape (= `<` 問題の解消)
//! - 空行 (連続する 2 つ以上の改行) で段落を分け、各段落を `<p>...</p>` で囲む
//! - 段落内の単一改行は `<br>` へ (= 連合先で改行が空白に潰れないように)
//!
//! [`crate::miauth::text::html_to_plain_text`] と TUI 側 `to_plain_text` が
//! ちょうど逆変換になっており、round-trip で元の plain text に戻る。これに
//! より DB の `note.content` は local / remote とも **常に HTML** となり、
//! 下流 (permalink / `MiAuth` / TUI / 通知) が content を一様に HTML として
//! 扱える (= local だけ plain text という従来の不整合を解消する)。
//!
//! `@user@host` mention や `:shortcode:` emoji は content にそのまま残す
//! (= `tag` 配列の `Mention` / `Emoji` が描画を駆動する)。escape 対象は
//! `& < >` のみで `@` / `:` には触れないため、抽出済みの tag と整合する。

/// plain text を AP `Note.content` 用の HTML に変換する。
///
/// 詳細はモジュールドキュメント参照。出力は `<p>...</p>` を 1 つ以上連ねた
/// 形 (段落内改行は `<br>`)。入力が空白のみ (= 段落が 1 つも残らない) のとき
/// は防御的に `<p></p>` を返す ── 呼び出し側で content は事前に非空 validate
/// 済みだが、空文字列を `content` に載せない保証を本関数内でも持つ。
#[must_use]
pub(crate) fn plain_text_to_html(input: &str) -> String {
    // 改行コードを `\n` に正規化 (CRLF / CR を取りこぼさない)。
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");

    let mut html = String::with_capacity(normalized.len() + 16);
    for paragraph in split_paragraphs(&normalized) {
        html.push_str("<p>");
        let mut line_first = true;
        for line in paragraph.split('\n') {
            if !line_first {
                html.push_str("<br>");
            }
            escape_html_text(line, &mut html);
            line_first = false;
        }
        html.push_str("</p>");
    }

    if html.is_empty() {
        html.push_str("<p></p>");
    }
    html
}

/// 連続改行 (空行) で段落に分割する。前後の改行を剥がし、空白のみの段落は
/// 捨てる。`"a\n\n\nb"` / `"a\n\n\n\nb"` のような 3 連以上の改行も 1 つの
/// 段落境界として扱う。
fn split_paragraphs(s: &str) -> Vec<&str> {
    let mut paragraphs = Vec::new();
    for part in s.split("\n\n") {
        let trimmed = part.trim_matches('\n');
        if !trimmed.trim().is_empty() {
            paragraphs.push(trimmed);
        }
    }
    paragraphs
}

/// HTML テキストノード用の最小 escape。`&` / `<` / `>` を実体参照化する。
/// テキストノードでは `"` / `'` の escape は不要 (属性値ではないため) なので
/// 触らず、round-trip での余計な変化を避ける。
fn escape_html_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::miauth::text::html_to_plain_text;

    #[test]
    fn wraps_single_line_in_paragraph() {
        assert_eq!(plain_text_to_html("hello"), "<p>hello</p>");
    }

    #[test]
    fn escapes_lt_so_it_is_not_an_unclosed_tag() {
        // 本 issue の本丸: `<` が裸で連合先に流れると未閉じタグ扱いされる。
        assert_eq!(plain_text_to_html("a < b"), "<p>a &lt; b</p>");
        assert_eq!(plain_text_to_html("<3"), "<p>&lt;3</p>");
    }

    #[test]
    fn escapes_amp_and_gt() {
        assert_eq!(plain_text_to_html("a & b > c"), "<p>a &amp; b &gt; c</p>");
    }

    #[test]
    fn single_newline_becomes_br() {
        assert_eq!(plain_text_to_html("line1\nline2"), "<p>line1<br>line2</p>");
    }

    #[test]
    fn blank_line_splits_paragraphs() {
        assert_eq!(plain_text_to_html("p1\n\np2"), "<p>p1</p><p>p2</p>");
    }

    #[test]
    fn triple_newline_collapses_to_single_paragraph_break() {
        assert_eq!(plain_text_to_html("a\n\n\n\nb"), "<p>a</p><p>b</p>");
    }

    #[test]
    fn crlf_is_normalized() {
        assert_eq!(plain_text_to_html("a\r\nb"), "<p>a<br>b</p>");
    }

    #[test]
    fn whitespace_only_yields_empty_paragraph() {
        // content は呼び出し側で非空 validate 済みだが、防御的に空 content を
        // 載せない。
        assert_eq!(plain_text_to_html("   "), "<p></p>");
    }

    #[test]
    fn does_not_touch_mention_or_shortcode_punctuation() {
        // `@` / `:` は escape 対象外 ── tag 抽出 (`Mention` / `Emoji`) と整合。
        assert_eq!(
            plain_text_to_html("hi @bob@remote.test :tada:"),
            "<p>hi @bob@remote.test :tada:</p>"
        );
    }

    #[test]
    fn round_trips_through_html_to_plain_text() {
        // plain_text_to_html は html_to_plain_text の逆変換 ── 元の plain text
        // へ戻ることを担保する (= MiAuth / TUI が確実に元文へ復元できる)。
        for original in [
            "a < b\nx & y",
            "hello world",
            "段落1\n\n段落2",
            "tom & jerry <3",
            "改行\nあり\nの\nテキスト",
        ] {
            let html = plain_text_to_html(original);
            assert_eq!(
                html_to_plain_text(&html),
                original,
                "round-trip mismatch for {original:?} (html: {html:?})"
            );
        }
    }
}
