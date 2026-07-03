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
use std::time::SystemTime;

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
    /// `POST /api/v1/media` の `kind` クエリ文字列。サーバ受理は
    /// `avatar|header|attachment` の 3 値固定 ([`crate::client::upload_media`])。
    pub fn as_kind(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Header => "header",
            Self::Attachment => "attachment",
        }
    }

    /// UI 表示用ラベル。今は `as_kind` と同値だが、将来 `添付` のように
    /// ローカライズしたいときに分岐させる用に別関数で持つ。
    pub fn label(self) -> &'static str {
        self.as_kind()
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// ファイルサイズ (bytes)。`try_read_dir` で `DirEntry::metadata` から
    /// 取得する。stat 失敗 / ディレクトリは `None`。render 時に同期 stat を
    /// 走らせると tokio メインスレッドをブロックするので、ここで事前に
    /// キャッシュしておく (PR #43 round-2 review Medium 対応)。
    pub size: Option<u64>,
    /// 作成日時 (Issue #287)。`Metadata::created` を優先し、取れない環境
    /// (= created が ENOTSUP の FS/カーネル) では `modified` (mtime) に
    /// フォールバックする。stat 失敗時は `None`。同種エントリ内の並び順
    /// (作成日降順) のソートキーに使う。`..` は常に `None` で先頭固定。
    pub created: Option<SystemTime>,
}

impl Entry {
    fn parent_entry(path: &Path) -> Self {
        Self {
            name: "..".to_string(),
            path: path
                .parent()
                .map_or_else(|| path.to_path_buf(), Path::to_path_buf),
            is_dir: true,
            size: None,
            created: None,
        }
    }
}

/// エントリの並び順比較 (Issue #287)。`..` を除いた範囲に適用する:
/// 1. ディレクトリ → ファイルの優先度。
/// 2. 同種内は **作成日降順** (新しいものが上)。created 不明 (`None`) は末尾へ。
/// 3. 時刻が同値なら名前昇順 (ASCII 安定)。
///
/// `Option<SystemTime>` の `Ord` は `None < Some` なので、降順にするため
/// `b` と `a` を入れ替えて比較する (= `Some` が先、`None` が後)。
fn cmp_entries(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => b.created.cmp(&a.created).then_with(|| a.name.cmp(&b.name)),
    }
}

#[derive(Debug)]
pub struct FilePicker {
    pub mode: PickerMode,
    pub cwd: PathBuf,
    /// 並び順: 親 `..` を先頭に、その後ディレクトリ → ファイルの順。
    /// 同種内は作成日降順 (新しいものが上、Issue #287)、時刻同値は名前昇順。
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
            // metadata は render 前に一度読んでキャッシュ。`DirEntry::metadata`
            // は OS によっては readdir で得た値をそのまま使う最適化があるので、
            // `fs::metadata` を別途呼ぶより安い。render 時に同期 stat を走らせ
            // ないためここで size / created を確定させる。
            let meta = dent.metadata().ok();
            // ファイルサイズ。失敗時は None で UI 側 "(unknown size)"。
            // ディレクトリは概念的に size 無し。
            let size = if is_dir {
                None
            } else {
                meta.as_ref().map(std::fs::Metadata::len)
            };
            // 作成日時 (Issue #287)。created 優先、無ければ mtime にフォールバック。
            let created = meta
                .as_ref()
                .and_then(|m| m.created().or_else(|_| m.modified()).ok());
            entries.push(Entry {
                name,
                path: dent.path(),
                is_dir,
                size,
                created,
            });
        }
        self.truncated = truncated;
        self.last_error = None;
        // 先頭の `..` を固定したまま残りを並べ替える (Issue #287)。
        entries[1..].sort_by(cmp_entries);
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

    #[test]
    fn file_size_cached_in_entry() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        // dog.jpg は 0 バイト (touch されたのみ)。
        let picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        let dog = picker
            .entries
            .iter()
            .find(|e| e.name == "dog.jpg")
            .expect("dog.jpg should be listed");
        assert!(!dog.is_dir);
        assert_eq!(dog.size, Some(0), "size should be stat'd at read_dir time");
        // ディレクトリ (..  / cats) は size: None。
        let parent = &picker.entries[0];
        assert_eq!(parent.name, "..");
        assert_eq!(parent.size, None);
        let cats = picker.entries.iter().find(|e| e.name == "cats").unwrap();
        assert!(cats.is_dir);
        assert_eq!(cats.size, None);
    }

    /// テスト用 [`Entry`] を `name` / `is_dir` / `created` から手早く作る。
    fn entry(name: &str, is_dir: bool, created: Option<SystemTime>) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            size: None,
            created,
        }
    }

    #[test]
    fn sort_dirs_before_files_then_created_desc() {
        // Issue #287: ディレクトリ優先 → 同種内は作成日降順。時刻同値は名前昇順、
        // created 不明 (None) は末尾。FS のタイムスタンプ解像度に依存しないよう
        // 比較関数を直接叩く。
        let base = SystemTime::UNIX_EPOCH;
        let older = base + std::time::Duration::from_secs(100);
        let newer = base + std::time::Duration::from_secs(200);
        let mut items = [
            entry("old_file.png", false, Some(older)),
            entry("new_dir", true, Some(newer)),
            entry("new_file.png", false, Some(newer)),
            entry("undated_file.png", false, None),
            entry("old_dir", true, Some(older)),
        ];
        items.sort_by(cmp_entries);
        let names: Vec<&str> = items.iter().map(|e| e.name.as_str()).collect();
        // dir が先 (new_dir → old_dir)、その後 file を作成日降順
        // (new_file → old_file → 日時不明 undated_file)。
        assert_eq!(
            names,
            vec![
                "new_dir",
                "old_dir",
                "new_file.png",
                "old_file.png",
                "undated_file.png",
            ]
        );
    }

    #[test]
    fn created_is_cached_in_entry() {
        // Issue #287: read_dir 時点で created (or mtime fallback) を stat 済み。
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        let dog = picker
            .entries
            .iter()
            .find(|e| e.name == "dog.jpg")
            .expect("dog.jpg should be listed");
        assert!(
            dog.created.is_some(),
            "created (or mtime fallback) should be stat'd at read_dir time"
        );
        // `..` は常に None 固定。
        assert_eq!(picker.entries[0].name, "..");
        assert_eq!(picker.entries[0].created, None);
    }
}
