//! Tests for resilience module

#[cfg(test)]
mod tests {
    use cuenv::errors::Error;
    use cuenv::resilience::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_retry_success() {
        let config = RetryConfig::default();
        let counter = Arc::new(AtomicUsize::new(0));

        let result: Result<&str, Error> = retry(&config, || {
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
    async fn test_circuit_breaker_opens_on_failures() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Fail 3 times
        for _ in 0..3 {
            let _: Result<(), Error> = cb
                .call(|| async { Err(Error::network("test", "fail")) })
                .await;
        }

        assert_eq!(cb.state().await, CircuitState::Open);

        // Next call should fail immediately
        let result: Result<&str, Error> = cb.call(|| async { Ok("should not execute") }).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Circuit breaker is open"));
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
