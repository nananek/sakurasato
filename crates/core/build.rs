//! `vendor/gemoji/emoji.json` (MIT © 2019 GitHub, Inc.) を Rust の const
//! slice に変換して `$OUT_DIR/unicode_emoji_data.rs` として書き出す。
//!
//! `src/unicode_emoji.rs` がこのファイルを `include!` する。生成された
//! `UNICODE_EMOJI` は [`crate::unicode_emoji::UnicodeEmojiEntry`] の
//! `&'static [UnicodeEmojiEntry]`。
//!
//! gemoji のフィールドのマッピング:
//!   - `emoji` → `codepoint` (multi-codepoint ZWJ シーケンス含む)
//!   - `aliases[0]` → `shortcode` (primary; aliases が空のエントリは
//!     picker で表示できないので無視)
//!   - `aliases[1..]` + `tags` (dedup) → `aliases`
//!   - `category` → `category`

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(serde::Deserialize)]
struct GemojiEntry {
    emoji: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn main() {
    let path = "../../vendor/gemoji/emoji.json";
    println!("cargo::rerun-if-changed={path}");
    println!("cargo::rerun-if-changed=build.rs");

    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let entries: Vec<GemojiEntry> =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"));

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let dest = Path::new(&out_dir).join("unicode_emoji_data.rs");
    let mut f =
        fs::File::create(&dest).unwrap_or_else(|e| panic!("create {}: {e}", dest.display()));

    writeln!(
        f,
        "// Auto-generated from vendor/gemoji/emoji.json by build.rs."
    )
    .unwrap();
    writeln!(
        f,
        "// Do not edit; regenerate by rebuilding `sakurasato-core`."
    )
    .unwrap();
    writeln!(f, "pub static UNICODE_EMOJI: &[UnicodeEmojiEntry] = &[").unwrap();
    for e in &entries {
        // aliases が空の gemoji エントリは shortcode を持たないため picker
        // で索引できない。zero-width joiner だけのフラグ等が該当する想定。
        let Some(shortcode) = e.aliases.first() else {
            continue;
        };
        let mut extra: Vec<String> = e.aliases.iter().skip(1).cloned().collect();
        for tag in &e.tags {
            if !extra.iter().any(|x| x == tag) {
                extra.push(tag.clone());
            }
        }
        writeln!(f, "    UnicodeEmojiEntry {{").unwrap();
        writeln!(f, "        codepoint: \"{}\",", escape(&e.emoji)).unwrap();
        writeln!(f, "        shortcode: \"{}\",", escape(shortcode)).unwrap();
        write!(f, "        aliases: &[").unwrap();
        for a in &extra {
            write!(f, "\"{}\", ", escape(a)).unwrap();
        }
        writeln!(f, "],").unwrap();
        writeln!(f, "        category: \"{}\",", escape(&e.category)).unwrap();
        writeln!(f, "    }},").unwrap();
    }
    writeln!(f, "];").unwrap();
}

/// Rust 文字列リテラルに乗せるための最小エスケープ。
///
/// 現行 gemoji データに `\` / `"` / 制御文字は含まれていないが、将来の
/// vendor 更新で混入しても生成ファイルが壊れないよう、`\\` / `"` に加えて
/// `\n` / `\r` / `\t` も明示的に処理しておく ([review #122] minor 1 対応)。
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
