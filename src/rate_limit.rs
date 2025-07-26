use crate::errors::{Error, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::interval;

/// Configuration for rate limiting
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum number of operations per window
    pub max_operations: usize,
    /// Time window for rate limiting
    pub window_duration: Duration,
    /// Whether to use sliding window (true) or fixed window (false)
    pub sliding_window: bool,
    /// Maximum burst size (for token bucket algorithm)
    pub burst_size: Option<usize>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_operations: 100,
            window_duration: Duration::from_secs(60),
            sliding_window: true,
            burst_size: Some(10),
        }
    }
}

/// Token bucket for rate limiting
struct TokenBucket {
    capacity: usize,
    tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: usize, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self, tokens: usize) -> bool {
        self.refill();

        if self.tokens >= tokens as f64 {
            self.tokens -= tokens as f64;
            true
        } else {
            false
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        self.last_refill = now;
    }

    fn available_tokens(&self) -> usize {
        self.tokens as usize
    }
}

/// Sliding window counter for rate limiting
struct SlidingWindow {
    window_duration: Duration,
    max_count: usize,
    events: Vec<Instant>,
}

impl SlidingWindow {
    fn new(window_duration: Duration, max_count: usize) -> Self {
        Self {
            window_duration,
            max_count,
            events: Vec::new(),
        }
    }

    fn try_record(&mut self) -> bool {
        let now = Instant::now();

        // Remove old events outside the window
        self.events
            .retain(|&event| now.duration_since(event) < self.window_duration);

        if self.events.len() < self.max_count {
            self.events.push(now);
            true
        } else {
            false
        }
    }

    fn current_count(&self) -> usize {
        let now = Instant::now();
        self.events
            .iter()
            .filter(|&&event| now.duration_since(event) < self.window_duration)
            .count()
    }
}

/// Rate limiter implementation
pub struct RateLimiter {
    config: RateLimitConfig,
    token_bucket: Option<Arc<Mutex<TokenBucket>>>,
    sliding_window: Option<Arc<Mutex<SlidingWindow>>>,
    semaphore: Arc<Semaphore>,
}

impl RateLimiter {
    /// Create a new rate limiter
    pub fn new(config: RateLimitConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_operations));

        let token_bucket = if let Some(burst_size) = config.burst_size {
            let refill_rate = config.max_operations as f64 / config.window_duration.as_secs_f64();
            Some(Arc::new(Mutex::new(TokenBucket::new(
                burst_size,
                refill_rate,
            ))))
        } else {
            None
        };

        let sliding_window = if config.sliding_window {
            Some(Arc::new(Mutex::new(SlidingWindow::new(
                config.window_duration,
                config.max_operations,
            ))))
        } else {
            None
        };

        Self {
            config,
            token_bucket,
            sliding_window,
            semaphore,
        }
    }

    /// Try to acquire permission for an operation
    pub async fn try_acquire(&self) -> Result<RateLimitPermit> {
        // First check semaphore for immediate rate limiting
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::configuration("Rate limit exceeded".to_string()))?;

        // Then check token bucket if configured
        if let Some(ref bucket) = self.token_bucket {
            let mut bucket = bucket.lock().await;
            if !bucket.try_consume(1) {
                return Err(Error::configuration(format!(
                    "Rate limit exceeded: token bucket empty (available: {})",
                    bucket.available_tokens()
                )));
            }
        }

        // Check sliding window if configured
        if let Some(ref window) = self.sliding_window {
            let mut window = window.lock().await;
            if !window.try_record() {
                return Err(Error::configuration(format!(
                    "Rate limit exceeded: {} operations in last {:?}",
                    window.current_count(),
                    self.config.window_duration
                )));
            }
        }

        Ok(RateLimitPermit { _permit: permit })
    }

    /// Wait to acquire permission for an operation
    pub async fn acquire(&self) -> Result<RateLimitPermit> {
        // Wait for semaphore
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|e| {
            Error::configuration(format!("Failed to acquire rate limit permit: {}", e))
        })?;

        // Check token bucket with retry
        if let Some(ref bucket) = self.token_bucket {
            loop {
                let mut bucket = bucket.lock().await;
                if bucket.try_consume(1) {
                    break;
                }
                drop(bucket);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // Check sliding window with retry
        if let Some(ref window) = self.sliding_window {
            loop {
                let mut window = window.lock().await;
                if window.try_record() {
                    break;
                }
                drop(window);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        Ok(RateLimitPermit { _permit: permit })
    }

    /// Get current rate limit status
    pub async fn status(&self) -> RateLimitStatus {
        let mut status = RateLimitStatus {
            available_permits: self.semaphore.available_permits(),
            max_permits: self.config.max_operations,
            window_duration: self.config.window_duration,
            token_bucket_tokens: None,
            sliding_window_count: None,
        };

        if let Some(ref bucket) = self.token_bucket {
            let bucket = bucket.lock().await;
            status.token_bucket_tokens = Some(bucket.available_tokens());
        }

        if let Some(ref window) = self.sliding_window {
            let window = window.lock().await;
            status.sliding_window_count = Some(window.current_count());
        }

        status
    }
}

/// Permit for a rate-limited operation
pub struct RateLimitPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Current status of rate limiter
#[derive(Debug, Clone)]
pub struct RateLimitStatus {
    pub available_permits: usize,
    pub max_permits: usize,
    pub window_duration: Duration,
    pub token_bucket_tokens: Option<usize>,
    pub sliding_window_count: Option<usize>,
}

/// Manager for multiple rate limiters
pub struct RateLimitManager {
    limiters: Arc<Mutex<HashMap<String, Arc<RateLimiter>>>>,
}

impl RateLimitManager {
    /// Create a new rate limit manager
    pub fn new() -> Self {
        Self {
            limiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a rate limiter for a resource
    pub async fn register(&self, resource: &str, config: RateLimitConfig) {
        let mut limiters = self.limiters.lock().await;
        limiters.insert(resource.to_string(), Arc::new(RateLimiter::new(config)));
    }

    /// Try to acquire permission for a resource
    pub async fn try_acquire(&self, resource: &str) -> Result<Option<RateLimitPermit>> {
        let limiters = self.limiters.lock().await;

        if let Some(limiter) = limiters.get(resource) {
            limiter.try_acquire().await.map(Some)
        } else {
            Ok(None)
        }
    }

    /// Wait to acquire permission for a resource
    pub async fn acquire(&self, resource: &str) -> Result<Option<RateLimitPermit>> {
        let limiters = self.limiters.lock().await;

        if let Some(limiter) = limiters.get(resource) {
            limiter.acquire().await.map(Some)
        } else {
            Ok(None)
        }
    }

    /// Get status for all rate limiters
    pub async fn status_all(&self) -> HashMap<String, RateLimitStatus> {
        let limiters = self.limiters.lock().await;
        let mut status_map = HashMap::new();

        for (resource, limiter) in limiters.iter() {
            status_map.insert(resource.clone(), limiter.status().await);
        }

        status_map
    }

    /// Start a background task to periodically log rate limit status
    pub fn start_monitoring(self: Arc<Self>, interval_secs: u64) {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(interval_secs));

            loop {
                interval.tick().await;

                let status_all = self.status_all().await;
                for (resource, status) in status_all {
                    log::debug!(
                        "Rate limit status for {}: {}/{} permits, window: {:?}",
                        resource,
                        status.available_permits,
                        status.max_permits,
                        status.window_duration
                    );

                    if let Some(tokens) = status.token_bucket_tokens {
                        log::debug!("  Token bucket: {} tokens available", tokens);
                    }

                    if let Some(count) = status.sliding_window_count {
                        log::debug!("  Sliding window: {} operations", count);
                    }
                }
            }
        });
    }
}

impl Default for RateLimitManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Pre-configured rate limiters for common resources
pub fn default_rate_limiters() -> RateLimitManager {
    let manager = RateLimitManager::new();

    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Hook execution rate limiting
            manager
                .register(
                    "hooks",
                    RateLimitConfig {
                        max_operations: 50,
                        window_duration: Duration::from_secs(60),
                        sliding_window: true,
                        burst_size: Some(10),
                    },
                )
                .await;

            // Secret resolution rate limiting
            manager
                .register(
                    "secrets",
                    RateLimitConfig {
                        max_operations: 100,
                        window_duration: Duration::from_secs(60),
                        sliding_window: true,
                        burst_size: Some(20),
                    },
                )
                .await;

            // Command execution rate limiting
            manager
                .register(
                    "commands",
                    RateLimitConfig {
                        max_operations: 200,
                        window_duration: Duration::from_secs(60),
                        sliding_window: false,
                        burst_size: None,
                    },
                )
                .await;

            // File operations rate limiting
            manager
                .register(
                    "files",
                    RateLimitConfig {
                        max_operations: 500,
                        window_duration: Duration::from_secs(60),
                        sliding_window: true,
                        burst_size: Some(50),
                    },
                )
                .await;
        });
    });

    manager
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_token_bucket() {
        let mut bucket = TokenBucket::new(10, 1.0);

        // Should be able to consume initial tokens
        assert!(bucket.try_consume(5));
        assert_eq!(bucket.available_tokens(), 5);

        // Should not be able to consume more than available
        assert!(!bucket.try_consume(6));

        // Wait for refill
        tokio::time::sleep(Duration::from_secs(2)).await;
        bucket.refill();
        assert!(bucket.available_tokens() >= 6);
    }

    #[tokio::test]
    async fn test_sliding_window() {
        let mut window = SlidingWindow::new(Duration::from_secs(1), 3);

        // Should accept first 3 events
        assert!(window.try_record());
        assert!(window.try_record());
        assert!(window.try_record());

        // Should reject 4th event
        assert!(!window.try_record());
        assert_eq!(window.current_count(), 3);

        // Wait for window to slide
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Should accept new event
        assert!(window.try_record());
    }

    #[tokio::test]
    async fn test_rate_limiter_semaphore() {
        let config = RateLimitConfig {
            max_operations: 2,
            window_duration: Duration::from_secs(1),
            sliding_window: false,
            burst_size: None,
        };

        let limiter = RateLimiter::new(config);

        // Should acquire first 2 permits
        let _permit1 = limiter.try_acquire().await.unwrap();
        let _permit2 = limiter.try_acquire().await.unwrap();

        // Should fail on 3rd
        assert!(limiter.try_acquire().await.is_err());
    }

    #[tokio::test]
    async fn test_rate_limiter_with_token_bucket() {
        let config = RateLimitConfig {
            max_operations: 10,
            window_duration: Duration::from_secs(1),
            sliding_window: false,
            burst_size: Some(5),
        };

        let limiter = RateLimiter::new(config);

        // Should be able to burst up to 5
        for _ in 0..5 {
            let _permit = limiter.try_acquire().await.unwrap();
        }

        // 6th should fail
        assert!(limiter.try_acquire().await.is_err());
    }

    #[tokio::test]
    async fn test_rate_limit_manager() {
        let manager = RateLimitManager::new();

        // Register a limiter
        manager
            .register(
                "test",
                RateLimitConfig {
                    max_operations: 1,
                    window_duration: Duration::from_secs(1),
                    sliding_window: false,
                    burst_size: None,
                },
            )
            .await;

        // Should acquire first permit
        let _permit = manager.try_acquire("test").await.unwrap().unwrap();

        // Should fail second
        assert!(manager.try_acquire("test").await.is_err());

        // Non-existent resource should return None
        assert!(manager.try_acquire("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_rate_limiter_status() {
        let config = RateLimitConfig {
            max_operations: 10,
            window_duration: Duration::from_secs(60),
            sliding_window: true,
            burst_size: Some(5),
        };

        let limiter = RateLimiter::new(config);

        // Get initial status
        let status = limiter.status().await;
        assert_eq!(status.max_permits, 10);
        assert_eq!(status.available_permits, 10);
        assert_eq!(status.token_bucket_tokens, Some(5));
        assert_eq!(status.sliding_window_count, Some(0));

        // Acquire some permits
        let _permit1 = limiter.try_acquire().await.unwrap();
        let _permit2 = limiter.try_acquire().await.unwrap();

        // Check updated status
        let status = limiter.status().await;
        assert_eq!(status.available_permits, 8);
        assert_eq!(status.token_bucket_tokens, Some(3));
        assert_eq!(status.sliding_window_count, Some(2));
    }

    #[tokio::test]
    async fn test_default_rate_limiters() {
        let manager = default_rate_limiters();

        // Should have pre-configured limiters
        let status = manager.status_all().await;
        assert!(status.contains_key("hooks"));
        assert!(status.contains_key("secrets"));
        assert!(status.contains_key("commands"));
        assert!(status.contains_key("files"));
    }
}
