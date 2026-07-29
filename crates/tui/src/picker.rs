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
//!   Backspace で親に上がる、Esc で picker を閉じる、`.` で隠しファイル
//!   トグル、`/` でパス直接入力モードに切り替え (Tab で補完、Enter で確定、
//!   Esc でキャンセルして通常ブラウズに戻る)。
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
    /// 絵文字管理画面 (Issue #328 系) からの Misskey 形式 zip インポート。
    /// `POST /api/v1/media` は叩かない (= `run_emoji_zip_import` が
    /// `POST /api/v1/emojis/import` に raw zip を送る別経路)。
    EmojiZip,
}

impl PickerMode {
    /// `POST /api/v1/media` の `kind` クエリ文字列。サーバ受理は
    /// `avatar|header|attachment` の 3 値固定 ([`crate::client::upload_media`])。
    /// `EmojiZip` はこの API を叩かないため呼ばれない想定 (exhaustive match
    /// のため値だけ埋める)。
    pub fn as_kind(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Header => "header",
            Self::Attachment => "attachment",
            Self::EmojiZip => "emoji_zip",
        }
    }

    /// UI 表示用ラベル。今は `as_kind` と同値だが、将来 `添付` のように
    /// ローカライズしたいときに分岐させる用に別関数で持つ。
    pub fn label(self) -> &'static str {
        match self {
            Self::EmojiZip => "emoji zip",
            _ => self.as_kind(),
        }
    }

    /// ファイル一覧に表示する拡張子フィルタ (小文字、`.` 無し)。`None` なら
    /// 全ファイルを表示 (Avatar/Header/Attachment の従来挙動)。`EmojiZip` は
    /// `.zip` 以外を隠し、誤選択を防ぐ。ディレクトリは常に表示するので対象外。
    pub fn extension_filter(self) -> Option<&'static str> {
        match self {
            Self::EmojiZip => Some("zip"),
            _ => None,
        }
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
    /// `/` で入るパス直接入力モード。`Some` の間は j/k 等の一覧ナビゲーション
    /// キーを止めてテキスト入力に回す ([`crate::event::translate_picker_key`]
    /// 参照)。
    pub path_input: Option<PathInput>,
}

/// [`FilePicker::path_input`] の編集 buffer。[`crate::lists::ListsInput`] /
/// [`crate::alt_prompt::AltPrompt`] と同じ「1 行バッファ + カーソルは常に
/// 末尾」の最小実装 (= 挿入位置を可変にするほどの入力長は想定しない)。
#[derive(Debug, Clone, Default)]
pub struct PathInput {
    pub buffer: String,
}

impl PathInput {
    pub fn insert_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    pub fn backspace(&mut self) {
        self.buffer.pop();
    }
}

/// [`FilePicker::submit_path_input`] の結果。
#[derive(Debug)]
pub enum PathInputOutcome {
    /// 入力が空 / picker 自体が無かった。
    Noop,
    /// ディレクトリへ descend 済み (= `set_cwd` 実行済み)。
    Descended,
    /// ファイルを指していた。呼び出し側は [`Activation::Selected`] と同じ
    /// 経路 (upload kick) に合流させる。
    Selected(PathBuf),
    /// 存在しない / 読めないパス。`String` はユーザ向けエラーメッセージ。
    /// 入力 buffer は破棄せず維持する (= 打ち直しではなく訂正できるように)。
    Invalid(String),
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
            path_input: None,
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
            // 拡張子フィルタ (Issue #328 系: `EmojiZip` モードは `.zip` 以外を
            // 隠す)。ディレクトリは常に descend 可能なので対象外。
            if !is_dir
                && let Some(ext) = self.mode.extension_filter()
                && !name.to_ascii_lowercase().ends_with(&format!(".{ext}"))
            {
                continue;
            }
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

    /// `/` ── パス直接入力モードに入る。現在の `cwd` を初期値にする (=
    /// ゼロから打ち直さず、末尾だけ書き換えれば近隣ディレクトリに移動できる)。
    pub fn open_path_input(&mut self) {
        let mut buffer = self.cwd.to_string_lossy().into_owned();
        if !buffer.ends_with('/') {
            buffer.push('/');
        }
        self.path_input = Some(PathInput { buffer });
    }

    /// Esc ── パス入力をキャンセルし、通常ブラウズに戻る (`cwd` は不変)。
    pub fn cancel_path_input(&mut self) {
        self.path_input = None;
    }

    /// Tab ── 入力中のパスを補完する。最後の `/` より後ろを prefix として、
    /// その手前のディレクトリの子から前方一致するものを探し、共通の最長
    /// 一致まで埋める (= shell の標準的な補完挙動)。候補が唯一かつ
    /// ディレクトリなら末尾に `/` を付けて続けて補完できるようにする。
    /// 候補が無ければ何もしない (= 誤入力の合図として buffer をそのまま残す)。
    pub fn complete_path_input(&mut self) {
        let Some(input) = self.path_input.as_mut() else {
            return;
        };
        let expanded = expand_tilde(&input.buffer);
        let (dir, prefix) = split_path_prefix(&expanded);
        let Ok(read) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut candidates: Vec<(String, bool)> = read
            .filter_map(Result::ok)
            .filter_map(|dent| {
                let name = dent.file_name().to_string_lossy().into_owned();
                if !prefix.is_empty() && !name.starts_with(&prefix) {
                    return None;
                }
                if prefix.is_empty() && name.starts_with('.') {
                    // 空 prefix (= "dir/" の直後で Tab) では隠しファイルを
                    // 候補から除く ── show_hidden 設定に関わらず、うっかり
                    // Tab連打で `.ssh` 等に踏み込まないための安全側デフォルト。
                    return None;
                }
                let is_dir = dent.file_type().is_ok_and(|t| t.is_dir());
                Some((name, is_dir))
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        candidates.sort();
        let common = longest_common_prefix(candidates.iter().map(|(name, _)| name.as_str()));
        if common.is_empty() {
            return;
        }
        let mut new_buffer = dir.to_string_lossy().into_owned();
        if !new_buffer.ends_with('/') {
            new_buffer.push('/');
        }
        new_buffer.push_str(&common);
        if candidates.len() == 1 && candidates[0].1 {
            new_buffer.push('/');
        }
        input.buffer = new_buffer;
    }

    /// Enter ── 入力中のパスを確定する。ディレクトリなら descend
    /// (`set_cwd` 済み)、ファイルなら [`PathInputOutcome::Selected`] を返し
    /// 呼び出し側で `Activation::Selected` と同じ経路に合流させる。存在し
    /// ない / 読めないパスは入力を維持したまま [`PathInputOutcome::Invalid`]
    /// を返す (= 打ち直しではなく訂正できるように)。
    pub fn submit_path_input(&mut self) -> PathInputOutcome {
        let Some(input) = self.path_input.take() else {
            return PathInputOutcome::Noop;
        };
        if input.buffer.trim().is_empty() {
            return PathInputOutcome::Noop;
        }
        let path = expand_tilde(&input.buffer);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                self.set_cwd(path);
                PathInputOutcome::Descended
            }
            Ok(meta) if meta.is_file() => PathInputOutcome::Selected(path),
            Ok(_) => {
                // ソケット / デバイスファイル等、file でも dir でもないもの。
                let msg = format!("not a regular file or directory: {}", path.display());
                self.path_input = Some(input);
                PathInputOutcome::Invalid(msg)
            }
            Err(err) => {
                let msg = format!("{}: {err}", path.display());
                self.path_input = Some(input);
                PathInputOutcome::Invalid(msg)
            }
        }
    }
}

/// `~` / `~/...` を `$HOME` に展開する。`HOME` 未設定ならそのまま
/// (= 大抵は存在しないパスとして `metadata()` が失敗し、呼び出し側で
/// エラー表示される)。
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix('~')
        && let Some(home) = std::env::var_os("HOME")
    {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            return PathBuf::from(home);
        }
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(input)
}

/// 入力 buffer を「補完対象の親ディレクトリ」と「マッチさせる prefix
/// (最後の `/` より後ろ)」に分ける。`/` が無ければ親を `.` (カレント) とする。
fn split_path_prefix(path: &Path) -> (PathBuf, String) {
    let s = path.to_string_lossy();
    match s.rfind('/') {
        Some(pos) => {
            let dir = if pos == 0 {
                "/".to_string()
            } else {
                s[..pos].to_string()
            };
            (PathBuf::from(dir), s[pos + 1..].to_string())
        }
        None => (PathBuf::from("."), s.into_owned()),
    }
}

/// 候補文字列群の最長共通 prefix。空集合なら空文字列。
fn longest_common_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut common: Option<String> = None;
    for name in names {
        common = Some(match common {
            None => name.to_string(),
            Some(prev) => {
                let len = prev
                    .chars()
                    .zip(name.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                prev.chars().take(len).collect()
            }
        });
    }
    common.unwrap_or_default()
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
    fn emoji_zip_mode_hides_non_zip_files() {
        let tmp = tempfile::tempdir().unwrap();
        create_dir_all(tmp.path().join("emojis")).unwrap();
        File::create(tmp.path().join("emojis/pack.zip")).unwrap();
        File::create(tmp.path().join("emojis/PACK2.ZIP")).unwrap();
        File::create(tmp.path().join("emojis/readme.txt")).unwrap();
        File::create(tmp.path().join("emojis/cats")).unwrap();
        let picker = FilePicker::new(PickerMode::EmojiZip, tmp.path().join("emojis"));
        let names: Vec<&str> = picker.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"pack.zip"));
        assert!(
            names.contains(&"PACK2.ZIP"),
            "extension match is case-insensitive"
        );
        assert!(!names.contains(&"readme.txt"));
        assert!(!names.contains(&"cats"), "non-zip file must be hidden");
    }

    #[test]
    fn emoji_zip_mode_still_shows_directories() {
        let tmp = tempfile::tempdir().unwrap();
        create_dir_all(tmp.path().join("root/subdir")).unwrap();
        File::create(tmp.path().join("root/not-a-zip.txt")).unwrap();
        let picker = FilePicker::new(PickerMode::EmojiZip, tmp.path().join("root"));
        let names: Vec<&str> = picker.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"subdir"),
            "directories bypass the extension filter"
        );
        assert!(!names.contains(&"not-a-zip.txt"));
    }

    #[test]
    fn other_modes_are_unaffected_by_extension_filter() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        let names: Vec<&str> = picker.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"dog.jpg"));
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

    // ─── パス直接入力 (`/` → Tab 補完 → Enter) ──────────────────────────

    #[test]
    fn open_path_input_seeds_buffer_with_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        picker.open_path_input();
        let input = picker.path_input.as_ref().expect("path input open");
        assert!(input.buffer.ends_with('/'));
        assert!(
            input
                .buffer
                .starts_with(&*tmp.path().join("photos").to_string_lossy())
        );
    }

    #[test]
    fn cancel_path_input_clears_state_without_touching_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().join("photos"));
        let cwd_before = picker.cwd.clone();
        picker.open_path_input();
        picker.cancel_path_input();
        assert!(picker.path_input.is_none());
        assert_eq!(picker.cwd, cwd_before);
    }

    #[test]
    fn complete_path_input_completes_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: format!("{}/rea", tmp.path().display()),
        });
        picker.complete_path_input();
        let buffer = picker.path_input.as_ref().unwrap().buffer.clone();
        assert!(
            buffer.ends_with("readme.txt"),
            "expected completion to readme.txt, got {buffer}"
        );
    }

    #[test]
    fn complete_path_input_stops_at_common_prefix_for_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        // "photos" と並んで前方一致する 2 件目を追加する ("ph" までは共通)。
        create_dir_all(tmp.path().join("physics")).unwrap();
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: format!("{}/ph", tmp.path().display()),
        });
        picker.complete_path_input();
        let buffer = picker.path_input.as_ref().unwrap().buffer.clone();
        assert!(
            buffer.ends_with("/ph"),
            "should stop at common prefix 'ph' (photos vs physics), got {buffer}"
        );
    }

    #[test]
    fn complete_path_input_appends_slash_for_sole_directory_match() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: format!("{}/pho", tmp.path().display()),
        });
        picker.complete_path_input();
        let buffer = picker.path_input.as_ref().unwrap().buffer.clone();
        assert!(
            buffer.ends_with("photos/"),
            "sole directory match should gain trailing slash, got {buffer}"
        );
    }

    #[test]
    fn complete_path_input_no_match_leaves_buffer_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        let original = format!("{}/zzz_no_such", tmp.path().display());
        picker.path_input = Some(PathInput {
            buffer: original.clone(),
        });
        picker.complete_path_input();
        assert_eq!(picker.path_input.as_ref().unwrap().buffer, original);
    }

    #[test]
    fn submit_path_input_descends_into_directory() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: tmp
                .path()
                .join("photos/cats")
                .to_string_lossy()
                .into_owned(),
        });
        let outcome = picker.submit_path_input();
        assert!(matches!(outcome, PathInputOutcome::Descended));
        assert!(picker.cwd.ends_with("cats"));
        assert!(picker.path_input.is_none());
    }

    #[test]
    fn submit_path_input_selects_file() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: tmp.path().join("readme.txt").to_string_lossy().into_owned(),
        });
        match picker.submit_path_input() {
            PathInputOutcome::Selected(path) => assert!(path.ends_with("readme.txt")),
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn submit_path_input_invalid_path_keeps_buffer_for_correction() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        let bogus = format!("{}/does-not-exist", tmp.path().display());
        picker.path_input = Some(PathInput {
            buffer: bogus.clone(),
        });
        match picker.submit_path_input() {
            PathInputOutcome::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(
            picker.path_input.as_ref().map(|p| p.buffer.clone()),
            Some(bogus),
            "buffer must survive an invalid submission so the user can correct it"
        );
    }

    #[test]
    fn submit_path_input_empty_buffer_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut picker = FilePicker::new(PickerMode::Attachment, tmp.path().to_path_buf());
        picker.path_input = Some(PathInput {
            buffer: "   ".to_string(),
        });
        assert!(matches!(picker.submit_path_input(), PathInputOutcome::Noop));
    }

    #[test]
    fn expand_tilde_uses_home_env() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // HOME 未設定環境ではこのテストは意味を持たないので skip。
        };
        let home_path = PathBuf::from(&home);
        assert_eq!(expand_tilde("~"), home_path);
        assert_eq!(expand_tilde("~/foo"), home_path.join("foo"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn split_path_prefix_splits_on_last_slash() {
        assert_eq!(
            split_path_prefix(Path::new("/a/b/c")),
            (PathBuf::from("/a/b"), "c".to_string())
        );
        assert_eq!(
            split_path_prefix(Path::new("/a/b/")),
            (PathBuf::from("/a/b"), String::new())
        );
        assert_eq!(
            split_path_prefix(Path::new("noslash")),
            (PathBuf::from("."), "noslash".to_string())
        );
    }

    #[test]
    fn longest_common_prefix_basic() {
        assert_eq!(
            longest_common_prefix(["photos", "physics"].into_iter()),
            "ph"
        );
        assert_eq!(longest_common_prefix(["only"].into_iter()), "only");
        assert_eq!(
            longest_common_prefix(std::iter::empty::<&str>()),
            String::new()
        );
    }
}
