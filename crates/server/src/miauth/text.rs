//! AP `Note.content` (= HTML) → Misskey 互換 plain text (MFM サブセット)
//! 変換 (= #170 / 親 #150)。
//!
//! ## 背景
//!
//! `ActivityPub` の `Note.content` は **HTML** で配送される (`Mastodon` /
//! `Misskey` いずれも `<p>` / `<br>` / `<a>` 程度のサニタイズ済 subset)。一方
//! `Misskey` の `/api/notes/timeline.note.text` フィールドは **MFM** (=
//! `Misskey` Flavored Markdown、実質 plain text + 独自記法) で渡される慣行。
//! `Misskey` クライアントは `text` 文字列を **そのままレンダリング** するた
//! め、HTML を流すと `<p>hello</p>` がエスケープせず生で表示される。
//!
//! このモジュールは入力 HTML を **最小限の plain text** に倒す:
//!
//! - `<br>`, `<br/>`, `<br />` → `\n`
//! - `</p>` → `\n\n` (= 段落区切り)
//! - `<a href="...">text</a>` → `text` (= URL は捨て、表示テキストだけ残す。
//!   Misskey は bare URL を auto-detect するので元 URL がリンクテキストに含
//!   まれていれば再リンク化される)
//! - 他のタグ → strip
//! - HTML entities (`&amp;` / `&lt;` / `&gt;` / `&quot;` / `&#39;` / `&apos;`
//!   / `&nbsp;` + numeric `&#NNNN;` / `&#xHHHH;`) → decode
//!
//! ## AGPL discipline
//!
//! AP `Note.content` の HTML 形と `Misskey` `text` の MFM 仕様は public spec
//! (= `ActivityStreams` 2.0 + `misskey-hub.net`) のみを資料に書き起こした。
//! `Misskey` 本体 (AGPL-3.0) の HTML parser コードは参照していない ── 出力は
//! 観察結果に基づく独自実装 (`[[agpl-discipline-miauth]]`)。

/// HTML を Misskey 互換 plain text に変換する。
///
/// 入力は **`ActivityPub` `Note.content` で実際に流れる subset** を想定:
/// `<p>` / `<br>` / `<a href>` / `<span>` 等。複雑な HTML (= form / table /
/// script など) は実害なくただ strip される。
///
/// 未閉じタグ・不正 HTML は best-effort で読み飛ばす。`script` / `style` /
/// `iframe` 等の悪意ある中身も「タグだけ消えて中身が plain text として残る」
/// 形になるが、本関数の出力は **client UI の display 文字列**であり HTML
/// として render されない (= XSS 経路は無い)。
pub fn html_to_plain_text(html: &str) -> String {
    // 第 1 ステップ: タグ削除 + 構造改行。
    let stripped = strip_tags(html);
    // 第 2 ステップ: HTML entity decode。
    decode_entities(&stripped)
}

/// HTML タグを strip して `<br>` / `</p>` の境界に改行を入れる。
///
/// state machine: タグ内 (= `<` を見たら exit までスキップ) と それ以外。
/// `<a>` の中身は **追跡せず**、開始タグも終了タグも単にスキップする ──
/// 結果として中身のテキストはそのまま残る (= `<a href="X">label</a>` →
/// `label`)。
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // タグ開始。tag_name + attrs を読みつつ `>` を探す。
            let mut tag = String::new();
            for tc in chars.by_ref() {
                if tc == '>' {
                    break;
                }
                tag.push(tc);
            }
            // tag_name を取り出して改行挿入を判定。
            // `tag` は `br`, `br/`, `/p`, `a href="X"`, `span class="..."` 等。
            let lower = tag.trim_start_matches('/').to_ascii_lowercase();
            let name = lower.split_whitespace().next().unwrap_or("");
            // `<br>` 系 → `\n`。`<br>` / `<br/>` / `<br />` 全部 catch する。
            if name == "br" || name.trim_end_matches('/') == "br" {
                out.push('\n');
                continue;
            }
            // `</p>` → `\n\n` (= 段落終端)。`<p>` 開始タグは無視。
            // `<p>` 自身は何も挿入しない (= 段落の先頭、前に既に改行がある想定)。
            if tag.starts_with('/') && (name == "p" || name.trim_end_matches('/') == "p") {
                out.push('\n');
                out.push('\n');
            }
        } else {
            out.push(c);
        }
    }
    // 連続する空白行を 2 行に圧縮 (`</p><p>` で `\n\n\n\n` が出るので)。
    collapse_blank_lines(&out)
}

/// 連続する 3 行以上の改行を 2 行に潰す。
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newline_run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push(c);
            }
        } else {
            newline_run = 0;
            out.push(c);
        }
    }
    // 前後の空白行も切り落とす。
    out.trim().to_string()
}

/// HTML entity を decode する。
///
/// 対応: `&amp;` / `&lt;` / `&gt;` / `&quot;` / `&apos;` / `&#39;` / `&nbsp;`
/// + 数値参照 `&#NNNN;` (10 進) / `&#xHHHH;` (16 進)。
///
/// 未知の entity は **そのまま** (= `&foo;` → `&foo;`) ── HTML 仕様に従い
/// ill-formed として残す。クライアント側で render するわけではないので
/// 表示上の問題のみ。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // ASCII fast path で 1 byte ずつ push できないので char で進める。
            // s は UTF-8 なので `s[i..]` から 1 char 読んで append。
            let rest = &s[i..];
            let ch = rest.chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        // `&` から `;` までを読む。entity body の最大長は 12 文字を上限とする
        // (= 最長の HTML5 named entity `CounterClockwiseContourIntegral` は 33
        // 文字あるが、AP `Note.content` で実際に流れるのは `amp` / `lt` / `gt`
        // / `nbsp` 等の数文字 + numeric 参照 `&#1114111;` (= 7 文字、Unicode 上限
        // U+10FFFF を 10 進表記した最長ケース) で 12 文字あれば余裕で覆える)。
        // 上限を設けることで `&...... 長い文字列に `;` を含むだけ ......;` を
        // entity 候補として走査せず O(1) で諦められる。
        let Some(end_rel) = s[i + 1..].find(';') else {
            out.push('&');
            i += 1;
            continue;
        };
        let end_abs = i + 1 + end_rel;
        if end_rel > 12 {
            // 長すぎる ── entity ではないと見なし `&` をそのまま残す。
            out.push('&');
            i += 1;
            continue;
        }
        let entity = &s[i + 1..end_abs];
        if let Some(decoded) = decode_single_entity(entity) {
            out.push_str(&decoded);
            i = end_abs + 1;
        } else {
            out.push('&');
            i += 1;
        }
    }
    out
}

/// 単一 entity body (= `&` と `;` を除いた中身) を decode する。
fn decode_single_entity(body: &str) -> Option<String> {
    match body {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" | "#39" => Some("'".to_string()),
        "nbsp" => Some(" ".to_string()),
        _ => {
            // 数値参照 `#NNNN` (10 進) / `#xHHHH` (16 進)。
            if let Some(num) = body.strip_prefix('#') {
                let cp = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    num.parse::<u32>().ok()?
                };
                let c = char::from_u32(cp)?;
                Some(c.to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_p_tag_and_keep_text() {
        assert_eq!(html_to_plain_text("<p>hello</p>"), "hello");
    }

    #[test]
    fn br_becomes_newline() {
        assert_eq!(html_to_plain_text("a<br>b<br/>c<br />d"), "a\nb\nc\nd");
    }

    #[test]
    fn two_paragraphs_separated_by_blank_line() {
        assert_eq!(
            html_to_plain_text("<p>first paragraph</p><p>second paragraph</p>"),
            "first paragraph\n\nsecond paragraph"
        );
    }

    #[test]
    fn a_tag_keeps_only_label_text() {
        assert_eq!(
            html_to_plain_text(r#"hello <a href="https://example.com">world</a>!"#),
            "hello world!"
        );
    }

    #[test]
    fn mention_a_tag_keeps_at_user() {
        assert_eq!(
            html_to_plain_text(
                r#"<p>cc <a href="https://example.com/@bob" class="mention">@bob@example.com</a></p>"#
            ),
            "cc @bob@example.com"
        );
    }

    #[test]
    fn span_h_card_wrapper_is_stripped() {
        // Mastodon 系の `<span class="h-card">` で囲む mention 形式。
        assert_eq!(
            html_to_plain_text(
                r#"<span class="h-card"><a href="https://example.com/@bob" class="u-url mention">@<span>bob</span></a></span>"#
            ),
            "@bob"
        );
    }

    #[test]
    fn html_entities_are_decoded() {
        assert_eq!(
            html_to_plain_text("Tom &amp; Jerry &lt;3 &quot;hi&quot;"),
            "Tom & Jerry <3 \"hi\""
        );
    }

    #[test]
    fn numeric_entities_are_decoded() {
        // `&#39;` (= apostrophe) and `&#x2728;` (= ✨)
        assert_eq!(html_to_plain_text("it&#39;s &#x2728;"), "it's ✨");
        // `&apos;` も同義扱い
        assert_eq!(html_to_plain_text("it&apos;s ok"), "it's ok");
    }

    #[test]
    fn empty_html_is_empty() {
        assert_eq!(html_to_plain_text(""), "");
    }

    #[test]
    fn plain_text_passes_through_with_collapse_blank_lines() {
        // 3 行以上の連続改行は 2 行に圧縮 + 前後 trim。
        assert_eq!(html_to_plain_text("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(html_to_plain_text("\nhello\n"), "hello");
    }

    #[test]
    fn unclosed_tag_is_swallowed_gracefully() {
        // `<p>` だけで close されない HTML も crash しない。
        assert_eq!(html_to_plain_text("<p>hello"), "hello");
    }

    #[test]
    fn unknown_entity_is_left_as_is() {
        // `&foo;` は HTML 仕様で undefined entity ── そのまま残る。
        assert_eq!(html_to_plain_text("&foo;"), "&foo;");
    }

    #[test]
    fn lone_ampersand_is_preserved() {
        assert_eq!(html_to_plain_text("a & b"), "a & b");
    }

    #[test]
    fn nbsp_decodes_to_space() {
        assert_eq!(html_to_plain_text("a&nbsp;b"), "a b");
    }
}
