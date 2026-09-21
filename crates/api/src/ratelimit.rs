use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const REDIS_TIMEOUT: Duration = Duration::from_millis(100);

const MAX_BUCKETS: usize = 10_000;

type Buckets = Arc<Mutex<HashMap<String, (u32, Instant)>>>;

#[derive(Clone)]
pub enum RateLimitState {
    InMemory {
        buckets: Buckets,
        max_requests: u32,
        window: Duration,
    },
    Redis {
        pool: deadpool_redis::Pool,
        max_requests: u32,
        window_secs: u64,
        fallback: Buckets,
        degraded_logged_at: Arc<AtomicU64>,
    },
}

impl RateLimitState {
    pub fn new(max_requests: u32, window: Duration) -> Self {
        RateLimitState::InMemory {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            max_requests,
            window,
        }
    }

    pub fn redis(pool: deadpool_redis::Pool, max_requests: u32, window: Duration) -> Self {
        RateLimitState::Redis {
            pool,
            max_requests,
            window_secs: window.as_secs().max(1),
            fallback: Arc::new(Mutex::new(HashMap::new())),
            degraded_logged_at: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn allow(&self, key: &str) -> bool {
        match self {
            RateLimitState::InMemory {
                buckets,
                max_requests,
                window,
            } => Self::allow_memory(buckets, *max_requests, *window, key),
            RateLimitState::Redis {
                pool,
                max_requests,
                window_secs,
                fallback,
                degraded_logged_at,
            } => {
                match tokio::time::timeout(REDIS_TIMEOUT, Self::redis_incr(pool, *window_secs, key))
                    .await
                {
                    Ok(Ok(count)) => count <= *max_requests as i64,
                    Ok(Err(e)) => {
                        Self::note_degraded(degraded_logged_at, &e.to_string());
                        Self::allow_memory(
                            fallback,
                            *max_requests,
                            Duration::from_secs(*window_secs),
                            key,
                        )
                    }
                    Err(_) => {
                        Self::note_degraded(degraded_logged_at, "timeout");
                        Self::allow_memory(
                            fallback,
                            *max_requests,
                            Duration::from_secs(*window_secs),
                            key,
                        )
                    }
                }
            }
        }
    }

    pub async fn over_limit(&self, key: &str) -> bool {
        match self {
            RateLimitState::InMemory {
                buckets,
                max_requests,
                window,
            } => Self::peek_memory(buckets, *max_requests, *window, key),
            RateLimitState::Redis {
                pool,
                max_requests,
                window_secs,
                fallback,
                degraded_logged_at,
            } => match tokio::time::timeout(REDIS_TIMEOUT, Self::redis_get(pool, key)).await {
                Ok(Ok(count)) => count >= *max_requests as i64,
                Ok(Err(e)) => {
                    Self::note_degraded(degraded_logged_at, &e.to_string());
                    Self::peek_memory(
                        fallback,
                        *max_requests,
                        Duration::from_secs(*window_secs),
                        key,
                    )
                }
                Err(_) => {
                    Self::note_degraded(degraded_logged_at, "timeout");
                    Self::peek_memory(
                        fallback,
                        *max_requests,
                        Duration::from_secs(*window_secs),
                        key,
                    )
                }
            },
        }
    }

    pub async fn hit(&self, key: &str) {
        let _ = self.allow(key).await;
    }

    fn allow_memory(buckets: &Buckets, max_requests: u32, window: Duration, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = buckets.lock().expect("rate-limit mutex poisoned");
        match buckets.get_mut(key) {
            Some((count, start)) if now.duration_since(*start) < window => {
                if *count >= max_requests {
                    false
                } else {
                    *count += 1;
                    true
                }
            }
            _ => {
                if buckets.len() >= MAX_BUCKETS {
                    buckets.retain(|_, (_, start)| now.duration_since(*start) < window);
                }
                buckets.insert(key.to_string(), (1, now));
                true
            }
        }
    }

    fn peek_memory(buckets: &Buckets, max_requests: u32, window: Duration, key: &str) -> bool {
        let now = Instant::now();
        let buckets = buckets.lock().expect("rate-limit mutex poisoned");
        matches!(buckets.get(key), Some((count, start))
            if now.duration_since(*start) < window && *count >= max_requests)
    }

    fn note_degraded(last: &AtomicU64, why: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let prev = last.load(Ordering::Relaxed);
        if now.saturating_sub(prev) >= 60
            && last
                .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::error!(
                reason = %why,
                "rate limiter: Redis unavailable — degraded to per-instance in-memory limiting"
            );
        }
    }

    async fn redis_incr(
        pool: &deadpool_redis::Pool,
        window_secs: u64,
        key: &str,
    ) -> Result<i64, deadpool_redis::PoolError> {
        let mut conn = pool.get().await?;
        let rkey = format!("ratelimit:{key}");
        let (count,): (i64,) = deadpool_redis::redis::pipe()
            .atomic()
            .cmd("SET")
            .arg(&rkey)
            .arg(0)
            .arg("NX")
            .arg("EX")
            .arg(window_secs)
            .ignore()
            .incr(&rkey, 1)
            .query_async(&mut conn)
            .await?;
        Ok(count)
    }

    async fn redis_get(
        pool: &deadpool_redis::Pool,
        key: &str,
    ) -> Result<i64, deadpool_redis::PoolError> {
        let mut conn = pool.get().await?;
        let rkey = format!("ratelimit:{key}");
        let count: Option<i64> = deadpool_redis::redis::cmd("GET")
            .arg(&rkey)
            .query_async(&mut conn)
            .await?;
        Ok(count.unwrap_or(0))
    }
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new(300, Duration::from_secs(60))
    }
}
