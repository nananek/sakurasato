//! 外向き HTTP 先の URL ガード。実装は `sakurasato_core::net_guard` に移管
//! (M5 PR2 で TUI の画像取得からも同関数を呼ぶため `core` クレートに昇格)。
//! 既存の server 側呼び出しは `reqwest::Url` を受けていたが、`reqwest::Url`
//! は `url::Url` の type alias なので追加変換不要。
//!
//! 旧 `host_blocked` / `is_self_host` 公開シグネチャ互換のため薄い再エクスポート
//! を提供する。
//!
//! [PR #35 claude-review]: SSRF 対策の重複防止 (= 配送 / actor fetch / TUI が
//! すべて同一関数を通すこと) は M3b-3 PR2 で決まった不変条件。

pub(crate) use sakurasato_core::net_guard::host_blocked;
pub(crate) use sakurasato_core::net_guard::is_self_host;
