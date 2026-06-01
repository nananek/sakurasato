//! 投稿エディタの状態。
//!
//! 単純な「カーソル付き文字列バッファ」+ visibility 選択 + CW (summary)。
//! 日本語/絵文字を含む UTF-8 を切らないよう、カーソル位置は **byte offset**
//! 単位で持ち、`String::insert`/`String::remove` で操作する。move 系のヘルパは
//! 必ず char 境界に揃える。
//!
//! 改行は `Shift+Enter` (= `Enter` 単独はソフトコミット)。複数行入力に対応する。
//!
//! `ratatui` 描画側は [`Compose::wrapped_lines`] が返す `&[String]` を行ごとに
//! `Line` として描く想定。

use std::str::FromStr;

/// 投稿エディタの可視性。`server::local_api::notes::CreateNoteRequest::visibility`
/// と揃え、M13 PR6 で `direct` (DM) を追加して 4 値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Unlisted,
    Followers,
    /// M13 PR6 / Issue #79: DM。`content` 中の `@user@host` mention で宛先を解決
    /// する (= server 側 #65 経路と同じ)。followers にも broadcast されない。
    Direct,
}

impl Visibility {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Followers => "followers",
            Self::Direct => "direct",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Followers => "followers",
            Self::Direct => "direct",
        }
    }

    /// 連続押し時のサイクル順。
    /// `public → unlisted → followers → direct → public`。
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Public => Self::Unlisted,
            Self::Unlisted => Self::Followers,
            Self::Followers => Self::Direct,
            Self::Direct => Self::Public,
        }
    }
}

impl FromStr for Visibility {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "public" => Ok(Self::Public),
            "unlisted" => Ok(Self::Unlisted),
            "followers" => Ok(Self::Followers),
            "direct" => Ok(Self::Direct),
            _ => Err("invalid visibility"),
        }
    }
}

/// 投稿に添付するメディアの最小情報。`POST /api/v1/notes` の
/// `attachment_ids` に積む id と、UI 表示用ラベル (= ファイル名/サイズ) を持つ。
#[derive(Debug, Clone)]
pub struct AttachmentRef {
    pub media_id: i64,
    /// UI バッジ用の短いラベル。ファイル名がそのまま入る想定。
    pub label: String,
}

/// 投稿エディタの状態。
#[derive(Debug, Clone)]
pub struct Compose {
    /// 本文。byte 列としての挿入/削除を扱う。
    buffer: String,
    /// カーソル位置 (byte offset)。常に `buffer` の char 境界。
    cursor: usize,
    /// CW (= `summary`) 入力モード時のバッファ。空なら CW なし。
    cw: String,
    /// `true` のとき CW 行にフォーカス、`false` のとき本文行。
    editing_cw: bool,
    /// `sensitive` フラグ。
    sensitive: bool,
    visibility: Visibility,
    /// `content` 最大文字数 (chars 単位)。server 既定 5000 と揃える。
    pub max_chars: usize,
    /// M7: 添付メディア。`submit` 時に `attachment_ids` として送る。最大件数
    /// は server 側 (`ATTACHMENT_MAX = 4`) と合わせる。
    attachments: Vec<AttachmentRef>,
    /// M13 PR6 / Issue #79: 返信先 Note の `ap_id`。`submit_note` で
    /// `CreateNoteRequest::in_reply_to_ap_id` として送る。
    in_reply_to_ap_id: Option<String>,
    /// M13 PR6: 返信先 Note のヘッダ表示用ラベル (author handle + 抜粋)。
    /// レンダリングだけが用途で wire には載らない。
    reply_parent_label: Option<String>,
}

/// 添付の最大件数 (= server 側 `ATTACHMENT_MAX`)。Mastodon と揃え。
pub const ATTACHMENT_MAX: usize = 4;

impl Default for Compose {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            cw: String::new(),
            editing_cw: false,
            sensitive: false,
            visibility: Visibility::Public,
            max_chars: 5000,
            attachments: Vec::new(),
            in_reply_to_ap_id: None,
            reply_parent_label: None,
        }
    }
}

impl Compose {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn cw(&self) -> &str {
        &self.cw
    }

    pub fn editing_cw(&self) -> bool {
        self.editing_cw
    }

    pub fn sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    pub fn toggle_cw_focus(&mut self) {
        self.editing_cw = !self.editing_cw;
    }

    pub fn toggle_sensitive(&mut self) {
        self.sensitive = !self.sensitive;
    }

    pub fn cycle_visibility(&mut self) {
        self.visibility = self.visibility.cycle();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.cw.clear();
        self.editing_cw = false;
        self.sensitive = false;
        self.attachments.clear();
        self.in_reply_to_ap_id = None;
        self.reply_parent_label = None;
    }

    /// M13 PR6: 返信モードに切り替える。`label` は画面上部の親 note 表示用。
    /// `clear` が呼ばれるまで保持され、`submit_note` で wire に乗る。
    pub fn set_reply_target(&mut self, ap_id: String, label: String) {
        self.in_reply_to_ap_id = Some(ap_id);
        self.reply_parent_label = Some(label);
    }

    pub fn in_reply_to_ap_id(&self) -> Option<&str> {
        self.in_reply_to_ap_id.as_deref()
    }

    pub fn reply_parent_label(&self) -> Option<&str> {
        self.reply_parent_label.as_deref()
    }

    pub fn attachments(&self) -> &[AttachmentRef] {
        &self.attachments
    }

    pub fn attachment_ids(&self) -> Vec<i64> {
        self.attachments.iter().map(|a| a.media_id).collect()
    }

    pub fn attachments_full(&self) -> bool {
        self.attachments.len() >= ATTACHMENT_MAX
    }

    /// 添付を 1 件追加。上限を超える場合は `false` を返す (= 呼び出し側で
    /// status バーに警告を出す)。
    pub fn add_attachment(&mut self, attachment: AttachmentRef) -> bool {
        if self.attachments_full() {
            return false;
        }
        self.attachments.push(attachment);
        true
    }

    /// 末尾の添付を 1 件外す。空なら `None`。
    pub fn pop_attachment(&mut self) -> Option<AttachmentRef> {
        self.attachments.pop()
    }

    /// 1 文字挿入。`max_chars` を超える挿入は無視する (= UI 側でハイライト
    /// 表示することを想定)。
    pub fn insert_char(&mut self, ch: char) {
        if self.editing_cw {
            // CW は短くしたいので 200 文字上限 (server の SUMMARY_MAX と一致)。
            if self.cw.chars().count() >= 200 {
                return;
            }
            self.cw.push(ch);
            return;
        }
        if self.buffer.chars().count() >= self.max_chars {
            return;
        }
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// `\n` を挿入。`Shift+Enter` から呼ぶ。
    pub fn insert_newline(&mut self) {
        if self.editing_cw {
            return;
        }
        self.insert_char('\n');
    }

    /// Backspace。カーソル直前の char を 1 つ消す。
    pub fn backspace(&mut self) {
        if self.editing_cw {
            let _ = self.cw.pop();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        // 直前の char 境界を探す。
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Delete。カーソル位置の char を 1 つ消す。
    pub fn delete_forward(&mut self) {
        if self.editing_cw {
            return;
        }
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or_else(|| self.buffer.len(), |(i, _)| self.cursor + i);
        self.buffer.replace_range(self.cursor..next, "");
    }

    /// カーソルを 1 char 左へ。
    pub fn move_left(&mut self) {
        if self.editing_cw || self.cursor == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
        self.cursor = prev;
    }

    /// カーソルを 1 char 右へ。
    pub fn move_right(&mut self) {
        if self.editing_cw || self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or_else(|| self.buffer.len(), |(i, _)| self.cursor + i);
        self.cursor = next;
    }

    /// カーソルを行頭 / 行末へ。
    pub fn move_line_start(&mut self) {
        if self.editing_cw {
            return;
        }
        // 直近の `\n` を探す。
        if let Some(idx) = self.buffer[..self.cursor].rfind('\n') {
            self.cursor = idx + 1;
        } else {
            self.cursor = 0;
        }
    }

    pub fn move_line_end(&mut self) {
        if self.editing_cw {
            return;
        }
        if let Some(rel) = self.buffer[self.cursor..].find('\n') {
            self.cursor += rel;
        } else {
            self.cursor = self.buffer.len();
        }
    }

    /// 文字数。`max_chars` 比較などで使う。
    pub fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// 残文字数。負にならない。
    pub fn remaining(&self) -> i64 {
        i64::try_from(self.max_chars).unwrap_or(i64::MAX)
            - i64::try_from(self.char_count()).unwrap_or(i64::MAX)
    }

    /// 投稿可能か (= 1 文字以上で `max_chars` 以下)。
    pub fn is_submittable(&self) -> bool {
        let trimmed = self.buffer.trim();
        !trimmed.is_empty() && self.char_count() <= self.max_chars
    }

    /// 描画用に行リストを返す (LF で分割)。空文字列でも `[""]` を 1 行返す。
    pub fn lines(&self) -> Vec<&str> {
        if self.buffer.is_empty() {
            return vec![""];
        }
        self.buffer.split('\n').collect()
    }

    /// カーソルが何行目の何列 (char 数) にあるか。
    /// 返り値は `(row, col_chars)`。
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let head = &self.buffer[..self.cursor];
        let row = head.bytes().filter(|&b| b == b'\n').count();
        let col = match head.rfind('\n') {
            Some(i) => head[i + 1..].chars().count(),
            None => head.chars().count(),
        };
        (row, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_cursor_track_bytes() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_char('あ');
        c.insert_char('b');
        assert_eq!(c.buffer(), "aあb");
        // a(1) + あ(3) + b(1) = 5
        assert_eq!(c.cursor(), 5);
    }

    #[test]
    fn backspace_respects_char_boundary() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_char('あ');
        c.backspace();
        assert_eq!(c.buffer(), "a");
        assert_eq!(c.cursor(), 1);
    }

    #[test]
    fn move_left_right_walk_chars() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_char('あ');
        c.insert_char('b');
        c.move_left();
        c.move_left();
        c.move_left();
        c.move_left();
        assert_eq!(c.cursor(), 0);
        c.move_right();
        assert_eq!(c.cursor(), 1);
        c.move_right();
        assert_eq!(c.cursor(), 4); // past あ
    }

    #[test]
    fn delete_forward_drops_next_char() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_char('あ');
        c.insert_char('b');
        c.move_left();
        c.move_left();
        c.delete_forward();
        assert_eq!(c.buffer(), "ab");
    }

    #[test]
    fn newline_split_and_row_col() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_newline();
        c.insert_char('b');
        c.insert_char('c');
        assert_eq!(c.buffer(), "a\nbc");
        assert_eq!(c.cursor_row_col(), (1, 2));
        let ls = c.lines();
        assert_eq!(ls, vec!["a", "bc"]);
    }

    #[test]
    fn cw_buffer_isolated() {
        let mut c = Compose::new();
        c.toggle_cw_focus();
        c.insert_char('w');
        c.insert_char('a');
        assert_eq!(c.cw(), "wa");
        assert_eq!(c.buffer(), "");
        c.backspace();
        assert_eq!(c.cw(), "w");
    }

    #[test]
    fn submittable_requires_non_empty() {
        let mut c = Compose::new();
        assert!(!c.is_submittable());
        c.insert_char(' ');
        assert!(!c.is_submittable());
        c.insert_char('x');
        assert!(c.is_submittable());
    }

    #[test]
    fn cap_at_max_chars() {
        let mut c = Compose::new();
        c.max_chars = 3;
        for ch in ['a', 'b', 'c'] {
            c.insert_char(ch);
        }
        c.insert_char('d');
        assert_eq!(c.buffer(), "abc");
    }

    #[test]
    fn visibility_cycles_through_four() {
        let mut c = Compose::new();
        assert_eq!(c.visibility(), Visibility::Public);
        c.cycle_visibility();
        assert_eq!(c.visibility(), Visibility::Unlisted);
        c.cycle_visibility();
        assert_eq!(c.visibility(), Visibility::Followers);
        c.cycle_visibility();
        assert_eq!(c.visibility(), Visibility::Direct);
        c.cycle_visibility();
        assert_eq!(c.visibility(), Visibility::Public);
    }

    #[test]
    fn direct_round_trips_via_from_str() {
        assert_eq!(Visibility::from_str("direct").unwrap(), Visibility::Direct);
        assert_eq!(Visibility::Direct.as_wire(), "direct");
    }

    #[test]
    fn reply_target_is_carried_until_clear() {
        let mut c = Compose::new();
        assert!(c.in_reply_to_ap_id().is_none());
        c.set_reply_target("https://x/notes/1".into(), "@a@b: hello".into());
        assert_eq!(c.in_reply_to_ap_id(), Some("https://x/notes/1"));
        assert_eq!(c.reply_parent_label(), Some("@a@b: hello"));
        c.clear();
        assert!(c.in_reply_to_ap_id().is_none());
        assert!(c.reply_parent_label().is_none());
    }

    #[test]
    fn move_line_start_end_within_multiline() {
        let mut c = Compose::new();
        c.insert_char('a');
        c.insert_char('b');
        c.insert_newline();
        c.insert_char('c');
        c.insert_char('d');
        // cursor at end ("ab\ncd"[5])
        c.move_line_start();
        assert_eq!(c.cursor(), 3); // start of "cd"
        c.move_line_end();
        assert_eq!(c.cursor(), 5);
    }

    #[test]
    fn attachments_respect_max_and_clear_resets_them() {
        let mut c = Compose::new();
        for i in 0..ATTACHMENT_MAX {
            let added = c.add_attachment(AttachmentRef {
                media_id: i64::try_from(i).unwrap(),
                label: format!("x{i}"),
            });
            assert!(added, "should accept {i}-th attachment");
        }
        // 5 件目は上限超過で拒否。
        assert!(c.attachments_full());
        let denied = c.add_attachment(AttachmentRef {
            media_id: 99,
            label: "x".into(),
        });
        assert!(!denied);
        // clear で attachments も空に。
        assert_eq!(c.attachments().len(), ATTACHMENT_MAX);
        c.clear();
        assert!(c.attachments().is_empty());
        assert!(!c.attachments_full());
    }

    #[test]
    fn pop_attachment_returns_last() {
        let mut c = Compose::new();
        c.add_attachment(AttachmentRef {
            media_id: 1,
            label: "a".into(),
        });
        c.add_attachment(AttachmentRef {
            media_id: 2,
            label: "b".into(),
        });
        let popped = c.pop_attachment().unwrap();
        assert_eq!(popped.media_id, 2);
        assert_eq!(c.attachment_ids(), vec![1]);
    }
}
