//! AP `Note.content` (HTML) → 端末向けプレーンテキスト変換。
//!
//! Mastodon は AP `Note.content` を `<p>...</p>` / `<br>` / `<a href="...">`
//! などの HTML として配信する (Misskey 由来でも同様)。DB には連合互換のため
//! HTML を素のまま保存し ── permalink / `actor outbox` などはこの形が必要 ──
//! TUI レンダリング時のみここで剥がす。
//!
//! 設計判断:
//! - `html5ever` / `ammonia` のような HTML パーサ crate は引かない。Mastodon /
//!   Misskey が AP `Note.content` に出すタグは実用上限定的 (`<p>`, `<br>`,
//!   `<a>`, `<span>`, インライン強調系) で、文字単位の小さい手書きパーサで
//!   足りる。distroless 静的バイナリのサイズ膨張も避けたい。
//! - 不明なタグは「中身だけ残す」 = 過剰除去より過小除去寄り。連合先が独自
//!   タグを足してきた場合に本文が消えるより、タグの空白が残るほうが UX 上
//!   マシ。
//! - リンク (`<a href="X">label</a>`) は **label のみ**残す。URL を別行に
//!   出すかどうかは Issue #133 (4) 添付プレビュー側で再検討する。
//!
//! Plain text 経路 (= local 投稿、Misskey 由来の plaintext) は `<` も `&` も
//! 含まないことが多いので fast-path で `clone()` 同等まで落とす。

/// AP `Note.content` 形式の文字列を、TUI の `note_lines` などが
/// `.lines()` で扱える純テキストに変換する。
///
/// - `<p>...</p>` → 段落区切り (= 空行を間に挟む)
/// - `<br>` / `<br/>` / `<br />` → 改行 1 つ
/// - `<a href="...">label</a>` → `label` のみ
/// - `<span>` / `<em>` / `<strong>` / `<i>` / `<b>` / `<code>` / `<pre>` →
///   タグを剥がして中身を残す
/// - その他の未知タグも同様 (= 中身だけ残す)
/// - 数値文字参照 (`&#NN;` / `&#xHH;`) と主要な named entity
///   (`&amp;` / `&lt;` / `&gt;` / `&quot;` / `&apos;` / `&nbsp;`) を decode
///
/// fast-path: `<` も `&` も含まない入力はそのままコピーを返す (= local の
/// plaintext 投稿)。
#[must_use]
pub fn to_plain_text(input: &str) -> String {
    if !input.contains('<') && !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => handle_tag(&mut chars, &mut out),
            '&' => handle_entity(&mut chars, &mut out),
            _ => out.push(c),
        }
    }
    trim_trailing_blank(&mut out);
    out
}

/// `<` を 1 つ消費した直後から呼ばれ、対応する `>` (or EOF) までを消費する。
fn handle_tag(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
    let closing = matches!(chars.peek(), Some('/'));
    if closing {
        chars.next();
    }
    let mut name = String::new();
    while let Some(&n) = chars.peek() {
        if n.is_ascii_alphanumeric() {
            name.push(n.to_ascii_lowercase());
            chars.next();
        } else {
            break;
        }
    }
    // タグ属性以降を `>` まで読み飛ばす (URL 等を引数に取るタグもあるので
    // 引用符内の `>` には引っかからないよう簡易だがエスケープ追跡する)。
    let mut in_quote: Option<char> = None;
    for n in chars.by_ref() {
        match (in_quote, n) {
            (Some(q), c) if c == q => in_quote = None,
            (None, '"') => in_quote = Some('"'),
            (None, '\'') => in_quote = Some('\''),
            (None, '>') => break,
            _ => {}
        }
    }

    match name.as_str() {
        "p" => {
            if closing {
                ensure_paragraph_break(out);
            } else if !out.is_empty() {
                // `<p>` 開始時点で前段に内容があれば段落区切りを入れる。
                // `</p><p>` の連続でも空行は 1 つに正規化される。
                ensure_paragraph_break(out);
            }
        }
        "br" => out.push('\n'),
        // ブロック要素は前後で改行 (= リスト / 引用ブロック / 見出し)。
        // 直前が既に改行終わりなら何もしない (= 連続ブロックで空行が増えない)。
        "blockquote" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre"
            if !out.is_empty() && !out.ends_with('\n') =>
        {
            out.push('\n');
        }
        // インライン要素 / 未知タグはタグだけ落として中身は残す。
        _ => {}
    }
}

/// `&` を 1 つ消費した直後から呼ばれ、`;` までを entity として解釈する。
/// 失敗時は `&...;` のリテラルを書き戻す (= 入力保護)。
fn handle_entity(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
    let mut body = String::new();
    let mut terminated = false;
    while let Some(&n) = chars.peek() {
        if n == ';' {
            chars.next();
            terminated = true;
            break;
        }
        // entity は最長 10 文字程度を想定 (= `&CounterClockwiseContourIntegral;`
        // のような長 named entity は AP 文脈で出ない)。
        if body.len() >= 10 || n.is_whitespace() {
            break;
        }
        body.push(n);
        chars.next();
    }
    if terminated && let Some(decoded) = decode_entity(&body) {
        out.push(decoded);
        return;
    }
    out.push('&');
    out.push_str(&body);
    if terminated {
        out.push(';');
    }
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(rest) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        let code = u32::from_str_radix(rest, 16).ok()?;
        return char::from_u32(code);
    }
    if let Some(rest) = name.strip_prefix('#') {
        let code: u32 = rest.parse().ok()?;
        return char::from_u32(code);
    }
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    }
}

/// 段落区切りを 1 つだけ挿入する。末尾に空白があれば落とし、既に空行で
/// 終わっていれば追加しない (= `<p>X</p><p>Y</p>` で `\n\n` が 1 個分だけ)。
fn ensure_paragraph_break(out: &mut String) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    if out.ends_with("\n\n") {
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

/// 末尾の空白 / 改行 / blank line を 1 まとめに落とす。
fn trim_trailing_blank(out: &mut String) {
    while let Some(c) = out.chars().last() {
        if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            out.pop();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::to_plain_text;

    #[test]
    fn plaintext_passes_through() {
        assert_eq!(to_plain_text("hello"), "hello");
        assert_eq!(to_plain_text(""), "");
        assert_eq!(to_plain_text("こんにちは\n世界"), "こんにちは\n世界");
    }

    #[test]
    fn strips_paragraph_tags() {
        assert_eq!(to_plain_text("<p>hello</p>"), "hello");
        assert_eq!(to_plain_text("<p>hello</p><p>world</p>"), "hello\n\nworld");
    }

    #[test]
    fn br_becomes_newline() {
        assert_eq!(to_plain_text("a<br>b"), "a\nb");
        assert_eq!(to_plain_text("a<br/>b"), "a\nb");
        assert_eq!(to_plain_text("a<br />b"), "a\nb");
    }

    #[test]
    fn anchor_keeps_label_only() {
        assert_eq!(
            to_plain_text(r#"see <a href="https://example.com/x">example</a> here"#),
            "see example here"
        );
    }

    #[test]
    fn span_em_strong_dropped() {
        assert_eq!(
            to_plain_text("<span>a</span><em>b</em><strong>c</strong>"),
            "abc"
        );
    }

    #[test]
    fn html_card_mention_unwrapped() {
        // Mastodon が mention を `<span class="h-card">` で包んで送ってくる
        // 典型ケース。
        let input = concat!(
            r#"<p><span class="h-card"><a href="https://example.com/@alice" "#,
            r#"class="u-url mention">@<span>alice</span></a></span> hi</p>"#
        );
        assert_eq!(to_plain_text(input), "@alice hi");
    }

    #[test]
    fn invisible_and_ellipsis_classes_unwrapped() {
        // Mastodon が長い URL を表示用に `invisible` + `ellipsis` で切る形式。
        let input = concat!(
            r#"<a href="https://example.com/long/path"><span class="invisible">"#,
            r#"https://</span><span class="ellipsis">example.com/lo</span>"#,
            r#"<span class="invisible">ng/path</span></a>"#
        );
        // 我々はラベル文字列をそのまま使う (= 端末で host 抜きで見える)。
        assert_eq!(to_plain_text(input), "https://example.com/long/path");
    }

    #[test]
    fn named_entities_decoded() {
        assert_eq!(to_plain_text("&amp;"), "&");
        assert_eq!(to_plain_text("&lt;b&gt;"), "<b>");
        assert_eq!(to_plain_text("&quot;ok&quot;"), "\"ok\"");
        assert_eq!(to_plain_text("&apos;a&apos;"), "'a'");
        // &nbsp; は通常スペースに落とす (端末は non-breaking space を持たない)。
        assert_eq!(to_plain_text("a&nbsp;b"), "a b");
    }

    #[test]
    fn numeric_entities_decoded() {
        assert_eq!(to_plain_text("&#65;"), "A");
        assert_eq!(to_plain_text("&#x41;"), "A");
        assert_eq!(to_plain_text("&#x1F338;"), "🌸");
    }

    #[test]
    fn unknown_entity_preserved() {
        // 未知 entity は生のままにする (= データ消失しない)。
        assert_eq!(to_plain_text("&unknownent;"), "&unknownent;");
    }

    #[test]
    fn bare_ampersand_preserved() {
        // `&` 単独はそのまま (= entity 解釈に失敗しても何も消えない)。
        assert_eq!(to_plain_text("AT&T"), "AT&T");
    }

    #[test]
    fn unknown_tag_keeps_contents() {
        assert_eq!(to_plain_text("<custom>inner</custom>"), "inner");
    }

    #[test]
    fn paragraph_break_normalized() {
        // 連続する `</p><p>` で空行が増えすぎないこと。
        assert_eq!(to_plain_text("<p>a</p><p>b</p><p>c</p>"), "a\n\nb\n\nc");
    }

    #[test]
    fn trailing_blank_trimmed() {
        // 末尾の `</p>` で増えた改行は落とす。
        assert_eq!(to_plain_text("<p>hello</p>   \n"), "hello");
    }

    #[test]
    fn mixed_content_with_br_and_a() {
        let input = concat!(
            r#"<p>hello<br>see <a href="https://e.example">link</a></p>"#,
            r#"<p>second &amp; final</p>"#
        );
        assert_eq!(to_plain_text(input), "hello\nsee link\n\nsecond & final");
    }

    #[test]
    fn quoted_gt_inside_attribute_not_a_tag_end() {
        // `<a title="1>2">x</a>` の属性内 `>` で打ち切られないこと。
        assert_eq!(to_plain_text(r#"<a title="1>2">x</a>"#), "x");
    }

    #[test]
    fn malformed_unterminated_tag_is_dropped() {
        // 入力が壊れていてもクラッシュしない (= EOF まで読み切って終わる)。
        let got = to_plain_text("hello <broken without close");
        assert_eq!(got, "hello");
    }

    #[test]
    fn blockquote_inserts_newline() {
        // ブロック要素は前後で改行を要求する (= 開始タグ前と終了タグ後)。
        assert_eq!(
            to_plain_text("a<blockquote>quoted</blockquote>b"),
            "a\nquoted\nb"
        );
    }
}
