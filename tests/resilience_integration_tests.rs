//! Integration tests for resilience patterns

#[cfg(test)]
mod tests {
    use cuenv::command_executor::CommandExecutorFactory;
    use cuenv::errors::Error;
    use cuenv::resilience::*;
    use cuenv::secrets::{CommandResolver, SecretResolver};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    #[tokio::test]
    async fn test_secret_resolver_with_transient_failures() {
        let test_executor = CommandExecutorFactory::test();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Configure responses that fail initially then succeed
        test_executor.add_response(
            "secret-cmd",
            vec!["get".to_string(), "key".to_string()],
            Box::new(move |_cmd, _args| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
                if count < 2 {
                    // Fail first two attempts
                    Err(Error::network("secret-service", "connection timeout"))
                } else {
                    // Succeed on third attempt
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: b"secret-value".to_vec(),
                        stderr: Vec::new(),
                    })
                }
            }),
        );

        let resolver = CommandResolver::with_executor(10, Box::new(test_executor));

        // Test that retry logic works
        let reference = r#"cuenv-resolver://{"cmd":"secret-cmd","args":["get","key"]}"#;
        let result = resolver.resolve(reference).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some("secret-value".to_string()));
        assert_eq!(call_count.load(Ordering::SeqCst), 3); // Initial + 2 retries
    }

    #[tokio::test]
    async fn test_circuit_breaker_prevents_cascading_failures() {
        let test_executor = CommandExecutorFactory::test();

        // Always fail
        test_executor.add_response(
            "failing-cmd",
            vec![],
            Box::new(|_cmd, _args| Err(Error::network("service", "always fails"))),
        );

        let resolver = CommandResolver::with_executor(10, Box::new(test_executor));

        // Cause circuit to open
        for i in 0..10 {
            let reference = format!(r#"cuenv-resolver://{{"cmd":"failing-cmd","args":[]}}"#);
            let _ = resolver.resolve(&reference).await;
        }

        // Now the circuit should be open, subsequent calls should fail fast
        let start = std::time::Instant::now();
        let reference = r#"cuenv-resolver://{"cmd":"failing-cmd","args":[]}"#;
        let result = resolver.resolve(reference).await;
        let duration = start.elapsed();

        assert!(result.is_err());
        // Should fail immediately due to open circuit
        assert!(duration < Duration::from_millis(100));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Circuit breaker is open"));
    }

    #[tokio::test]
    async fn test_retry_config_respects_error_types() {
        let config = RetryConfig {
            retry_on: RetryOn::Network,
            max_retries: 3,
            ..Default::default()
        };

        let network_attempts = Arc::new(AtomicUsize::new(0));
        let fs_attempts = Arc::new(AtomicUsize::new(0));

        // Test network error - should retry
        let network_counter = network_attempts.clone();
        let result = retry(&config, || {
            network_counter.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::network("test", "network failure")) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(network_attempts.load(Ordering::SeqCst), 4); // 1 initial + 3 retries

        // Test filesystem error - should not retry
        let fs_counter = fs_attempts.clone();
        let result = retry(&config, || {
            fs_counter.fetch_add(1, Ordering::SeqCst);
            async {
                Err(Error::file_system(
                    std::path::PathBuf::from("/tmp/test"),
                    "write",
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                ))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(fs_attempts.load(Ordering::SeqCst), 1); // No retries
    }

    #[tokio::test]
    async fn test_exponential_backoff_timing() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            jitter_factor: 0.0, // No jitter for predictable timing
            ..Default::default()
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let timestamps = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let attempts_clone = attempts.clone();
        let timestamps_clone = timestamps.clone();

        let start = std::time::Instant::now();
        let _ = retry(&config, || {
            let attempts = attempts_clone.clone();
            let timestamps = timestamps_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                timestamps.lock().await.push(std::time::Instant::now());
                Err(Error::network("test", "fail"))
            }
        })
        .await;

        let timestamps = timestamps.lock().await;
        assert_eq!(timestamps.len(), 4); // Initial + 3 retries

        // Check delays between attempts (approximately)
        for i in 1..timestamps.len() {
            let delay = timestamps[i].duration_since(timestamps[i - 1]);
            let expected = Duration::from_millis(100) * 2u32.pow((i - 1) as u32);
            let expected = expected.min(Duration::from_secs(1));

            // Allow some margin for execution time
            assert!(delay >= expected - Duration::from_millis(50));
            assert!(delay <= expected + Duration::from_millis(50));
        }
    }

    #[tokio::test]
    async fn test_recovery_suggestions() {
        let network_err = Error::network("api.example.com", "connection refused");
        let suggestion = suggest_recovery(&network_err);
        assert!(suggestion.contains("internet connection"));
        assert!(suggestion.contains("service may be temporarily unavailable"));

        let fs_err = Error::file_system(
            std::path::PathBuf::from("/etc/secure"),
            "read",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let suggestion = suggest_recovery(&fs_err);
        assert!(suggestion.contains("permissions"));
        assert!(suggestion.contains("disk space"));

        let config_err = Error::configuration("invalid CUE syntax");
        let suggestion = suggest_recovery(&config_err);
        assert!(suggestion.contains("env.cue"));
        assert!(suggestion.contains("syntax errors"));

        let timeout_err = Error::timeout("hook execution", Duration::from_secs(30));
        let suggestion = suggest_recovery(&timeout_err);
        assert!(suggestion.contains("timed out"));
        assert!(suggestion.contains("increase the timeout"));
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

        let attempt = Arc::new(AtomicUsize::new(0));

        // Open the circuit with failures
        for _ in 0..2 {
            let attempt_clone = attempt.clone();
            let _ = cb
                .call(|| async move {
                    attempt_clone.fetch_add(1, Ordering::SeqCst);
                    Err(Error::network("test", "fail"))
                })
                .await;
        }

        assert_eq!(cb.state().await, CircuitState::Open);

        // Wait for break duration
        sleep(Duration::from_millis(150)).await;

        // Circuit should transition to half-open on next call attempt
        let attempt_clone = attempt.clone();
        let result = cb
            .call(|| async move {
                let count = attempt_clone.fetch_add(1, Ordering::SeqCst);
                if count < 4 {
                    // Continue failing
                    Err(Error::network("test", "fail"))
                } else {
                    // Start succeeding
                    Ok("success")
                }
            })
            .await;

        // First half-open attempt fails, circuit reopens
        assert!(result.is_err());
        assert_eq!(cb.state().await, CircuitState::Open);

        // Wait again
        sleep(Duration::from_millis(150)).await;

        // Now succeed in half-open state
        for i in 0..2 {
            let attempt_clone = attempt.clone();
            let result = cb
                .call(|| async move {
                    attempt_clone.fetch_add(1, Ordering::SeqCst);
                    Ok(format!("success-{}", i))
                })
                .await;
            assert!(result.is_ok());
        }

        // Circuit should be closed after 2 successes
        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_statistics() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());

        // Initial stats
        let stats = cb.stats().await;
        assert_eq!(stats.state, CircuitState::Closed);
        assert_eq!(stats.failure_count, 0);
        assert_eq!(stats.success_count, 0);

        // Add some successes
        for _ in 0..3 {
            let _ = cb.call(|| async { Ok("success") }).await;
        }

        let stats = cb.stats().await;
        assert_eq!(stats.state, CircuitState::Closed);
        assert_eq!(stats.failure_count, 0);

        // Add failures to open circuit
        for _ in 0..5 {
            let _ = cb
                .call(|| async { Err(Error::network("test", "fail")) })
                .await;
        }

        let stats = cb.stats().await;
        assert_eq!(stats.state, CircuitState::Open);
        assert!(stats.failure_count >= 5);
        assert!(stats.last_failure_time.is_some());
    }

    #[tokio::test]
    async fn test_retry_with_custom_predicate() {
        let attempts = Arc::new(AtomicUsize::new(0));

        // Custom predicate that only retries on specific error messages
        let retry_predicate =
            Arc::new(|error: &Error| -> bool { error.to_string().contains("retry-me") });

        let config = RetryConfig {
            retry_on: RetryOn::Custom(retry_predicate),
            max_retries: 3,
            ..Default::default()
        };

        // Test error that should be retried
        let attempts_clone = attempts.clone();
        let result = retry(&config, || {
            attempts_clone.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::configuration("retry-me please")) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 4); // Initial + 3 retries

        // Reset counter
        attempts.store(0, Ordering::SeqCst);

        // Test error that should NOT be retried
        let attempts_clone = attempts.clone();
        let result = retry(&config, || {
            attempts_clone.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::configuration("do not retry")) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1); // No retries
    }
}
