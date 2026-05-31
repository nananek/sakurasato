//! 視覚刺激抑制モード (= per-element image suppression, M9 PR2)。
//!
//! 「画像を全部消す」(M5 PR2 の `--no-images`) ではなく、要素別に細かく
//! トグルできるようにする。お一人様 TUI の「端末の中の静かな隠れ家」
//! コンセプト (CLAUDE.md §1) に合わせ、ユーザが落ち着きたい程度に応じて
//! アバター / 添付 / カスタム絵文字 / プレビュー / アニメ をそれぞれ
//! on/off できる。
//!
//! ## 要素一覧
//!
//! | element | 既定 | 影響箇所 |
//! |---|---|---|
//! | `avatar` | on | timeline 各 note の発信者アイコン (`ui::render_avatar`) |
//! | `attachment` | on | (将来) note 添付画像のサムネ表示 |
//! | `emoji` | on | (将来) カスタム絵文字のインライン表示 |
//! | `preview` | on | ファイルピッカでローカル画像をプレビューする (`ui::render_picker_preview`) |
//! | `animation` | on | GIF / APNG / animated WebP のアニメ再生。off = 1 フレーム目だけ |
//!
//! ## アニメ
//!
//! ratatui-image / image クレートのレンダリングはそもそも単一フレームの
//! `DynamicImage` に落とすため、現状は **常に静止画** で表示される (= 1
//! フレーム目)。`animation` トグルは「将来 ratatui-image がアニメ対応した
//! 時の予約席」として残し、現状は常に static として扱われる (= 設定
//! トグルが render 経路に影響しない)。本モジュールではフラグだけ用意して
//! おき、UI で「アニメ抑制 ON」を見える化する。
//!
//! ## CLI
//!
//! `--no-images` は **全要素を一括 off** にするキルスイッチとして残す。
//! `--no-avatars` / `--no-attachments` / `--no-emojis` / `--no-previews`
//! / `--no-animations` で個別にも切れる。`--no-images` と細粒度フラグが
//! 同時指定された場合は **`--no-images` が最優先で全要素 off** に倒し、
//! 個別 `--no-*` は (どのみち off になるので) 評価しない。「一括 off の後に
//! 個別 on で戻す」用途は `--*-on` フラグを提供せず、ランタイムの `i`
//! overlay (キーバインド `*`) で復帰させる設計にしている ── ただし起動時
//! に全要素 off で来た場合は Picker が None で固定されるため、ランタイム
//! 復帰でも実際の画像描画は出ない (M9 PR2 review Finding 3 で明示の警告
//! メッセージを出すよう改修済)。

use serde::Deserialize;

/// 各要素の表示 on/off。true = 表示する、false = 抑制する。
///
/// 5 つの独立した要素を持つため bool 5 つになる。state machine 化すると
/// 「要素別 on/off」というユーザ視点の操作と乖離して読みづらくなるので、
/// pedantic 警告は明示的に許可している。
#[allow(
    clippy::struct_excessive_bools,
    reason = "要素別トグルは bool 5 つが最自然 (state machine 化は UX と乖離)"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSuppression {
    pub avatar: bool,
    pub attachment: bool,
    pub emoji: bool,
    pub preview: bool,
    pub animation: bool,
}

impl Default for ImageSuppression {
    fn default() -> Self {
        Self::all_on()
    }
}

impl ImageSuppression {
    /// 全要素 on (= 視覚刺激抑制なし)。
    #[must_use]
    pub const fn all_on() -> Self {
        Self {
            avatar: true,
            attachment: true,
            emoji: true,
            preview: true,
            animation: true,
        }
    }

    /// 全要素 off (= 完全テキスト UI、`--no-images` 相当)。
    #[must_use]
    pub const fn all_off() -> Self {
        Self {
            avatar: false,
            attachment: false,
            emoji: false,
            preview: false,
            animation: false,
        }
    }

    /// どれか 1 つでも有効なら `true`。Picker 取得自体は (= 端末問い合わせ
    /// による初期化コスト) どれか 1 要素でも使う場合だけ走らせたい。
    ///
    /// `animation` も含めて 5 要素すべてを OR で見る ── `disable_all()` /
    /// `all_off()` が animation も触る以上、`any_enabled()` も対称的に判定
    /// しないと「`--no-avatars --no-attachments --no-emojis --no-previews`
    /// だけで起動 → animation=true なのに `any_enabled()=false` で Picker 未
    /// 初期化」のサイレント不整合を起こす ([[m9-pr2-review]] Finding 2)。
    #[must_use]
    pub const fn any_enabled(&self) -> bool {
        self.avatar || self.attachment || self.emoji || self.preview || self.animation
    }

    /// `--no-images` のような単一フラグ → 全要素 off に倒す。
    pub fn disable_all(&mut self) {
        *self = Self::all_off();
    }

    /// 1 要素のトグル。runtime キーバインドから呼ぶ。
    pub fn toggle(&mut self, kind: Element) {
        match kind {
            Element::Avatar => self.avatar = !self.avatar,
            Element::Attachment => self.attachment = !self.attachment,
            Element::Emoji => self.emoji = !self.emoji,
            Element::Preview => self.preview = !self.preview,
            Element::Animation => self.animation = !self.animation,
        }
    }

    /// `kind` がオンか。
    #[must_use]
    pub const fn is_on(&self, kind: Element) -> bool {
        match kind {
            Element::Avatar => self.avatar,
            Element::Attachment => self.attachment,
            Element::Emoji => self.emoji,
            Element::Preview => self.preview,
            Element::Animation => self.animation,
        }
    }
}

/// 要素種別。`ImageSuppression::toggle` / `is_on` で参照する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Element {
    Avatar,
    Attachment,
    Emoji,
    Preview,
    Animation,
}

impl Element {
    /// UI に出すラベル ("avatar" / "attachment" / ...)。help / overlay 用。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Attachment => "attachment",
            Self::Emoji => "emoji",
            Self::Preview => "preview",
            Self::Animation => "animation",
        }
    }

    /// 全要素を順序付きで列挙 (overlay 描画 / toggle UI 用)。`Avatar` → ...
    /// → `Animation` の固定順。
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Avatar,
            Self::Attachment,
            Self::Emoji,
            Self::Preview,
            Self::Animation,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_all_on() {
        let s = ImageSuppression::default();
        for e in Element::all() {
            assert!(s.is_on(e), "{e:?} should default to on");
        }
        assert!(s.any_enabled());
    }

    #[test]
    fn disable_all_turns_off_every_element() {
        let mut s = ImageSuppression::default();
        s.disable_all();
        for e in Element::all() {
            assert!(!s.is_on(e), "{e:?} should be off after disable_all");
        }
        assert!(!s.any_enabled());
    }

    #[test]
    fn toggle_flips_one_element() {
        let mut s = ImageSuppression::default();
        s.toggle(Element::Avatar);
        assert!(!s.is_on(Element::Avatar));
        assert!(s.is_on(Element::Attachment));
        s.toggle(Element::Avatar);
        assert!(s.is_on(Element::Avatar));
    }

    #[test]
    fn any_enabled_includes_animation_field() {
        // [[m9-pr2-review]] Finding 2 回帰: avatar/attachment/emoji/preview を
        // 全部 off にしても animation だけ on なら any_enabled は true で
        // ないと Picker 初期化スキップでサイレント壊れる。
        let s = ImageSuppression {
            avatar: false,
            attachment: false,
            emoji: false,
            preview: false,
            animation: true,
        };
        assert!(s.any_enabled());
    }

    #[test]
    fn animation_toggle_does_not_affect_static_elements() {
        let mut s = ImageSuppression::default();
        s.toggle(Element::Animation);
        // animation off でも avatar/preview は依然 on (== ratatui-image は
        // どのみち静止フレームを返す)。
        assert!(s.is_on(Element::Avatar));
        assert!(s.is_on(Element::Preview));
        assert!(!s.is_on(Element::Animation));
    }
}
