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
//! - **解決済み mention / 抽出済み hashtag は `<a>` でマークアップ**する
//!   (Mastodon / Misskey 互換。`plain_text_to_html_with_links`)
//!
//! [`crate::miauth::text::html_to_plain_text`] と TUI 側 `to_plain_text` が
//! ちょうど逆変換になっており、round-trip で元の plain text に戻る。これに
//! より DB の `note.content` は local / remote とも **常に HTML** となり、
//! 下流 (permalink / `MiAuth` / TUI / 通知) が content を一様に HTML として
//! 扱える (= local だけ plain text という従来の不整合を解消する)。
//!
//! `:shortcode:` emoji は content にそのまま残す (= `tag` 配列の `Emoji` が
//! 描画を駆動する)。mention / hashtag の `<a>` は [`crate::local_api::notes`]
//! の `scan_mentions` / `scan_hashtags` と同一の抽出ロジックで span を取り、
//! 解決マップに載っているものだけをリンク化する ── `tag` 配列 (`Mention` /
//! `Hashtag`) と描画 span がずれない。

use std::collections::HashMap;

/// plain text を AP `Note.content` 用の HTML に変換する。リンク化は行わない
/// (= 空マップで委譲)。既存の呼び出し元 / テスト向けの後方互換 API。
///
/// 詳細はモジュールドキュメント参照。出力は `<p>...</p>` を 1 つ以上連ねた
/// 形 (段落内改行は `<br>`)。入力が空白のみ (= 段落が 1 つも残らない) のとき
/// は防御的に `<p></p>` を返す ── 呼び出し側で content は事前に非空 validate
/// 済みだが、空文字列を `content` に載せない保証を本関数内でも持つ。
#[must_use]
#[allow(dead_code, reason = "後方互換の委譲 API (現状テスト専用)")]
pub(crate) fn plain_text_to_html(input: &str) -> String {
    plain_text_to_html_with_links(input, &HashMap::new(), &HashMap::new())
}

/// plain text を AP `Note.content` 用の HTML に変換し、**解決済み mention と
/// 抽出済み hashtag を `<a>` でマークアップ**する。
///
/// - `mentions`: key = lowercase `"user@host"`、value = actor URI。
///   `<a href="{uri}" class="mention" rel="nofollow">@user@host</a>` を emit。
/// - `hashtags`: key = lowercase tag name (`#` 抜き)、value = 絶対 href
///   (`{host}/tags/{name}`)。`<a href="{href}" class="hashtag" rel="nofollow">#tag</a>`
///   を emit。
///
/// マップに無い mention / hashtag は素のテキスト (escape) のまま ── 自己
/// mention (解決時に drop される) や未解決タグはリンクしない。span は
/// [`crate::local_api::notes::scan_mentions`] / [`crate::local_api::notes::scan_hashtags`]
/// の byte span を使うので、`tag` 配列の抽出 (`parse_mentions` /
/// `parse_hashtags`) と完全に一致する (= 描画と tag がずれない)。
#[must_use]
pub(crate) fn plain_text_to_html_with_links(
    input: &str,
    mentions: &HashMap<String, String>,
    hashtags: &HashMap<String, String>,
) -> String {
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
            render_line_with_links(line, mentions, hashtags, &mut html);
            line_first = false;
        }
        html.push_str("</p>");
    }

    if html.is_empty() {
        html.push_str("<p></p>");
    }
    html
}

/// 1 行の plain text に mention / hashtag の `<a>` を差し込みながら escape する。
///
/// `scan_mentions` / `scan_hashtags` が返す byte span のうち、解決マップに
/// 存在するものだけを `LinkSpan` として集め、start 順に並べてテキストを
/// 組み立てる。span は重複しない (= mention と hashtag は境界規則が排他) が、
/// 防御的に前回 end より前の span はスキップする。
fn render_line_with_links(
    line: &str,
    mentions: &HashMap<String, String>,
    hashtags: &HashMap<String, String>,
    out: &mut String,
) {
    let mut spans: Vec<LinkSpan> = Vec::new();
    for s in crate::local_api::notes::scan_mentions(line) {
        let key = format!("{}@{}", s.user, s.host).to_ascii_lowercase();
        if let Some(href) = mentions.get(&key) {
            spans.push(LinkSpan {
                start: s.start,
                end: s.end,
                href: href.clone(),
                class: "mention",
            });
        }
    }
    for s in crate::local_api::notes::scan_hashtags(line) {
        let key = s.name.to_lowercase();
        if let Some(href) = hashtags.get(&key) {
            spans.push(LinkSpan {
                start: s.start,
                end: s.end,
                href: href.clone(),
                class: "hashtag",
            });
        }
    }
    spans.sort_by_key(|s| s.start);

    let mut pos = 0;
    for span in spans {
        if span.start < pos {
            // span の重なりは起きない想定 (= 境界規則が排他) だが、万一に備えて
            // 2 個目以降を捨てる (= 壊れた HTML を出さない)。
            continue;
        }
        escape_html_text(&line[pos..span.start], out);
        // `<a href="{href}" class="{mention|hashtag}" rel="nofollow">`
        out.push_str("<a href=\"");
        escape_html_attr(&span.href, out);
        out.push_str("\" class=\"");
        out.push_str(span.class);
        out.push_str("\" rel=\"nofollow\">");
        escape_html_text(&line[span.start..span.end], out);
        out.push_str("</a>");
        pos = span.end;
    }
    escape_html_text(&line[pos..], out);
}

/// リンク化対象の mention / hashtag span 1 件分。`href` は解決済みの絶対 URL。
struct LinkSpan {
    start: usize,
    end: usize,
    href: String,
    class: &'static str,
}

/// 属性値用の最小 escape。`&` と `"` を実体参照化する ── `href` に `&` を含む
/// URL (actor URI のクエリ等) が来ても属性値が壊れないように。テキストノード
/// 用の [`escape_html_text`] とは対象が違うので別関数。
fn escape_html_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
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

    // ── plain_text_to_html_with_links (MFM mention/hashtag markup) ──────

    fn mentions() -> HashMap<String, String> {
        HashMap::from([
            (
                "bob@remote.test".to_string(),
                "https://remote.test/users/bob".to_string(),
            ),
            (
                "carol@other.test".to_string(),
                "https://other.test/users/carol".to_string(),
            ),
        ])
    }

    fn hashtags() -> HashMap<String, String> {
        HashMap::from([
            (
                "sakura".to_string(),
                "https://sakurasato.test/tags/sakura".to_string(),
            ),
            (
                "桜".to_string(),
                "https://sakurasato.test/tags/桜".to_string(),
            ),
        ])
    }

    #[test]
    fn links_resolved_mention() {
        let html =
            plain_text_to_html_with_links("hi @bob@remote.test!", &mentions(), &HashMap::new());
        assert_eq!(
            html,
            r#"<p>hi <a href="https://remote.test/users/bob" class="mention" rel="nofollow">@bob@remote.test</a>!</p>"#
        );
    }

    #[test]
    fn leaves_unresolved_mention_as_plain_text() {
        // 解決マップに無い mention はリンクしない (= 素のテキスト)。
        let html =
            plain_text_to_html_with_links("hi @ghost@remote.test", &mentions(), &HashMap::new());
        assert_eq!(html, "<p>hi @ghost@remote.test</p>");
    }

    #[test]
    fn links_resolved_hashtag() {
        let html = plain_text_to_html_with_links("spring #sakura", &HashMap::new(), &hashtags());
        assert_eq!(
            html,
            r#"<p>spring <a href="https://sakurasato.test/tags/sakura" class="hashtag" rel="nofollow">#sakura</a></p>"#
        );
    }

    #[test]
    fn hashtag_boundary_prevents_c_sharp() {
        // `C#` / 単語内 `#` はハッシュタグと認識しない (= リンクしない)。
        let html = plain_text_to_html_with_links("C# tag", &HashMap::new(), &hashtags());
        assert_eq!(html, "<p>C# tag</p>");
        let html = plain_text_to_html_with_links("abc#def", &HashMap::new(), &hashtags());
        assert_eq!(html, "<p>abc#def</p>");
    }

    #[test]
    fn preserves_original_case_in_label() {
        // span は元のケースを保持したまま `<a>` の label に載せる。href は
        // 解決マップの lowercase key で引ける (= `#SAKURA` もリンク化)。
        let html =
            plain_text_to_html_with_links("#SAKURA @BOB@REMOTE.TEST", &mentions(), &hashtags());
        assert_eq!(
            html,
            r#"<p><a href="https://sakurasato.test/tags/sakura" class="hashtag" rel="nofollow">#SAKURA</a> <a href="https://remote.test/users/bob" class="mention" rel="nofollow">@BOB@REMOTE.TEST</a></p>"#
        );
    }

    #[test]
    fn japanese_hashtag_is_linked() {
        let html = plain_text_to_html_with_links("#桜 満開", &HashMap::new(), &hashtags());
        assert_eq!(
            html,
            r#"<p><a href="https://sakurasato.test/tags/桜" class="hashtag" rel="nofollow">#桜</a> 満開</p>"#
        );
    }

    #[test]
    fn escapes_html_specials_around_links() {
        // escape (`<` / `&`) とリンクが混在しても壊れない。
        let html =
            plain_text_to_html_with_links("a < b & #sakura > c", &HashMap::new(), &hashtags());
        assert_eq!(
            html,
            r#"<p>a &lt; b &amp; <a href="https://sakurasato.test/tags/sakura" class="hashtag" rel="nofollow">#sakura</a> &gt; c</p>"#
        );
    }

    #[test]
    fn links_survive_multiline() {
        // 段落内改行 (`<br>`) / 段落分割と混在しても span がずれない。
        let html = plain_text_to_html_with_links(
            "line1 #sakura\nline2 @bob@remote.test",
            &mentions(),
            &hashtags(),
        );
        assert_eq!(
            html,
            r#"<p>line1 <a href="https://sakurasato.test/tags/sakura" class="hashtag" rel="nofollow">#sakura</a><br>line2 <a href="https://remote.test/users/bob" class="mention" rel="nofollow">@bob@remote.test</a></p>"#
        );
    }

    #[test]
    fn round_trips_with_links_through_html_to_plain_text() {
        // `<a>` 付きでも html_to_plain_text が label だけ残すため、元文に戻る。
        for original in [
            "hi @bob@remote.test #sakura",
            "spring #桜 and @bob@remote.test",
            "#sakura\nwith @carol@other.test\n#桜",
            "C# is not a hashtag but #sakura is",
        ] {
            let html = plain_text_to_html_with_links(original, &mentions(), &hashtags());
            assert_eq!(
                html_to_plain_text(&html),
                original,
                "round-trip mismatch for {original:?} (html: {html:?})"
            );
        }
    }
}
