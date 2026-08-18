//! 破壊的操作の Yes/No 確認オーバーレイ (ユーザーブロック PR6 / 連合ドメイン
//! ブロック PR7 で共用、計画書 §6.7)。
//!
//! 既存 `alt_prompt.rs` は 1 行テキスト入力オーバーレイであり Yes/No 確認
//! 機構ではないことを実装調査で確認済み (計画書 §10 確定事項 #4)。本モジュール
//! はそれとは別に、「メッセージを表示し `y`/`Enter` で確定・`n`/`Esc` で
//! キャンセルするだけ」の最小オーバーレイを提供する。
//!
//! 呼び出し元画面 (Profile / `DomainDetail`) の target 情報 (actor id /
//! host) は state を複製せず、それぞれの既存 screen state
//! (`App::profile_stack` / `App::domain_detail`) からそのまま読む ──
//! `ConfirmPrompt` 自体は「何を確認しているか」の種別 (`ConfirmKind`) と
//! 表示文言、戻り先 `Focus` だけを持つ。

use crate::app::Focus;

/// 確認待ちの操作の種別。`crate::runtime` の `Action::ConfirmYes` ハンドラが
/// これを見て実処理に分岐する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Profile 画面: 選択中 actor をブロックする (ユーザーブロック PR6)。
    BlockActor,
    /// `DomainDetail` 画面: 選択中ホストを suspend する (連合ドメインブロック PR7)。
    SuspendDomain,
}

/// 確認オーバーレイの state。`App::confirm_prompt` が `Some` のあいだだけ
/// `Focus::ConfirmPrompt` を取りうる。
#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    /// オーバーレイに表示するメッセージ (例: `"block @bob@example.test?"`)。
    pub message: String,
    pub kind: ConfirmKind,
    /// キャンセル / 確定後に戻る `Focus` (呼び出し元画面)。
    pub return_focus: Focus,
}

impl ConfirmPrompt {
    #[must_use]
    pub fn new(message: impl Into<String>, kind: ConfirmKind, return_focus: Focus) -> Self {
        Self {
            message: message.into(),
            kind,
            return_focus,
        }
    }
}
