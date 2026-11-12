//! 按 action 维度的令牌桶。
//!
//! 设计说明 §4.2 要求组间独立限速；即便 P0 只有一个组，也按 action 分桶，
//! 这样 `exposure` 组上线时不需要重做限速层。

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct RateSpec {
    /// 每秒补充的令牌数。
    pub qps: f64,
    /// 桶容量（允许的突发量）。
    pub burst: f64,
}

impl RateSpec {
    pub fn new(qps: f64, burst: f64) -> Self {
        Self { qps, burst }
    }
}

impl Default for RateSpec {
    fn default() -> Self {
        Self {
            qps: 5.0,
            burst: 5.0,
        }
    }
}

struct Bucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl Bucket {
    fn new(spec: RateSpec) -> Self {
        let capacity = spec.burst.max(1.0);
        Self {
            capacity,
            tokens: capacity,
            refill_per_sec: spec.qps.max(f64::EPSILON),
            last: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last = now;
        }
    }
}

pub struct RateLimiter {
    default_spec: RateSpec,
    overrides: BTreeMap<String, RateSpec>,
    buckets: Mutex<BTreeMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(default_spec: RateSpec, overrides: BTreeMap<String, RateSpec>) -> Self {
        Self {
            default_spec,
            overrides,
            buckets: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn spec_for(&self, action: &str) -> RateSpec {
        self.overrides
            .get(action)
            .copied()
            .unwrap_or(self.default_spec)
    }

    /// 取一个令牌，必要时等待。等待期间不持锁。
    pub async fn acquire(&self, action: &str) {
        self.acquire_n(action, 1.0).await;
    }

    pub async fn acquire_n(&self, action: &str, tokens: f64) {
        loop {
            let wait = {
                let mut buckets = self
                    .buckets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let spec = self.spec_for(action);
                let bucket = buckets
                    .entry(action.to_string())
                    .or_insert_with(|| Bucket::new(spec));
                let now = Instant::now();
                bucket.refill(now);
                if bucket.tokens >= tokens {
                    bucket.tokens -= tokens;
                    return;
                }
                let deficit = tokens - bucket.tokens;
                std::time::Duration::from_secs_f64(deficit / bucket.refill_per_sec)
            };
            if wait.is_zero() {
                continue;
            }
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn burst_is_immediate_then_rate_limited() {
        let limiter = RateLimiter::new(RateSpec::new(10.0, 1.0), BTreeMap::new());
        let start = Instant::now();
        limiter.acquire("DescribeInstances").await;
        limiter.acquire("DescribeInstances").await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(95),
            "第二个令牌应至少等一个补充周期，实际 {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn actions_have_independent_buckets() {
        let mut overrides = BTreeMap::new();
        overrides.insert("DescribeVpcs".to_string(), RateSpec::new(1.0, 1.0));
        let limiter = RateLimiter::new(RateSpec::new(10.0, 1.0), overrides);

        let start = Instant::now();
        limiter.acquire("DescribeVpcs").await;
        limiter.acquire("DescribeInstances").await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "不同 action 不共享桶"
        );
    }
}
