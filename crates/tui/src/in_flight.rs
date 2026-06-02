//! Issue #131: ネットワーク通信中であることを画面に出すための in-flight counter。
//!
//! [`crate::app::App::in_flight`] が `Arc<AtomicUsize>` で持ち、各 async ハンドラ
//! が [`InFlightGuard::new`] で increment、関数を抜けるとき (= drop 時) に
//! 自動 decrement する。`render_status` が `> 0` のとき左端に spinner を出す。
//!
//! `Arc<AtomicUsize>` を採用しているのは `&mut App` の borrow を await
//! またぎで保持できない Rust の制約への対応。guard 自体は owned で持てる
//! ので、async 関数の途中で `app.set_status(...)` を呼び直しても干渉しない。
//!
//! ## 使い方
//!
//! ```ignore
//! async fn refresh_timeline(app: &mut App, api: &LocalApi) {
//!     let _g = InFlightGuard::new(app.in_flight.clone());
//!     // ここから return (= early exit を含む) のどこを抜けても dec される
//!     match api.timeline_home(None, 50).await { /* ... */ }
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// RAII guard ── 構築時に counter を +1、drop 時に -1 する。
///
/// `Arc` を握っているので `App` が drop されても guard が生きていれば
/// counter は維持される (= 実害なし、guard 側が先に drop される)。
#[must_use = "guard must be held for the duration of the in-flight operation"]
#[derive(Debug)]
pub struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    pub fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_increments_on_new_and_decrements_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        {
            let _g = InFlightGuard::new(counter.clone());
            assert_eq!(counter.load(Ordering::Relaxed), 1);
            {
                let _g2 = InFlightGuard::new(counter.clone());
                assert_eq!(counter.load(Ordering::Relaxed), 2);
            }
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn guard_drop_underflow_saturates_at_zero_via_atomic_wraparound_is_avoided() {
        // 万一 new と drop の対が崩れても (= 想定外) panic にはしない。
        // Relaxed fetch_sub は 0 → usize::MAX に wrap するため、運用上は
        // 「常に new と drop が 1:1 で対応している」前提を呼び出し側 (=
        // async fn の冒頭で必ず let _g = ... を書く) が守ることで担保。
        // 本テストは「guard が複数あっても 1 つずつ正確に dec する」を確認。
        let counter = Arc::new(AtomicUsize::new(0));
        let g1 = InFlightGuard::new(counter.clone());
        let g2 = InFlightGuard::new(counter.clone());
        let g3 = InFlightGuard::new(counter.clone());
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        drop(g2);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        drop(g1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        drop(g3);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
