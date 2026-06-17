//! 外向き AP object fetch の **per-domain トークンバケット** レート制限
//! (Issue #269)。
//!
//! ## 背景
//!
//! PR #267 (#266) で followee の `Announce` 受信時に未知 Note を server 直
//! fetch するようになった結果、悪意ある (または侵害された) followee が大量の
//! `Announce` を送ると、server から外部への outbound GET を大量に誘発できる。
//! remote actor fetch (署名検証経路) も同様に無制限なので、フォロー数 / 受信量
//! が増えると外部ドメインへの増幅 `DoS` surface になりうる。
//!
//! ## 方針
//!
//! [`crate::remote_actor::fetch_object_json`] は actor fetch と Note fetch の
//! **唯一の chokepoint** なので、そこで宛先 host ごとにトークンバケットを噛ませ
//! る。お一人様サーバなので上限は緩めで十分 ── 通常運用 (followee の散発的な
//! boost / 未知 actor の署名検証) は burst に収まり、flood 時だけ drop される。
//! drop された fetch は「その boost が表示されない / その署名検証が一時的に
//! 失敗して相手が後で再送する」だけで、機能の本質は壊れない。
//!
//! グローバルではなく **per-domain** にするのは、一つの攻撃元ドメインへの増幅
//! を抑えつつ、無関係な正規ドメインへの fetch を巻き添えにしないため。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// burst 上限 (= bucket 容量)。短時間にこの数までは即時に通す。
const DEFAULT_BURST: f64 = 20.0;

/// sustained rate (1 秒あたりの補充トークン数)。flood が続いてもこの速度まで
/// しか外部 domain を叩かない。お一人様の正規受信ではまず張り付かない値。
const DEFAULT_REFILL_PER_SEC: f64 = 4.0;

/// 追跡 host 数の上限 (メモリ bound)。多数の異なる domain を fetch させて
/// `HashMap` を肥大化させる攻撃を防ぐため、超過時は **満タンに戻った (= もはや
/// 律速していない) idle bucket** を間引く。
const MAX_TRACKED_HOSTS: usize = 4096;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// 宛先 host をキーにしたトークンバケット群。`AppState` に 1 つ持たせて
/// プロセス全体で共有する (内部 `Mutex` で同期)。
#[derive(Debug)]
pub(crate) struct DomainRateLimiter {
    burst: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl DomainRateLimiter {
    pub(crate) fn new() -> Self {
        Self::with_params(DEFAULT_BURST, DEFAULT_REFILL_PER_SEC)
    }

    fn with_params(burst: f64, refill_per_sec: f64) -> Self {
        Self {
            burst,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `host` 宛の fetch を 1 件分試みる。bucket に空きがあれば 1 トークン消費
    /// して `true`、枯渇していれば `false` (= 呼び出し側は fetch を drop)。
    pub(crate) fn try_acquire(&self, host: &str) -> bool {
        self.try_acquire_at(host, Instant::now())
    }

    /// テスト可能なコア。`now` を注入することで時間経過による補充を決定論的に
    /// 検証できる。
    fn try_acquire_at(&self, host: &str, now: Instant) -> bool {
        // `Mutex` poison は「別スレッドが lock 保持中に panic した」場合のみ。
        // バケット状態は単なるカウンタで不変条件を壊さないので、毒された
        // 中身をそのまま使い続けて差し支えない。
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // メモリ bound: 新規 host を入れる前に、満タンに戻った idle bucket を
        // 間引く。満タン = `burst / refill_per_sec` 秒以上アクセスが無い
        // bucket は、再生成しても初期状態 (満タン) と区別がつかないので捨てて
        // よい (= 律速の取りこぼしにならない)。
        if buckets.len() >= MAX_TRACKED_HOSTS && !buckets.contains_key(host) {
            let full_refill = Duration::from_secs_f64(self.burst / self.refill_per_sec);
            buckets.retain(|_, b| now.saturating_duration_since(b.last_refill) < full_refill);
        }

        let burst = self.burst;
        let refill_per_sec = self.refill_per_sec;
        let bucket = buckets.entry(host.to_string()).or_insert_with(|| Bucket {
            tokens: burst,
            last_refill: now,
        });

        // 経過時間ぶんトークンを補充 (上限 burst)。`saturating_duration_since`
        // で時計が巻き戻っても負にならない。
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(burst);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for DomainRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_denies() {
        let rl = DomainRateLimiter::with_params(5.0, 1.0);
        let now = Instant::now();
        // burst ぶん (5) は同一時刻でも通る。
        for i in 0..5 {
            assert!(
                rl.try_acquire_at("a.test", now),
                "burst token {i} must pass"
            );
        }
        // 6 件目は枯渇で弾かれる。
        assert!(
            !rl.try_acquire_at("a.test", now),
            "over-burst must be denied"
        );
    }

    #[test]
    fn refills_over_time() {
        let rl = DomainRateLimiter::with_params(5.0, 2.0);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(rl.try_acquire_at("a.test", t0));
        }
        assert!(!rl.try_acquire_at("a.test", t0));
        // 1 秒で 2 トークン補充 → 2 件通って 3 件目は枯渇。
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.try_acquire_at("a.test", t1));
        assert!(rl.try_acquire_at("a.test", t1));
        assert!(!rl.try_acquire_at("a.test", t1));
    }

    #[test]
    fn refill_is_capped_at_burst() {
        let rl = DomainRateLimiter::with_params(5.0, 100.0);
        let t0 = Instant::now();
        // 長時間放置しても burst を超えて貯まらない。
        let t_far = t0 + Duration::from_hours(1);
        for _ in 0..5 {
            assert!(rl.try_acquire_at("a.test", t_far));
        }
        assert!(!rl.try_acquire_at("a.test", t_far));
    }

    #[test]
    fn hosts_are_independent() {
        let rl = DomainRateLimiter::with_params(2.0, 1.0);
        let now = Instant::now();
        assert!(rl.try_acquire_at("a.test", now));
        assert!(rl.try_acquire_at("a.test", now));
        assert!(!rl.try_acquire_at("a.test", now));
        // 別 host は別バケット。
        assert!(rl.try_acquire_at("b.test", now));
        assert!(rl.try_acquire_at("b.test", now));
        assert!(!rl.try_acquire_at("b.test", now));
    }

    #[test]
    fn idle_buckets_are_evicted_under_pressure() {
        // burst/refill = 1 秒で満タンに戻る設定。eviction の閾値検証。
        let rl = DomainRateLimiter::with_params(4.0, 4.0);
        let t0 = Instant::now();
        // MAX_TRACKED_HOSTS を超える数の host を 1 回ずつ叩く。
        for i in 0..(MAX_TRACKED_HOSTS + 10) {
            assert!(rl.try_acquire_at(&format!("h{i}.test"), t0));
        }
        // すべて同一時刻なので、追加 host 投入時の retain は full_refill (1s)
        // 未満 = 1 件も間引かれず、map は超過したまま入る (= 取りこぼし無し)。
        {
            let n = rl.buckets.lock().unwrap().len();
            assert!(n > MAX_TRACKED_HOSTS, "no eviction while all buckets fresh");
        }
        // 1 秒後に満タンへ戻った状態で新 host を入れると idle が間引かれる。
        let t1 = t0 + Duration::from_secs(2);
        assert!(rl.try_acquire_at("fresh.test", t1));
        let n = rl.buckets.lock().unwrap().len();
        assert!(
            n <= MAX_TRACKED_HOSTS,
            "idle buckets evicted back under the cap (got {n})",
        );
    }
}
