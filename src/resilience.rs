//! Resilience patterns for error recovery including retry logic and circuit breakers
//!
//! This module provides reusable utilities for handling transient failures
//! and protecting against cascading failures in external operations.

use crate::errors::{Error, Result};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

/// Default maximum number of retry attempts
const DEFAULT_MAX_RETRIES: usize = 3;

/// Default base delay for exponential backoff (100ms)
const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(100);

/// Default maximum delay for exponential backoff (10s)
const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(10);

/// Default jitter factor (0.1 = 10% randomization)
const DEFAULT_JITTER_FACTOR: f64 = 0.1;

/// Configuration for retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_retries: usize,
    /// Base delay for exponential backoff
    pub base_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Jitter factor for randomization (0.0 to 1.0)
    pub jitter_factor: f64,
    /// Whether to retry on specific error types
    pub retry_on: RetryOn,
}

/// Which errors should trigger a retry
#[derive(Clone)]
pub enum RetryOn {
    /// Retry on all errors
    All,
    /// Retry only on network errors
    Network,
    /// Retry only on filesystem errors
    FileSystem,
    /// Retry on network and filesystem errors
    Transient,
    /// Custom retry predicate
    Custom(Arc<dyn Fn(&Error) -> bool + Send + Sync>),
}

impl std::fmt::Debug for RetryOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryOn::All => write!(f, "RetryOn::All"),
            RetryOn::Network => write!(f, "RetryOn::Network"),
            RetryOn::FileSystem => write!(f, "RetryOn::FileSystem"),
            RetryOn::Transient => write!(f, "RetryOn::Transient"),
            RetryOn::Custom(_) => write!(f, "RetryOn::Custom(<predicate>)"),
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            jitter_factor: DEFAULT_JITTER_FACTOR,
            retry_on: RetryOn::Transient,
        }
    }
}

impl RetryConfig {
    /// Create a retry config for network operations
    pub fn for_network() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(30),
            jitter_factor: 0.2,
            retry_on: RetryOn::Network,
        }
    }

    /// Create a retry config for filesystem operations
    pub fn for_filesystem() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(2),
            jitter_factor: 0.1,
            retry_on: RetryOn::FileSystem,
        }
    }

    /// Check if an error should be retried
    fn should_retry(&self, error: &Error) -> bool {
        match &self.retry_on {
            RetryOn::All => true,
            RetryOn::Network => matches!(error, Error::Network { .. }),
            RetryOn::FileSystem => matches!(error, Error::FileSystem { .. }),
            RetryOn::Transient => {
                matches!(error, Error::Network { .. } | Error::FileSystem { .. })
            }
            RetryOn::Custom(predicate) => predicate(error),
        }
    }

    /// Calculate delay for a given attempt with exponential backoff and jitter
    fn calculate_delay(&self, attempt: usize) -> Duration {
        let exponential_delay = self.base_delay * 2u32.pow(attempt as u32);
        let capped_delay = exponential_delay.min(self.max_delay);

        // Add jitter to prevent thundering herd
        if self.jitter_factor > 0.0 {
            use std::time::SystemTime;
            let jitter_range = capped_delay.as_millis() as f64 * self.jitter_factor;
            // Simple pseudo-random jitter based on system time
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let seed = now.as_nanos() as f64;
            let normalized = ((seed % 1000.0) / 1000.0 - 0.5) * 2.0;
            let jitter = normalized * jitter_range;
            let final_millis = (capped_delay.as_millis() as f64 + jitter).max(0.0) as u64;
            Duration::from_millis(final_millis)
        } else {
            capped_delay
        }
    }
}

/// Execute an operation with retry logic
pub async fn retry<F, Fut, T>(config: &RetryConfig, operation: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match operation().await {
            Ok(result) => {
                if attempt > 0 {
                    log::info!("Operation succeeded after {} retries", attempt);
                }
                return Ok(result);
            }
            Err(error) => {
                if attempt < config.max_retries && config.should_retry(&error) {
                    let delay = config.calculate_delay(attempt);
                    log::warn!(
                        "Operation failed (attempt {}/{}), retrying in {:?}: {}",
                        attempt + 1,
                        config.max_retries + 1,
                        delay,
                        error
                    );
                    sleep(delay).await;
                    last_error = Some(error);
                } else {
                    // Don't retry - either max retries reached or error is not retryable
                    return Err(error);
                }
            }
        }
    }

    // This should be unreachable, but just in case
    Err(last_error.unwrap_or_else(|| Error::configuration("Retry loop ended unexpectedly")))
}

/// Circuit breaker states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed - requests pass through normally
    Closed,
    /// Circuit is open - requests fail immediately
    Open,
    /// Circuit is half-open - limited requests allowed to test recovery
    HalfOpen,
}

/// Configuration for circuit breaker behavior
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of failures before opening the circuit
    pub failure_threshold: usize,
    /// Success threshold to close the circuit from half-open state
    pub success_threshold: usize,
    /// Time window for counting failures
    pub timeout: Duration,
    /// Duration to wait before attempting half-open state
    pub break_duration: Duration,
    /// Maximum number of requests in half-open state
    pub half_open_max_calls: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            timeout: Duration::from_secs(60),
            break_duration: Duration::from_secs(30),
            half_open_max_calls: 3,
        }
    }
}

/// Circuit breaker implementation
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: Arc<RwLock<CircuitState>>,
    failure_count: Arc<AtomicUsize>,
    success_count: Arc<AtomicUsize>,
    half_open_calls: Arc<AtomicUsize>,
    last_failure_time: Arc<Mutex<Option<Instant>>>,
    last_state_change: Arc<Mutex<Instant>>,
    generation: Arc<AtomicU64>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(CircuitState::Closed)),
            failure_count: Arc::new(AtomicUsize::new(0)),
            success_count: Arc::new(AtomicUsize::new(0)),
            half_open_calls: Arc::new(AtomicUsize::new(0)),
            last_failure_time: Arc::new(Mutex::new(None)),
            last_state_change: Arc::new(Mutex::new(Instant::now())),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the current state of the circuit
    pub async fn state(&self) -> CircuitState {
        let state = *self.state.read().await;

        // Check if we should transition from Open to HalfOpen
        if state == CircuitState::Open {
            let last_change = *self.last_state_change.lock().await;
            if last_change.elapsed() >= self.config.break_duration {
                self.transition_to_half_open().await;
                return CircuitState::HalfOpen;
            }
        }

        state
    }

    /// Execute an operation through the circuit breaker
    pub async fn call<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let current_state = self.state().await;

        match current_state {
            CircuitState::Open => Err(Error::configuration(
                "Circuit breaker is open - service unavailable",
            )),
            CircuitState::HalfOpen => {
                let calls = self.half_open_calls.fetch_add(1, Ordering::SeqCst);
                if calls >= self.config.half_open_max_calls {
                    return Err(Error::configuration(
                        "Circuit breaker half-open limit reached",
                    ));
                }
                self.execute_with_recording(operation).await
            }
            CircuitState::Closed => self.execute_with_recording(operation).await,
        }
    }

    /// Execute operation and record the result
    async fn execute_with_recording<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let generation = self.generation.load(Ordering::SeqCst);
        let result = operation().await;

        match result {
            Ok(_) => {
                self.record_success(generation).await;
            }
            Err(_) => {
                self.record_failure(generation).await;
            }
        }

        result
    }

    /// Record a successful call
    async fn record_success(&self, generation: u64) {
        // Only record if we're still in the same generation
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }

        let state = *self.state.read().await;

        match state {
            CircuitState::HalfOpen => {
                let count = self.success_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.config.success_threshold {
                    self.transition_to_closed().await;
                }
            }
            CircuitState::Closed => {
                // Reset failure count on success in closed state
                self.failure_count.store(0, Ordering::SeqCst);
                *self.last_failure_time.lock().await = None;
            }
            CircuitState::Open => {} // Shouldn't happen
        }
    }

    /// Record a failed call
    async fn record_failure(&self, generation: u64) {
        // Only record if we're still in the same generation
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }

        let now = Instant::now();
        let mut last_failure = self.last_failure_time.lock().await;

        // Reset failure count if outside the timeout window
        if let Some(last) = *last_failure {
            if now.duration_since(last) > self.config.timeout {
                self.failure_count.store(0, Ordering::SeqCst);
            }
        }

        *last_failure = Some(now);
        drop(last_failure);

        let state = *self.state.read().await;

        match state {
            CircuitState::Closed => {
                let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.config.failure_threshold {
                    self.transition_to_open().await;
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open state reopens the circuit
                self.transition_to_open().await;
            }
            CircuitState::Open => {} // Already open
        }
    }

    /// Transition to open state
    async fn transition_to_open(&self) {
        let mut state = self.state.write().await;
        if *state != CircuitState::Open {
            log::warn!("Circuit breaker opening");
            *state = CircuitState::Open;
            *self.last_state_change.lock().await = Instant::now();
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.reset_counters();
        }
    }

    /// Transition to half-open state
    async fn transition_to_half_open(&self) {
        let mut state = self.state.write().await;
        if *state != CircuitState::HalfOpen {
            log::info!("Circuit breaker entering half-open state");
            *state = CircuitState::HalfOpen;
            *self.last_state_change.lock().await = Instant::now();
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.reset_counters();
        }
    }

    /// Transition to closed state
    async fn transition_to_closed(&self) {
        let mut state = self.state.write().await;
        if *state != CircuitState::Closed {
            log::info!("Circuit breaker closing");
            *state = CircuitState::Closed;
            *self.last_state_change.lock().await = Instant::now();
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.reset_counters();
        }
    }

    /// Reset internal counters
    fn reset_counters(&self) {
        self.failure_count.store(0, Ordering::SeqCst);
        self.success_count.store(0, Ordering::SeqCst);
        self.half_open_calls.store(0, Ordering::SeqCst);
    }

    /// Get current circuit breaker statistics
    pub async fn stats(&self) -> CircuitBreakerStats {
        CircuitBreakerStats {
            state: *self.state.read().await,
            failure_count: self.failure_count.load(Ordering::SeqCst),
            success_count: self.success_count.load(Ordering::SeqCst),
            half_open_calls: self.half_open_calls.load(Ordering::SeqCst),
            last_failure_time: *self.last_failure_time.lock().await,
            last_state_change: *self.last_state_change.lock().await,
        }
    }
}

/// Statistics about circuit breaker state
#[derive(Debug)]
pub struct CircuitBreakerStats {
    pub state: CircuitState,
    pub failure_count: usize,
    pub success_count: usize,
    pub half_open_calls: usize,
    pub last_failure_time: Option<Instant>,
    pub last_state_change: Instant,
}

/// Retry with circuit breaker protection
pub async fn retry_with_circuit_breaker<F, Fut, T>(
    retry_config: &RetryConfig,
    circuit_breaker: &CircuitBreaker,
    operation: F,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    retry(retry_config, || circuit_breaker.call(|| operation())).await
}

/// Helper to suggest recovery actions based on error type
pub fn suggest_recovery(error: &Error) -> String {
    match error {
        Error::Network { .. } => "Network error: Check your internet connection and try again. \
             If the problem persists, the service may be temporarily unavailable."
            .to_string(),
        Error::FileSystem { .. } => "File system error: Check file permissions and disk space. \
             Ensure the path exists and is accessible."
            .to_string(),
        Error::CommandExecution { .. } => {
            "Command execution failed: Ensure the command is installed and in your PATH. \
             Check the command syntax and arguments."
                .to_string()
        }
        Error::Configuration { message } => {
            format!(
                "Configuration error: {}. Check your env.cue file for syntax errors \
                 or invalid configurations.",
                message
            )
        }
        Error::CueParse { message, .. } => {
            format!(
                "CUE parsing error: {}. Verify your CUE syntax using 'cue vet'.",
                message
            )
        }
        Error::Environment { message, .. } => {
            format!(
                "Environment error: {}. Try unloading and reloading the environment.",
                message
            )
        }
        Error::Timeout { .. } => "Operation timed out: The operation took too long to complete. \
             Try again or increase the timeout if possible."
            .to_string(),
        Error::SecretResolution { message, .. } => {
            format!(
                "Secret resolution failed: {}. Check your secret resolver configuration \
                 and ensure authentication is set up correctly.",
                message
            )
        }
        Error::ShellExpansion { message, .. } => {
            format!(
                "Shell expansion failed: {}. Check the syntax of your shell expressions.",
                message
            )
        }
        Error::Json { message, .. } => {
            format!(
                "JSON processing error: {}. Ensure the data is valid JSON format.",
                message
            )
        }
        Error::Ffi { message, .. } => {
            format!(
                "FFI operation failed: {}. This is likely an internal error.",
                message
            )
        }
        Error::Unsupported { feature, message } => {
            format!(
                "Unsupported feature '{}': {}. This feature may not be available \
                 on your platform or configuration.",
                feature, message
            )
        }
        Error::PermissionDenied { .. } => {
            "Permission denied: Check file permissions and ensure you have the necessary \
             access rights. You may need to run with elevated privileges."
                .to_string()
        }
        _ => "An error occurred. Please check the logs for more details.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn test_retry_success() {
        let config = RetryConfig::default();
        let counter = Arc::new(AtomicUsize::new(0));

        let result = retry(&config, || {
            let count = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if count < 2 {
                    Err(Error::network("test", "simulated failure"))
                } else {
                    Ok("success")
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_max_attempts() {
        let config = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };

        let counter = Arc::new(AtomicUsize::new(0));

        let result: Result<()> = retry(&config, || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::network("test", "always fails")) }
        })
        .await;

        assert!(result.is_err());
        // Initial attempt + 2 retries = 3 total
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_non_retryable_error() {
        let config = RetryConfig {
            retry_on: RetryOn::Network,
            ..Default::default()
        };

        let counter = Arc::new(AtomicUsize::new(0));

        let result: Result<()> = retry(&config, || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::configuration("not retryable")) }
        })
        .await;

        assert!(result.is_err());
        // Should only try once
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_on_failures() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Fail 3 times
        for _ in 0..3 {
            let _: Result<()> = cb
                .call(|| async { Err(Error::network("test", "fail")) })
                .await;
        }

        assert_eq!(cb.state().await, CircuitState::Open);

        // Next call should fail immediately
        let result = cb.call(|| async { Ok("should not execute") }).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Circuit breaker is open"));
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_recovery() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            success_threshold: 2,
            break_duration: Duration::from_millis(100),
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        for _ in 0..2 {
            let _: Result<()> = cb
                .call(|| async { Err(Error::network("test", "fail")) })
                .await;
        }
        assert_eq!(cb.state().await, CircuitState::Open);

        // Wait for break duration
        sleep(Duration::from_millis(150)).await;

        // Should be half-open now
        assert_eq!(cb.state().await, CircuitState::HalfOpen);

        // Two successful calls should close it
        for _ in 0..2 {
            let _ = cb.call(|| async { Ok("success") }).await;
        }

        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_failure() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            break_duration: Duration::from_millis(100),
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        for _ in 0..2 {
            let _: Result<()> = cb
                .call(|| async { Err(Error::network("test", "fail")) })
                .await;
        }

        // Wait for break duration
        sleep(Duration::from_millis(150)).await;

        // Should be half-open
        assert_eq!(cb.state().await, CircuitState::HalfOpen);

        // Failure in half-open should reopen
        let _: Result<()> = cb
            .call(|| async { Err(Error::network("test", "fail")) })
            .await;

        assert_eq!(cb.state().await, CircuitState::Open);
    }

    #[tokio::test]
    async fn test_retry_with_circuit_breaker() {
        let retry_config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_millis(10),
            ..Default::default()
        };

        let cb_config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(cb_config);

        let counter = Arc::new(AtomicUsize::new(0));
        let should_succeed = Arc::new(AtomicBool::new(false));

        // First, cause circuit to open
        for _ in 0..3 {
            let _: Result<()> = retry_with_circuit_breaker(&retry_config, &cb, || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err(Error::network("test", "fail")) }
            })
            .await;
        }

        assert_eq!(cb.state().await, CircuitState::Open);

        // Now calls should fail immediately without retrying
        should_succeed.store(true, Ordering::SeqCst);
        let before_count = counter.load(Ordering::SeqCst);

        let result = retry_with_circuit_breaker(&retry_config, &cb, || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Ok("would succeed") }
        })
        .await;

        assert!(result.is_err());
        // Should not have incremented counter (circuit open)
        assert_eq!(counter.load(Ordering::SeqCst), before_count);
    }

    #[test]
    fn test_calculate_delay_with_jitter() {
        let config = RetryConfig {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            jitter_factor: 0.5,
            ..Default::default()
        };

        // Test multiple times to ensure jitter is working
        let mut delays = Vec::new();
        for _ in 0..10 {
            delays.push(config.calculate_delay(2));
        }

        // All delays should be different due to jitter
        let unique_delays: std::collections::HashSet<_> = delays.iter().collect();
        assert!(unique_delays.len() > 1);

        // All should be within expected range (400ms ± 50%)
        for delay in delays {
            assert!(delay >= Duration::from_millis(200));
            assert!(delay <= Duration::from_millis(600));
        }
    }

    #[test]
    fn test_suggest_recovery() {
        let network_err = Error::network("api.example.com", "connection refused");
        assert!(suggest_recovery(&network_err).contains("internet connection"));

        let fs_err = Error::file_system(
            std::path::PathBuf::from("/tmp/test"),
            "read",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        assert!(suggest_recovery(&fs_err).contains("permissions"));

        let config_err = Error::configuration("invalid syntax");
        assert!(suggest_recovery(&config_err).contains("env.cue"));
    }
}
