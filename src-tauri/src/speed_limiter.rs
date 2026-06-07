//! Token-bucket speed limiter for bandwidth throttling.
//!
//! Provides a global rate limiter shared across all active download segments.
//! Rate of 0 means unlimited throughput.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

/// A thread-safe token-bucket speed limiter.
///
/// The limiter uses atomic operations for the rate and token count,
/// and a mutex-protected `Instant` for tracking the last refill time.
///
/// # Algorithm
///
/// - Capacity equals the configured rate (1 second's worth of tokens).
/// - Tokens are refilled based on elapsed time since the last refill.
/// - `acquire(bytes)` loops: refill → try to consume tokens via CAS → sleep if deficit.
/// - Rate of 0 means unlimited — `acquire` returns immediately.
/// - Refill granularity targets ~100ms intervals for smooth throughput.
#[derive(Clone)]
pub struct SpeedLimiter {
    inner: Arc<SpeedLimiterInner>,
}

struct SpeedLimiterInner {
    /// Configured rate in bytes per second. 0 = unlimited.
    rate: AtomicU64,
    /// Current available tokens (bytes that can be written).
    tokens: AtomicU64,
    /// Last refill timestamp, protected by a mutex for accurate elapsed tracking.
    last_refill: Mutex<Instant>,
}

impl SpeedLimiter {
    /// Create a new speed limiter with the given rate in bytes per second.
    ///
    /// A rate of 0 means unlimited (acquire always returns immediately).
    pub fn new(bytes_per_sec: u64) -> Self {
        let tokens = if bytes_per_sec == 0 { 0 } else { bytes_per_sec };

        Self {
            inner: Arc::new(SpeedLimiterInner {
                rate: AtomicU64::new(bytes_per_sec),
                tokens: AtomicU64::new(tokens),
                last_refill: Mutex::new(Instant::now()),
            }),
        }
    }

    /// Update the rate dynamically without restarting downloads.
    ///
    /// The new rate takes effect within the next acquire call (~100ms).
    /// If the new rate is lower than current tokens, tokens are capped
    /// to the new capacity on the next refill.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        let old_rate = self.inner.rate.swap(bytes_per_sec, Ordering::Relaxed);

        // If switching from unlimited to limited, seed tokens
        if old_rate == 0 && bytes_per_sec > 0 {
            self.inner.tokens.store(bytes_per_sec, Ordering::Relaxed);
        }
        // If new rate is lower, cap tokens to new capacity
        if bytes_per_sec > 0 {
            let current = self.inner.tokens.load(Ordering::Relaxed);
            if current > bytes_per_sec {
                self.inner.tokens.store(bytes_per_sec, Ordering::Relaxed);
            }
        }
    }

    /// Acquire `bytes` tokens, blocking until they are available.
    ///
    /// If rate is 0 (unlimited), returns immediately.
    /// Otherwise, refills tokens based on elapsed time and consumes
    /// via compare-and-swap. Sleeps proportionally to the deficit
    /// when tokens are insufficient.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }

        loop {
            let rate = self.inner.rate.load(Ordering::Relaxed);

            // Unlimited mode — no throttling
            if rate == 0 {
                return;
            }

            // Refill tokens based on elapsed time
            self.refill(rate).await;

            // Try to consume tokens via CAS loop
            loop {
                let current = self.inner.tokens.load(Ordering::Relaxed);
                if current >= bytes {
                    // Attempt to consume
                    match self.inner.tokens.compare_exchange(
                        current,
                        current - bytes,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,    // Successfully acquired
                        Err(_) => continue, // CAS failed, retry inner loop
                    }
                } else {
                    // Not enough tokens — calculate sleep time
                    let deficit = bytes - current;
                    // Sleep proportional to deficit: (deficit / rate) seconds
                    let wait_ms = (deficit as u128 * 1000) / (rate as u128);
                    let wait_ms = wait_ms.max(10) as u64; // At least 10ms for smooth throughput
                    sleep(Duration::from_millis(wait_ms)).await;
                    break; // Break inner CAS loop, re-enter outer loop to refill
                }
            }
        }
    }

    /// Returns the currently configured rate in bytes per second.
    ///
    /// Part of the `SpeedLimiter` interface from the design; used by tests and
    /// available to callers, though the production wiring does not read it back.
    #[allow(dead_code)]
    pub fn current_rate(&self) -> u64 {
        self.inner.rate.load(Ordering::Relaxed)
    }

    /// Refill tokens based on elapsed time since last refill.
    /// Capacity is capped at `rate` (1 second's worth of tokens).
    async fn refill(&self, rate: u64) {
        if rate == 0 {
            return;
        }

        let mut last_refill = self.inner.last_refill.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(*last_refill);
        let elapsed_ms = elapsed.as_millis() as u64;

        if elapsed_ms == 0 {
            return;
        }

        // Calculate new tokens: (rate * elapsed_ms) / 1000
        let new_tokens = (rate as u128 * elapsed_ms as u128 / 1000) as u64;

        if new_tokens > 0 {
            *last_refill = now;

            // Add tokens, capping at capacity (= rate)
            let capacity = rate;
            loop {
                let current = self.inner.tokens.load(Ordering::Relaxed);
                let target = (current + new_tokens).min(capacity);
                if target == current {
                    break;
                }
                match self.inner.tokens.compare_exchange(
                    current,
                    target,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Instant;

    #[tokio::test]
    async fn unlimited_returns_immediately() {
        let limiter = SpeedLimiter::new(0);
        let start = Instant::now();
        limiter.acquire(1_000_000).await;
        // Should be essentially instant
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn set_rate_to_unlimited_returns_immediately() {
        let limiter = SpeedLimiter::new(1024);
        limiter.set_rate(0);
        let start = Instant::now();
        limiter.acquire(1_000_000).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn current_rate_reflects_configured_value() {
        let limiter = SpeedLimiter::new(5_000_000);
        assert_eq!(limiter.current_rate(), 5_000_000);
        limiter.set_rate(1_000_000);
        assert_eq!(limiter.current_rate(), 1_000_000);
        limiter.set_rate(0);
        assert_eq!(limiter.current_rate(), 0);
    }

    #[tokio::test]
    async fn acquire_zero_bytes_returns_immediately() {
        let limiter = SpeedLimiter::new(100);
        let start = Instant::now();
        limiter.acquire(0).await;
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    #[tokio::test]
    async fn burst_allows_immediate_acquire_up_to_capacity() {
        // Rate = 10000 bytes/sec, capacity = 10000 tokens
        let limiter = SpeedLimiter::new(10_000);
        let start = Instant::now();
        // Should succeed immediately since we start with full capacity
        limiter.acquire(10_000).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn throttles_when_tokens_exhausted() {
        // Rate = 10000 bytes/sec → capacity = 10000 tokens
        let limiter = SpeedLimiter::new(10_000);

        // Exhaust the burst capacity
        limiter.acquire(10_000).await;

        // Now acquiring more should block for ~100ms (1000 bytes at 10000 bytes/sec)
        let start = Instant::now();
        limiter.acquire(1_000).await;
        let elapsed = start.elapsed();
        // Should take at least ~80ms (allowing some tolerance)
        assert!(
            elapsed >= Duration::from_millis(80),
            "Expected >= 80ms, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn dynamic_rate_change_takes_effect() {
        let limiter = SpeedLimiter::new(1_000_000); // 1MB/s
        assert_eq!(limiter.current_rate(), 1_000_000);

        limiter.set_rate(500_000); // 500KB/s
        assert_eq!(limiter.current_rate(), 500_000);

        // Acquire should still work after rate change
        limiter.acquire(1000).await;
    }

    #[tokio::test]
    async fn concurrent_acquires_are_safe() {
        let limiter = SpeedLimiter::new(100_000); // 100KB/s

        let mut handles = Vec::new();
        for _ in 0..10 {
            let lim = limiter.clone();
            handles.push(tokio::spawn(async move {
                lim.acquire(1_000).await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
    }

    // ─── Property 7: Speed limiter throughput bound ───────────────────────────────
    //
    // For any configured rate > 0 and any time window, the total bytes passed
    // through the limiter SHALL not exceed the configured rate (per elapsed second)
    // plus one burst capacity (one second's worth of tokens == `rate`).
    //
    // This is the token-bucket invariant: the bucket starts with at most `capacity`
    // tokens and refills at `rate` bytes/sec, capped at `capacity`. Hence over a
    // window of `T` seconds the maximum consumable bytes is `capacity + rate * T`.
    //
    // **Validates: Requirement 4.1**

    /// Deterministic example: acquiring more than the burst capacity must take time
    /// and the observed throughput must respect the token-bucket bound.
    #[tokio::test]
    async fn throughput_stays_within_bound_example() {
        let rate = 100_000u64; // 100 KB/s, capacity == 100_000 tokens
        let limiter = SpeedLimiter::new(rate);

        // Acquire 1.3x the capacity in small chunks so throttling kicks in.
        let total_bytes = rate + rate * 3 / 10; // 130_000
        let chunk = rate / 10; // 10_000

        let start = Instant::now();
        let mut acquired = 0u64;
        while acquired < total_bytes {
            let n = chunk.min(total_bytes - acquired);
            limiter.acquire(n).await;
            acquired += n;
        }
        let elapsed = start.elapsed();

        // Upper bound: rate * elapsed_secs + capacity (with a small rounding slack).
        let allowed = rate as f64 * elapsed.as_secs_f64() + rate as f64 + rate as f64 * 0.02;
        assert!(
            acquired as f64 <= allowed,
            "acquired {acquired} exceeds bound {allowed:.0} (elapsed {:.3}s)",
            elapsed.as_secs_f64()
        );
    }

    proptest! {
        // Timing-sensitive tests sleep on real time, so keep the case count modest.
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// Property 7: over the acquisition window, total bytes consumed never
        /// exceed `rate * elapsed_secs + capacity`, where `capacity == rate`.
        ///
        /// **Validates: Requirement 4.1**
        #[test]
        fn prop_throughput_bounded_by_rate_plus_burst(
            rate in 50_000u64..=500_000,
            // extra over capacity: 10%..=50% of the rate, forcing real throttling.
            extra_tenths in 1u64..=5,
            chunks in 4usize..=16,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();

            rt.block_on(async move {
                let limiter = SpeedLimiter::new(rate);
                let capacity = rate; // one second's worth of tokens

                let total_bytes = rate + rate * extra_tenths / 10;
                let chunk = (total_bytes / chunks as u64).max(1);

                let start = Instant::now();
                let mut acquired = 0u64;
                while acquired < total_bytes {
                    let n = chunk.min(total_bytes - acquired);
                    limiter.acquire(n).await;
                    acquired += n;
                }
                let elapsed_secs = start.elapsed().as_secs_f64();

                // Token-bucket bound: consumed ≤ rate * elapsed_secs + capacity.
                // Integer-ms refill rounds down (never over-grants), so a tiny slack
                // only guards against floating-point edge cases.
                let allowed = rate as f64 * elapsed_secs + capacity as f64 + rate as f64 * 0.02;
                prop_assert!(
                    acquired as f64 <= allowed,
                    "acquired {} bytes exceeds bound {:.0} (rate={}, elapsed={:.3}s)",
                    acquired,
                    allowed,
                    rate,
                    elapsed_secs,
                );

                Ok(())
            })?;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Property 8: Speed limiter fairness (Task 2.3)
//
// This module is intentionally separate from the `tests` module above so that
// it can be added without disturbing the existing throughput/burst tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod fairness_tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// Run `n` concurrent consumers that continuously call `acquire(chunk)` on a
    /// single shared limiter, and return the bytes acquired by each consumer
    /// **during the measurement window only** (after an initial warm-up).
    ///
    /// All consumers share one `SpeedLimiter`, mirroring how download segments
    /// share the global limiter. Each consumer keeps demand saturated (it always
    /// has another chunk to acquire), so the distribution of bytes reflects how
    /// the limiter shares bandwidth across active consumers.
    ///
    /// The `warmup` phase runs the consumers without counting any bytes. This
    /// drains the bucket's one-time burst allowance (a full `rate` tokens present
    /// at construction, per Req 4.1) so that the measurement window observes only
    /// steady-state, refill-driven sharing. Fairness (Req 4.4) is defined over a
    /// sustained window, not over the initial burst transient — whichever consumer
    /// happens to be scheduled first would otherwise grab the entire burst and
    /// skew a short measurement. Counting only post-warm-up bytes makes the test
    /// deterministic without weakening the actual fairness guarantee.
    fn run_consumers(
        rate: u64,
        n: usize,
        chunk: u64,
        warmup: Duration,
        window: Duration,
    ) -> Vec<u64> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        rt.block_on(async move {
            let limiter = SpeedLimiter::new(rate);
            let mut handles = Vec::with_capacity(n);

            for _ in 0..n {
                let lim = limiter.clone();
                handles.push(tokio::spawn(async move {
                    let counter = AtomicU64::new(0);
                    let start = Instant::now();
                    let measure_from = start + warmup;
                    let deadline = measure_from + window;
                    while Instant::now() < deadline {
                        lim.acquire(chunk).await;
                        // Only count bytes acquired within the measurement
                        // window so the one-time burst (consumed during warm-up)
                        // does not distort the steady-state distribution.
                        if Instant::now() >= measure_from {
                            counter.fetch_add(chunk, Ordering::Relaxed);
                        }
                    }
                    counter.load(Ordering::Relaxed)
                }));
            }

            let mut acquired = Vec::with_capacity(n);
            for h in handles {
                acquired.push(h.await.expect("consumer task panicked"));
            }
            acquired
        })
    }

    // ─── Unit test: fixed parameters, fairness + no starvation ───────────────────

    #[test]
    fn fairness_no_starvation_fixed() {
        let rate = 200_000u64; // 200 KB/s
        let n = 4usize;
        let chunk = 1_000u64;
        let acquired = run_consumers(
            rate,
            n,
            chunk,
            Duration::from_millis(400),
            Duration::from_millis(1000),
        );

        let total: u64 = acquired.iter().sum();
        assert!(total > 0, "no bytes were acquired at all");

        let fair = total / n as u64;
        for (i, &got) in acquired.iter().enumerate() {
            // No consumer is starved to zero.
            assert!(
                got > 0,
                "consumer {i} was starved (0 bytes); dist={acquired:?}"
            );
            // Each consumer receives a substantial share of the delivered total.
            assert!(
                got as f64 >= fair as f64 * 0.4,
                "consumer {i} under-served: got {got}, fair {fair}; dist={acquired:?}"
            );
            // No consumer monopolizes far beyond its fair share.
            assert!(
                got as f64 <= fair as f64 * 1.7,
                "consumer {i} over-served: got {got}, fair {fair}; dist={acquired:?}"
            );
        }
    }

    // ─── Property 8: Speed limiter fairness ──────────────────────────────────────
    //
    // For any set of N concurrent consumers sharing one SpeedLimiter, each
    // consumer receives approximately total/N of the delivered bandwidth over a
    // sufficiently long window: no consumer is starved (everyone makes progress)
    // and no consumer monopolizes the limiter.
    //
    // The assertion compares each consumer against the *delivered* mean
    // (total / N) rather than an absolute target, since the property is about
    // how bandwidth is distributed across consumers, independent of timing
    // jitter in the total throughput.
    //
    // Measurement begins only after a warm-up that drains the bucket's one-time
    // burst (Req 4.1). Requirement 4.4 specifies fairness *sustained over a
    // 1-second window*, so the initial burst — which a single consumer can grab
    // before its peers are scheduled — is deliberately excluded. This makes the
    // property deterministic while still exercising the real steady-state
    // sharing behaviour of the shared token bucket.
    //
    // **Validates: Requirement 4.4**

    proptest! {
        #![proptest_config(ProptestConfig {
            // Each case runs real concurrent consumers for ~1.4s, so keep the
            // case count modest to bound total test time.
            cases: 10,
            // Timing/scheduling cannot be meaningfully shrunk; disable it.
            max_shrink_iters: 0,
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_fairness_no_starvation(
            rate in 80_000u64..=300_000u64,
            n in 2usize..=4usize,
        ) {
            // Small chunk relative to per-window tokens so each consumer performs
            // many acquisitions (smooth competition for the shared bucket).
            let chunk = (rate / 200).max(256);
            let acquired = run_consumers(
                rate,
                n,
                chunk,
                Duration::from_millis(400),
                Duration::from_millis(1000),
            );

            let total: u64 = acquired.iter().sum();
            prop_assert!(total > 0, "no bytes acquired; rate={}, n={}", rate, n);

            let fair = total as f64 / n as f64;
            for (i, &got) in acquired.iter().enumerate() {
                // No starvation: every consumer makes real progress.
                prop_assert!(
                    got > 0,
                    "consumer {} starved; dist={:?} (rate={}, n={})",
                    i, acquired, rate, n
                );
                // Lower fairness bound: at least 40% of the fair share.
                prop_assert!(
                    got as f64 >= fair * 0.4,
                    "consumer {} under-served: got {}, fair {:.0}; dist={:?}",
                    i, got, fair, acquired
                );
                // Upper fairness bound: at most 1.7x the fair share.
                prop_assert!(
                    got as f64 <= fair * 1.7,
                    "consumer {} over-served: got {}, fair {:.0}; dist={:?}",
                    i, got, fair, acquired
                );
            }
        }
    }
}
