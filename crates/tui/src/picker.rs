//! ファイルピッカ (M7)。
//!
//! TUI からアバター / ヘッダ / 添付画像を選ぶための簡易ブラウザ。
//!
//! 設計方針:
//! - **依存最小**: `std::fs` で `read_dir`、`Vec<Entry>` を素朴に持つ。
//!   モダンな fuzzy finder のような機能は最小実装後に検討する。
//! - **2 pane**: 左にパス入力 + ディレクトリ一覧、右にプレビュー。プレビュー
//!   レンダリングはランタイム側 ([`crate::runtime`]) が `ImageCache` を使って
//!   行うので、本モジュールは「選択ファイルのパス」だけ握る。
//! - **キーバインド**: Up/Down (j/k) で選択、Enter で descend or select、
//!   Backspace で親に上がる、Esc で picker を閉じる、Tab で隠しファイル
//!   トグル。直接編集モード (`/` を押すと path text field に切り替え) は
//!   将来実装する (現状はパス文字列を初期値として渡せば十分)。
//!
//! セキュリティ:
//! - シンボリックリンクはたどる ── お一人様サーバなので想定リスクは低い。
//!   将来悪意ある FS layout に晒される事態 (= サーバ運用に依存) があるなら
//!   `Metadata::file_type` で reject するが、TUI ローカル運用なら不要。
//! - 巨大ディレクトリは `MAX_ENTRIES` で切る (= UI で破綻させない)。

use std::path::{Path, PathBuf};

/// 1 ディレクトリ走査で読み込む上限。これを超える分は静かに切り捨てる
/// (status バーに警告を出す呼び出し側で対応する)。
pub const MAX_ENTRIES: usize = 1000;

/// アップロード用途。`PATCH /api/v1/actor/profile` に投げる先 (avatar/header)
/// と、`POST /api/v1/notes` の `attachment_ids` に積む先 (attachment) を
/// 区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Avatar,
    Header,
    Attachment,
}

impl PickerMode {
    /// `POST /api/v1/media` の `kind` クエリ文字列。
    pub fn as_kind(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Header => "header",
            Self::Attachment => "attachment",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Header => "header",
            Self::Attachment => "attachment",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

impl Entry {
    fn parent_entry(path: &Path) -> Self {
        Self {
            name: "..".to_string(),
            path: path
                .parent()
                .map_or_else(|| path.to_path_buf(), Path::to_path_buf),
            is_dir: true,
        }
    }
}

#[derive(Debug)]
pub struct FilePicker {
    pub mode: PickerMode,
    pub cwd: PathBuf,
    /// 並び順: 親 `..`、その後ディレクトリ (a-z)、最後にファイル (a-z)。
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub show_hidden: bool,
    /// 直近の `refresh` で truncate されたか。UI でバッジを出す用。
    pub truncated: bool,
    /// 直近の I/O エラー文字列 (`read_dir` 失敗等)。
    pub last_error: Option<String>,
}

impl FilePicker {
    /// 開始ディレクトリ。`start` が読めない場合は home → `/` の順で fallback。
    pub fn new(mode: PickerMode, start: PathBuf) -> Self {
        let mut picker = Self {
            mode,
            cwd: PathBuf::new(),
            entries: Vec::new(),
            selected: 0,
            show_hidden: false,
            truncated: false,
            last_error: None,
        };
        // `set_cwd` は失敗時に親 → home → `/` を試す。
        picker.set_cwd(start);
        picker
    }

    /// `path` に移動。失敗時は home → `/` を順に試す。
    pub fn set_cwd(&mut self, path: PathBuf) {
        let candidates = [
            path,
            std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
            PathBuf::from("/"),
        ];
        for candidate in candidates {
            match self.try_read_dir(&candidate) {
                Ok(entries) => {
                    self.cwd = candidate;
                    self.entries = entries;
                    self.selected = 0;
                    return;
                }
                Err(err) => {
                    self.last_error = Some(format!("{err}"));
                }
            }
        }
        // 万策尽きた: cwd は空 / entries 空。UI に「読めません」と出る。
        self.cwd = PathBuf::new();
        self.entries.clear();
    }

    /// `cwd` を再読する。隠しファイルトグル後などに呼ぶ。
    pub fn refresh(&mut self) {
        let cwd = self.cwd.clone();
        if cwd.as_os_str().is_empty() {
            return;
        }
        match self.try_read_dir(&cwd) {
            Ok(entries) => {
                self.entries = entries;
                if self.selected >= self.entries.len() {
                    self.selected = self.entries.len().saturating_sub(1);
                }
            }
            Err(err) => {
                self.last_error = Some(format!("{err}"));
            }
        }
    }

    fn try_read_dir(&mut self, path: &Path) -> std::io::Result<Vec<Entry>> {
        let read = std::fs::read_dir(path)?;
        let mut entries: Vec<Entry> = Vec::with_capacity(64);
        // 先頭は親へ戻る `..` (root では追加しない)。
        if path.parent().is_some() {
            entries.push(Entry::parent_entry(path));
        }
        let mut truncated = false;
        for (i, dent_result) in read.enumerate() {
            if i >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(dent) = dent_result else { continue };
            let name = dent.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            // file_type は symlink を fail-open に扱う (= 通常 file/dir として判定)。
            let is_dir = dent.file_type().is_ok_and(|t| t.is_dir());
            entries.push(Entry {
                name,
                path: dent.path(),
                is_dir,
            });
        }
        self.truncated = truncated;
        self.last_error = None;
        // dir → file の優先度。文字列比較は ASCII 安定。
        entries[1..].sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        Ok(entries)
    }

    pub fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn page_down(&mut self, viewport: usize) {
        let step = viewport.max(1);
        self.selected = (self.selected + step).min(self.entries.len().saturating_sub(1));
    }

    pub fn page_up(&mut self, viewport: usize) {
        let step = viewport.max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    /// 現在選択中のエントリ。空ディレクトリ時は `None`。
    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// 隠しファイル表示トグル。`refresh()` を呼ばないと UI に反映されないので
    /// 続けて呼ぶこと。
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// `..` を選んだか dir を選んだら descend、ファイルなら `Selected` を返す。
    /// 呼び出し側 (runtime) は `Selected(path)` を受けてアップロード経路を蹴る。
    pub fn activate(&mut self) -> Activation {
        let Some(current) = self.current().cloned() else {
            return Activation::Noop;
        };
        if current.is_dir {
            self.set_cwd(current.path);
            Activation::Descended
        } else {
            Activation::Selected(current.path)
        }
    }

    /// Backspace ── 親へ移動。
    pub fn go_parent(&mut self) {
        let parent = self.cwd.parent().map(Path::to_path_buf);
        if let Some(p) = parent {
            self.set_cwd(p);
        }
    }
}

#[derive(Debug)]
pub enum Activation {
    Noop,
    Descended,
    Selected(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, create_dir_all};

    fn make_tree(root: &Path) {
        create_dir_all(root.join("photos/cats")).unwrap();
        File::create(root.join("photos/cats/yuki.png")).unwrap();
        File::create(root.join("photos/dog.jpg")).unwrap();
        File::create(root.join("photos/.hidden.png")).unwrap();
        File::create(root.join("readme.txt")).unwrap();
    }

    #[test]
    fn lists_directory_with_parent_first() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let picker = FilePicker::new(PickerMode::Avatar, tmp.path().join("photos"));
        assert_eq!(picker.entries[0].name, "..");
        assert!(picker.entries[0].is_dir);
        // ディレクトリ (cats) が先、ファイル (dog.jpg) が後。
        let names: Vec<&str> = picker.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["..", "cats", "dog.jpg"]);
    }

    #[test]
    fn hidden_files_toggleable() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        assert!(!picker.entries.iter().any(|e| e.name == ".hidden.png"));
        picker.toggle_hidden();
        assert!(picker.entries.iter().any(|e| e.name == ".hidden.png"));
        picker.toggle_hidden();
        assert!(!picker.entries.iter().any(|e| e.name == ".hidden.png"));
    }

    #[test]
    fn descend_and_parent_navigate() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        // entries[1] = "cats" (dir)。selected = 1 にして activate。
        picker.selected = 1;
        let act = picker.activate();
        assert!(matches!(act, Activation::Descended));
        assert!(picker.cwd.ends_with("cats"));
        // 中に "yuki.png" が見える。
        assert!(picker.entries.iter().any(|e| e.name == "yuki.png"));

        // 親へ戻る。
        picker.go_parent();
        assert!(picker.cwd.ends_with("photos"));
    }

    #[test]
    fn selecting_file_returns_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        let idx = picker
            .entries
            .iter()
            .position(|e| e.name == "dog.jpg")
            .unwrap();
        picker.selected = idx;
        match picker.activate() {
            Activation::Selected(path) => {
                assert!(path.ends_with("dog.jpg"));
            }
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn picker_mode_kind_strings() {
        assert_eq!(PickerMode::Avatar.as_kind(), "avatar");
        assert_eq!(PickerMode::Header.as_kind(), "header");
        assert_eq!(PickerMode::Attachment.as_kind(), "attachment");
    }
}
