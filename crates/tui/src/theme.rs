//! テーマ (カラースキーム) ── `config/themes/*.toml` から読み込み、
//! ratatui の [`Color`] に変換する層。
//!
//! # 設計方針
//!
//! - **色のハードコード禁止** (CLAUDE.md §10)。UI 描画は必ず [`Theme`] のフィールドを
//!   参照する。直接 `Color::Rgb(...)` を書くのは禁止。
//! - 組み込みテーマ 3 種 (sakura / dark / light) は `include_str!` で
//!   コンパイル時に焼き込み、`config/themes/` が存在しないコンテナでも
//!   必ず起動できる。
//! - ユーザ提供の TOML はフィールド単位でオーバライド可能 (`with_overrides`)。
//!   一部だけ書いた TOML でも残りは組み込み既定で埋まる。
//!
//! # 色の表記
//!
//! - 16 進: `#rrggbb` / `#rgb` (大小文字いずれも)
//! - 名前: `"black"` / `"red"` / `"lightblue"` / `"white"` / `"reset"` 等
//!   crossterm の名前 + ratatui の `LightXxx` を受け入れる。
//! - 256 色: `"u8:42"` 形式 (= ratatui `Color::Indexed(42)`)
//!
//! 大文字小文字は区別しない。先頭/末尾の空白はトリムする。

use std::collections::HashMap;
use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

/// 組み込みテーマの TOML ソース。`Theme::builtin` から参照する。
const BUILTIN_SAKURA: &str = include_str!("../../../config/themes/sakura.toml");
const BUILTIN_DARK: &str = include_str!("../../../config/themes/dark.toml");
const BUILTIN_LIGHT: &str = include_str!("../../../config/themes/light.toml");

/// 1 テーマ分のカラーパレット。すべてのフィールドが `Color` に解決済み。
///
/// 新しい UI フィールドが必要になったらここに足し、すべての組み込みテーマ
/// TOML を併せて更新すること。フィールド名は TOML キーと一致する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub description: String,
    pub dark: bool,
    pub palette: Palette,
}

#[allow(clippy::struct_field_names, reason = "TOML キー名と 1 対 1 で揃える")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Color,
    pub foreground: Color,
    pub muted: Color,
    pub border: Color,
    pub selection: Color,
    pub accent: Color,
    pub accent_strong: Color,
    pub status_bar_bg: Color,
    pub status_bar_fg: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    pub link: Color,
    pub cw_marker: Color,
}

/// 色の表記 → `Color` を解決するときに起こりうるエラー。
#[derive(Debug, thiserror::Error)]
pub enum ColorParseError {
    #[error("empty color literal")]
    Empty,
    #[error("invalid hex color `{0}`")]
    InvalidHex(String),
    #[error("invalid 8-bit indexed color `{0}`")]
    InvalidIndexed(String),
    #[error("unknown color name `{0}`")]
    UnknownName(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("failed to read theme file {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse theme TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid color for `{field}`: {source}")]
    Color {
        field: &'static str,
        #[source]
        source: ColorParseError,
    },
    #[error("unknown builtin theme `{0}` (available: sakura, dark, light)")]
    UnknownBuiltin(String),
}

impl Theme {
    /// 組み込みテーマ名 (`sakura`/`dark`/`light`) を解決する。お一人様
    /// サーバなので組み込みは 3 つに絞る。
    pub fn builtin(name: &str) -> Result<Self, ThemeError> {
        let src = match name.trim().to_ascii_lowercase().as_str() {
            "sakura" => BUILTIN_SAKURA,
            "dark" => BUILTIN_DARK,
            "light" => BUILTIN_LIGHT,
            other => return Err(ThemeError::UnknownBuiltin(other.into())),
        };
        Self::from_toml_str(src)
    }

    /// 組み込みテーマ一覧。`--list-themes` 用。
    pub fn builtin_names() -> &'static [&'static str] {
        &["sakura", "dark", "light"]
    }

    /// 完全な TOML 文字列をパースする。`palette` の全フィールドが必要。
    pub fn from_toml_str(src: &str) -> Result<Self, ThemeError> {
        let raw: RawTheme = toml::from_str(src)?;
        raw.try_into_theme()
    }

    /// TOML ファイルから読み込む。
    pub fn from_path(path: &Path) -> Result<Self, ThemeError> {
        let src = std::fs::read_to_string(path).map_err(|e| ThemeError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::from_toml_str(&src)
    }

    /// 既定テーマ (`sakura`) に、ユーザ TOML の一部フィールドだけを上書きする。
    /// `overlay_src` は palette テーブルだけを含む不完全な TOML でもよい。
    pub fn with_overrides(base: &str, overlay_src: &str) -> Result<Self, ThemeError> {
        let base_theme = Self::builtin(base)?;
        let overlay: PartialTheme = toml::from_str(overlay_src)?;
        overlay.apply_to(base_theme)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin("sakura").expect("builtin sakura theme is valid")
    }
}

#[derive(Debug, Deserialize)]
struct RawTheme {
    #[serde(default)]
    meta: RawMeta,
    palette: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    dark: bool,
}

impl RawTheme {
    fn try_into_theme(self) -> Result<Theme, ThemeError> {
        let pal = &self.palette;
        Ok(Theme {
            name: self.meta.name,
            description: self.meta.description,
            dark: self.meta.dark,
            palette: Palette {
                background: required_color(pal, "background")?,
                foreground: required_color(pal, "foreground")?,
                muted: required_color(pal, "muted")?,
                border: required_color(pal, "border")?,
                selection: required_color(pal, "selection")?,
                accent: required_color(pal, "accent")?,
                accent_strong: required_color(pal, "accent_strong")?,
                status_bar_bg: required_color(pal, "status_bar_bg")?,
                status_bar_fg: required_color(pal, "status_bar_fg")?,
                warning: required_color(pal, "warning")?,
                error: required_color(pal, "error")?,
                success: required_color(pal, "success")?,
                link: required_color(pal, "link")?,
                cw_marker: required_color(pal, "cw_marker")?,
            },
        })
    }
}

/// ユーザ定義 TOML 用。palette の一部フィールドだけを差し込む。
#[derive(Debug, Default, Deserialize)]
struct PartialTheme {
    #[serde(default)]
    meta: Option<RawMeta>,
    #[serde(default)]
    palette: HashMap<String, String>,
}

impl PartialTheme {
    fn apply_to(self, mut base: Theme) -> Result<Theme, ThemeError> {
        if let Some(meta) = self.meta {
            if !meta.name.is_empty() {
                base.name = meta.name;
            }
            if !meta.description.is_empty() {
                base.description = meta.description;
            }
            // dark フラグはユーザ側を信用する (bool は default=false なので
            // 「明示的に書いたか」が判別できないが、テーマ全体上書きの場合は
            // 既に from_toml_str 経由でくる)。
            base.dark = meta.dark || base.dark;
        }
        for (k, v) in self.palette {
            let color = parse_color(&v).map_err(|e| ThemeError::Color {
                field: field_static_name(&k),
                source: e,
            })?;
            assign_palette_field(&mut base.palette, &k, color).map_err(|e| ThemeError::Color {
                field: field_static_name(&k),
                source: e,
            })?;
        }
        Ok(base)
    }
}

fn required_color(map: &HashMap<String, String>, field: &'static str) -> Result<Color, ThemeError> {
    let raw = map
        .get(field)
        .ok_or(ThemeError::Color {
            field,
            source: ColorParseError::Empty,
        })?
        .clone();
    parse_color(&raw).map_err(|e| ThemeError::Color { field, source: e })
}

fn assign_palette_field(pal: &mut Palette, key: &str, color: Color) -> Result<(), ColorParseError> {
    match key {
        "background" => pal.background = color,
        "foreground" => pal.foreground = color,
        "muted" => pal.muted = color,
        "border" => pal.border = color,
        "selection" => pal.selection = color,
        "accent" => pal.accent = color,
        "accent_strong" => pal.accent_strong = color,
        "status_bar_bg" => pal.status_bar_bg = color,
        "status_bar_fg" => pal.status_bar_fg = color,
        "warning" => pal.warning = color,
        "error" => pal.error = color,
        "success" => pal.success = color,
        "link" => pal.link = color,
        "cw_marker" => pal.cw_marker = color,
        other => return Err(ColorParseError::UnknownName(other.into())),
    }
    Ok(())
}

fn field_static_name(key: &str) -> &'static str {
    match key {
        "background" => "background",
        "foreground" => "foreground",
        "muted" => "muted",
        "border" => "border",
        "selection" => "selection",
        "accent" => "accent",
        "accent_strong" => "accent_strong",
        "status_bar_bg" => "status_bar_bg",
        "status_bar_fg" => "status_bar_fg",
        "warning" => "warning",
        "error" => "error",
        "success" => "success",
        "link" => "link",
        "cw_marker" => "cw_marker",
        _ => "(unknown)",
    }
}

/// 文字列を ratatui [`Color`] にパースする。書式は module-level doc 参照。
pub fn parse_color(raw: &str) -> Result<Color, ColorParseError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ColorParseError::Empty);
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex).ok_or_else(|| ColorParseError::InvalidHex(raw.into()));
    }
    if let Some(idx) = s.strip_prefix("u8:") {
        return idx
            .parse::<u8>()
            .map(Color::Indexed)
            .map_err(|_| ColorParseError::InvalidIndexed(raw.into()));
    }
    parse_named(s).ok_or_else(|| ColorParseError::UnknownName(raw.into()))
}

fn parse_hex(hex: &str) -> Option<Color> {
    let bytes = hex.as_bytes();
    match bytes.len() {
        // `#rgb` を `#rrggbb` に展開。
        3 => {
            let r = hex_digit(bytes[0])?;
            let g = hex_digit(bytes[1])?;
            let b = hex_digit(bytes[2])?;
            Some(Color::Rgb(r * 17, g * 17, b * 17))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_named(name: &str) -> Option<Color> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_sakura_loads() {
        let theme = Theme::builtin("sakura").expect("builtin sakura");
        assert_eq!(theme.name, "Sakurasato Sakura");
        assert!(theme.dark);
        // pink accent (#f6c1d6)
        assert_eq!(theme.palette.accent, Color::Rgb(0xf6, 0xc1, 0xd6));
    }

    #[test]
    fn builtin_dark_and_light_load() {
        let dark = Theme::builtin("dark").expect("dark");
        let light = Theme::builtin("light").expect("light");
        assert!(dark.dark);
        assert!(!light.dark);
        assert_ne!(dark.palette.background, light.palette.background);
    }

    #[test]
    fn unknown_builtin_errors() {
        let err = Theme::builtin("rainbow").unwrap_err();
        assert!(matches!(err, ThemeError::UnknownBuiltin(_)));
    }

    #[test]
    fn parse_color_hex_six() {
        assert_eq!(parse_color("#ff8800").unwrap(), Color::Rgb(255, 136, 0));
        assert_eq!(parse_color("  #FF8800 ").unwrap(), Color::Rgb(255, 136, 0));
    }

    #[test]
    fn parse_color_hex_three() {
        assert_eq!(parse_color("#abc").unwrap(), Color::Rgb(0xaa, 0xbb, 0xcc));
    }

    #[test]
    fn parse_color_indexed() {
        assert_eq!(parse_color("u8:42").unwrap(), Color::Indexed(42));
        assert!(parse_color("u8:300").is_err());
        assert!(parse_color("u8:").is_err());
    }

    #[test]
    fn parse_color_named() {
        assert_eq!(parse_color("red").unwrap(), Color::Red);
        assert_eq!(parse_color("LightBlue").unwrap(), Color::LightBlue);
        assert_eq!(parse_color("gray").unwrap(), Color::Gray);
        assert_eq!(parse_color("grey").unwrap(), Color::Gray);
        assert!(parse_color("indigo").is_err());
    }

    #[test]
    fn parse_color_rejects_empty() {
        assert!(matches!(
            parse_color("").unwrap_err(),
            ColorParseError::Empty
        ));
        assert!(matches!(
            parse_color("   ").unwrap_err(),
            ColorParseError::Empty
        ));
    }

    #[test]
    fn parse_color_rejects_bad_hex() {
        assert!(matches!(
            parse_color("#zz0000").unwrap_err(),
            ColorParseError::InvalidHex(_)
        ));
        assert!(matches!(
            parse_color("#abcd").unwrap_err(),
            ColorParseError::InvalidHex(_)
        ));
    }

    #[test]
    fn overlay_overrides_only_specified_fields() {
        let overlay = "[palette]\naccent = \"#00ff00\"\n";
        let theme = Theme::with_overrides("sakura", overlay).unwrap();
        assert_eq!(theme.palette.accent, Color::Rgb(0, 255, 0));
        // 他のフィールドは base のまま
        let base = Theme::builtin("sakura").unwrap();
        assert_eq!(theme.palette.background, base.palette.background);
    }

    #[test]
    fn overlay_unknown_field_errors() {
        let overlay = "[palette]\ngalaxy = \"#000000\"\n";
        let err = Theme::with_overrides("sakura", overlay).unwrap_err();
        assert!(matches!(err, ThemeError::Color { .. }));
    }

    #[test]
    fn from_path_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, include_str!("../../../config/themes/sakura.toml")).unwrap();
        let theme = Theme::from_path(&path).unwrap();
        assert_eq!(theme.name, "Sakurasato Sakura");
    }
}
