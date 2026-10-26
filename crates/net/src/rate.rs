//! Token-bucket rate limiting for upload and download.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Limits throughput to a number of bytes per second (0 = unlimited), with
/// bursts of up to one second's worth.
#[derive(Debug)]
pub struct RateLimit {
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::new(0)
    }
}

impl RateLimit {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                rate: bytes_per_sec,
                tokens: bytes_per_sec as f64,
                last: Instant::now(),
            }),
        }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        if let Ok(mut b) = self.bucket.lock() {
            b.rate = bytes_per_sec;
            b.tokens = b.tokens.min(bytes_per_sec as f64);
        }
    }

    pub fn rate(&self) -> u64 {
        self.bucket.lock().map_or(0, |b| b.rate)
    }

    /// How long to wait before `n` more bytes may pass; takes them now.
    fn take(&self, n: usize) -> Duration {
        let Ok(mut b) = self.bucket.lock() else {
            return Duration::ZERO;
        };
        if b.rate == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let rate = b.rate as f64;
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(rate);
        b.last = now;
        b.tokens -= n as f64;
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / rate)
        }
    }

    /// Wait until `n` bytes may pass.
    pub async fn acquire(&self, n: usize) {
        let wait = self.take(n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn limits_throughput() {
        let r = RateLimit::new(100_000);
        let start = Instant::now();
        for _ in 0..30 {
            r.acquire(10_000).await; // 300 kB in total
        }
        // One second of burst, then 100 kB/s: about 2 s.
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(1900) && took <= Duration::from_millis(2100),
            "{took:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let r = RateLimit::new(0);
        let start = Instant::now();
        r.acquire(1 << 30).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        r.set_rate(1000);
        assert_eq!(r.rate(), 1000);
    }
}
