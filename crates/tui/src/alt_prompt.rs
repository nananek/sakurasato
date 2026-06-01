//! 添付アップロード時の alt text 入力プロンプト (M13 PR6 / Issue #79)。
//!
//! 添付ファイル選択 → server に POST する直前に 1 行入力で alt text を取る。
//! 空入力 (Enter のみ) の場合は alt 無しで送る ── server 側 `upload_media`
//! は `alt` クエリ空文字を None と同じ扱いにする。
//!
//! 1 行入力 overlay の単純な state ── `buffer` だけを持ち、Picker から渡って
//! きた `path` / `kind_label` / `kind` を保持して `Action::AltPromptSubmit`
//! で `run_upload` に渡す。

use crate::picker::PickerMode;

/// アップロード予約。`path` を保持したまま alt text 入力を待つ。
#[derive(Debug, Clone)]
pub struct AltPrompt {
    pub mode: PickerMode,
    pub path: std::path::PathBuf,
    pub label: String,
    pub buffer: String,
}

impl AltPrompt {
    /// alt text の最大長。Mastodon 慣習の 1500 字より緩めだが、TUI で
    /// 1 行入力なのでこれくらいで十分。サーバ側 `upload_media` には個別の
    /// 上限が無いが、`media-proxy.max_bytes` の query string 上限で自然に
    /// 抑制される ── 念のため 1000 字で TUI 側でも止める。
    pub const MAX_CHARS: usize = 1000;

    pub fn new(mode: PickerMode, path: std::path::PathBuf, label: String) -> Self {
        Self {
            mode,
            path,
            label,
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

    pub fn alt_text(&self) -> &str {
        self.buffer.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> AltPrompt {
        AltPrompt::new(
            PickerMode::Attachment,
            std::path::PathBuf::from("/tmp/x.png"),
            "x.png".into(),
        )
    }

    #[test]
    fn insert_caps_at_max() {
        let mut p = fake();
        for _ in 0..(AltPrompt::MAX_CHARS + 10) {
            p.insert_char('x');
        }
        assert_eq!(p.buffer.chars().count(), AltPrompt::MAX_CHARS);
    }

    #[test]
    fn backspace_pops_one_char() {
        let mut p = fake();
        p.insert_char('a');
        p.insert_char('b');
        p.backspace();
        assert_eq!(p.buffer, "a");
    }

    #[test]
    fn alt_text_trims_whitespace() {
        let mut p = fake();
        p.insert_char(' ');
        p.insert_char('a');
        p.insert_char(' ');
        assert_eq!(p.alt_text(), "a");
    }
}
