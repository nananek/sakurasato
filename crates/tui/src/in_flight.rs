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
    fn guard_multiple_drops_decrement_correctly() {
        // 複数の guard を任意順で drop しても 1 つずつ正確に dec され、
        // 全部 drop で 0 に戻る。3 件並列の async (= 起動時 timeline +
        // 数本の reaction 等) を想定したスモーク。
        //
        // 注: Relaxed fetch_sub は 0 → usize::MAX に wrap する仕様で、
        // panic は出ない。「new と drop が 1:1 で対応する」前提は呼び出し
        // 側 (= 各 async fn 冒頭で必ず `let _g = ...` を書く) が守る。
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
