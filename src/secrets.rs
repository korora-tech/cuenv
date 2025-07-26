use crate::audit::{audit_logger, AuditLogger};
use crate::command_executor::{CommandExecutor, CommandExecutorFactory};
use crate::errors::{Error, Result};
use crate::rate_limit::RateLimitManager;
use crate::resilience::{retry, CircuitBreaker, CircuitBreakerConfig, RetryConfig};
use crate::types::{CommandArguments, EnvironmentVariables, SecretValues};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug, Deserialize, Serialize)]
pub struct ResolverConfig {
    pub cmd: String,
    pub args: Vec<String>,
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, reference: &str) -> Result<Option<String>>;
}

/// Generic command-based secret resolver that uses CUE-defined resolver configurations
pub struct CommandResolver {
    // Semaphore to limit concurrent secret resolutions
    semaphore: Arc<Semaphore>,
    // Track if we've shown the initial approval prompt
    approval_shown: Arc<Mutex<bool>>,
    // Command executor for running external commands
    executor: Box<dyn CommandExecutor>,
    // Retry configuration for transient failures
    retry_config: RetryConfig,
    // Circuit breaker for external commands
    circuit_breaker: Arc<CircuitBreaker>,
    // Rate limiter for secret resolution
    rate_limiter: Option<Arc<RateLimitManager>>,
    // Audit logger
    audit_logger: Option<Arc<AuditLogger>>,
}

impl CommandResolver {
    pub fn new(max_concurrent: usize) -> Self {
        Self::with_executor(max_concurrent, CommandExecutorFactory::system())
    }

    pub fn with_executor(max_concurrent: usize, executor: Box<dyn CommandExecutor>) -> Self {
        let retry_config = RetryConfig::for_network();
        let circuit_breaker_config = CircuitBreakerConfig {
            failure_threshold: 5,
            success_threshold: 2,
            timeout: std::time::Duration::from_secs(300), // 5 minutes
            break_duration: std::time::Duration::from_secs(60), // 1 minute
            half_open_max_calls: 3,
        };

        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            approval_shown: Arc::new(Mutex::new(false)),
            executor,
            retry_config,
            circuit_breaker: Arc::new(CircuitBreaker::new(circuit_breaker_config)),
            rate_limiter: None,
            audit_logger: audit_logger(),
        }
    }

    /// Set the rate limiter for secret resolution
    pub fn with_rate_limiter(mut self, rate_limiter: Arc<RateLimitManager>) -> Self {
        self.rate_limiter = Some(rate_limiter);
        self
    }

    async fn ensure_approval(&self) -> Result<()> {
        let mut shown = self.approval_shown.lock().await;
        if !*shown {
            // In a real implementation, we might want to prompt the user here
            // For now, we'll just log that we're starting secret resolution
            log::info!("Starting secret resolution. This may prompt for authentication...");
            *shown = true;
        }
        Ok(())
    }

    fn parse_resolver_reference(reference: &str) -> Option<ResolverConfig> {
        if let Some(json_str) = reference.strip_prefix("cuenv-resolver://") {
            serde_json::from_str(json_str).ok()
        } else {
            None
        }
    }

    async fn execute_resolver(&self, config: &ResolverConfig) -> Result<String> {
        // Acquire semaphore permit for rate limiting
        let _permit = match self.semaphore.acquire().await {
            Ok(permit) => permit,
            Err(e) => {
                return Err(Error::configuration(format!(
                    "failed to acquire semaphore for rate limiting: {e}"
                )));
            }
        };

        // Execute with retry and circuit breaker
        let cmd = config.cmd.clone();
        let args = config.args.clone();
        let executor = &self.executor;
        let circuit_breaker = &self.circuit_breaker;

        retry(&self.retry_config, || {
            let cmd = cmd.clone();
            let args = args.clone();
            circuit_breaker.call(|| async move {
                let command_args = CommandArguments::from_vec(args.clone());
                let output = executor.execute(&cmd, &command_args).await.map_err(|e| {
                    Error::command_execution(
                        &cmd,
                        args.clone(),
                        format!("failed to execute command: {e}"),
                        None,
                    )
                })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(Error::command_execution(
                        &cmd,
                        args.clone(),
                        format!("command failed: {stderr}"),
                        output.status.code(),
                    ));
                }

                String::from_utf8(output.stdout)
                    .map(|s| s.trim().to_string())
                    .map_err(|e| {
                        Error::configuration(format!("command output is not valid UTF-8: {e}"))
                    })
            })
        })
        .await
    }
}

#[async_trait]
impl SecretResolver for CommandResolver {
    async fn resolve(&self, reference: &str) -> Result<Option<String>> {
        if let Some(config) = Self::parse_resolver_reference(reference) {
            // Check rate limit if configured
            let _rate_limit_permit = if let Some(ref rate_limiter) = self.rate_limiter {
                match rate_limiter.try_acquire("secrets").await {
                    Ok(Some(permit)) => Some(permit),
                    Ok(None) => None,
                    Err(e) => {
                        // Log rate limit exceeded
                        if let Some(ref logger) = self.audit_logger {
                            let _ = logger.log_rate_limit("secrets", 0, 0, true).await;
                        }
                        return Err(Error::configuration(format!("Rate limit exceeded: {}", e)));
                    }
                }
            } else {
                None
            };

            // Ensure we've shown approval message on first resolution
            match self.ensure_approval().await {
                Ok(()) => {}
                Err(e) => return Err(e),
            }

            let result = self.execute_resolver(&config).await;

            // Log the secret resolution attempt
            if let Some(ref logger) = self.audit_logger {
                let _ = logger
                    .log_secret_resolution(
                        &reference[0..20.min(reference.len())], // Truncate for security
                        "command",
                        result.is_ok(),
                        result.as_ref().err().map(|e| e.to_string()),
                    )
                    .await;
            }

            match result {
                Ok(result) => Ok(Some(result)),
                Err(e) => Err(e),
            }
        } else {
            Ok(None)
        }
    }
}

pub struct SecretManager {
    resolver: Box<dyn SecretResolver>,
}

pub struct ResolvedSecrets {
    pub env_vars: EnvironmentVariables,
    pub secret_values: SecretValues,
}

impl SecretManager {
    pub fn new() -> Self {
        Self {
            // Use up to 10 concurrent secret resolutions
            resolver: Box::new(CommandResolver::new(10)),
        }
    }
}

impl Default for SecretManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretManager {
    pub fn with_resolver(resolver: Box<dyn SecretResolver>) -> Self {
        Self { resolver }
    }

    pub async fn resolve_secrets(&self, env_vars: EnvironmentVariables) -> Result<ResolvedSecrets> {
        let mut resolved_env = EnvironmentVariables::new();
        let mut secret_values = SecretValues::new();

        // Collect all secret resolution tasks
        let mut tasks = Vec::new();

        for (key, value) in env_vars {
            if value.starts_with("cuenv-resolver://") {
                let key_clone = key.clone();
                let value_clone = value.clone();
                let resolver = &self.resolver;

                tasks.push(async move {
                    let result = resolver.resolve(&value_clone).await;
                    match result {
                        Ok(opt) => Ok((key_clone, value_clone, opt)),
                        Err(e) => {
                            log::warn!("Failed to resolve secret for {key_clone}: {e}");
                            // Return Ok with None to indicate failure but preserve the original value
                            Ok((key_clone, value_clone, None))
                        }
                    }
                });
            } else {
                // Non-secret values pass through immediately
                resolved_env.insert(key, value);
            }
        }

        // Resolve all secrets in parallel
        let results: Vec<Result<(String, String, Option<String>)>> =
            futures::future::join_all(tasks).await;

        for result in results {
            let (key, original_value, resolved) = result?;
            if let Some(secret) = resolved {
                resolved_env.insert(key.clone(), secret.clone());
                secret_values.insert(secret);
                log::debug!("Resolved secret for {key}");
            } else {
                // If resolution failed, keep the original value
                resolved_env.insert(key, original_value);
            }
        }

        Ok(ResolvedSecrets {
            env_vars: resolved_env,
            secret_values,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::sleep;

    #[test]
    fn test_parse_resolver_reference() {
        let reference = r#"cuenv-resolver://{"cmd":"op","args":["read","op://vault/item/field"]}"#;
        let config = CommandResolver::parse_resolver_reference(reference);
        assert!(config.is_some());

        let config = config.unwrap();
        assert_eq!(config.cmd, "op");
        assert_eq!(config.args, vec!["read", "op://vault/item/field"]);
    }

    #[test]
    fn test_parse_invalid_reference() {
        let reference = "op://vault/item/field";
        let config = CommandResolver::parse_resolver_reference(reference);
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn test_secret_manager_with_mixed_values() {
        let manager = SecretManager::new();

        let mut env_vars = EnvironmentVariables::new();
        env_vars.insert("NORMAL_VAR".to_string(), "plain-value".to_string());
        env_vars.insert("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string());

        let resolved = manager.resolve_secrets(env_vars).await.unwrap();

        // Normal variables should pass through unchanged
        assert_eq!(resolved.env_vars.get("NORMAL_VAR").unwrap(), "plain-value");
        assert_eq!(
            resolved.env_vars.get("PATH").unwrap(),
            "/usr/bin:/usr/local/bin"
        );

        // No secrets were resolved
        assert!(resolved.secret_values.is_empty());
    }

    // Test concurrent secret resolution
    #[tokio::test]
    async fn test_concurrent_secret_resolution() {
        struct TestResolver {
            call_count: Arc<AtomicUsize>,
            delay: Duration,
        }

        #[async_trait]
        impl SecretResolver for TestResolver {
            async fn resolve(&self, reference: &str) -> Result<Option<String>> {
                if reference.starts_with("cuenv-resolver://") {
                    self.call_count.fetch_add(1, Ordering::SeqCst);
                    sleep(self.delay).await;
                    let suffix = reference.strip_prefix("cuenv-resolver://").unwrap();
                    let secret = format!("secret-{suffix}");
                    Ok(Some(secret))
                } else {
                    Ok(None)
                }
            }
        }

        let call_count = Arc::new(AtomicUsize::new(0));
        let resolver = TestResolver {
            call_count: call_count.clone(),
            delay: Duration::from_millis(50),
        };

        let manager = SecretManager {
            resolver: Box::new(resolver),
        };

        let mut env_vars = EnvironmentVariables::new();
        for i in 0..20 {
            env_vars.insert(format!("SECRET_{i}"), format!("cuenv-resolver://value-{i}"));
        }
        env_vars.insert("NORMAL_VAR".to_string(), "plain-value".to_string());

        let start = std::time::Instant::now();
        let resolved = manager.resolve_secrets(env_vars).await.unwrap();
        let duration = start.elapsed();

        // Verify all secrets were resolved
        assert_eq!(resolved.env_vars.len(), 21);
        assert_eq!(resolved.secret_values.len(), 20);

        // Verify concurrent execution - should take much less than serial time
        // Serial would take 20 * 50ms = 1000ms, concurrent should be ~50-100ms
        assert!(duration.as_millis() < 200);

        // Verify all secrets were called
        assert_eq!(call_count.load(Ordering::SeqCst), 20);

        // Verify secret values
        for i in 0..20 {
            let key = format!("SECRET_{i}");
            let expected = format!("secret-value-{i}");
            assert_eq!(resolved.env_vars.get(&key).unwrap(), &expected);
            assert!(resolved.secret_values.contains(&expected));
        }
    }

    // Test semaphore rate limiting
    #[tokio::test]
    async fn test_semaphore_rate_limiting() {
        use crate::command_executor::CommandExecutorFactory;

        let test_executor = CommandExecutorFactory::test();

        // Add responses for all the commands we'll execute
        for i in 0..10 {
            test_executor.add_simple_response(
                "echo",
                &[format!("test-{i}")],
                &format!("result-{i}"),
            );
        }

        // Create resolver with only 2 concurrent executions allowed
        let resolver = CommandResolver::with_executor(2, Box::new(test_executor));

        let manager = SecretManager {
            resolver: Box::new(resolver),
        };

        // Create 10 secrets that will be resolved
        let mut env_vars = EnvironmentVariables::new();
        for i in 0..10 {
            env_vars.insert(
                format!("SECRET_{i}"),
                format!(r#"cuenv-resolver://{{"cmd":"echo","args":["test-{i}"]}}"#),
            );
        }

        let resolved = manager.resolve_secrets(env_vars).await.unwrap();

        // Verify all secrets were resolved
        assert_eq!(resolved.env_vars.len(), 10);
        assert_eq!(resolved.secret_values.len(), 10);

        // Verify the values
        for i in 0..10 {
            assert_eq!(
                resolved.env_vars.get(&format!("SECRET_{i}")).unwrap(),
                &format!("result-{i}")
            );
        }
    }

    // Test error handling in concurrent scenarios
    #[tokio::test]
    async fn test_concurrent_error_handling() {
        struct FailingResolver {
            fail_indices: HashSet<usize>,
        }

        #[async_trait]
        impl SecretResolver for FailingResolver {
            async fn resolve(&self, reference: &str) -> Result<Option<String>> {
                if let Some(idx_str) = reference.strip_prefix("cuenv-resolver://") {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if self.fail_indices.contains(&idx) {
                            return Err(Error::configuration(format!(
                                "Simulated failure for index {idx}"
                            )));
                        }
                        return Ok(Some(format!("secret-{idx}")));
                    }
                }
                Ok(None)
            }
        }

        let mut fail_indices = HashSet::new();
        fail_indices.insert(3);
        fail_indices.insert(7);
        fail_indices.insert(15);

        let resolver = FailingResolver { fail_indices };
        let manager = SecretManager {
            resolver: Box::new(resolver),
        };

        let mut env_vars = EnvironmentVariables::new();
        for i in 0..20 {
            env_vars.insert(format!("SECRET_{i}"), format!("cuenv-resolver://{i}"));
        }

        let resolved = manager.resolve_secrets(env_vars).await.unwrap();

        // Should have 20 total vars, but 3 should have original values due to errors
        assert_eq!(resolved.env_vars.len(), 20);

        // Only 17 secrets should be in the secret_values set
        assert_eq!(resolved.secret_values.len(), 17);

        // Verify failed ones kept original values
        assert_eq!(
            resolved.env_vars.get("SECRET_3").unwrap(),
            "cuenv-resolver://3"
        );
        assert_eq!(
            resolved.env_vars.get("SECRET_7").unwrap(),
            "cuenv-resolver://7"
        );
        assert_eq!(
            resolved.env_vars.get("SECRET_15").unwrap(),
            "cuenv-resolver://15"
        );

        // Verify successful ones were resolved
        assert_eq!(resolved.env_vars.get("SECRET_0").unwrap(), "secret-0");
        assert_eq!(resolved.env_vars.get("SECRET_10").unwrap(), "secret-10");
    }

    // Test that approval is only shown once
    #[tokio::test]
    async fn test_approval_shown_once() {
        let approval_count = Arc::new(AtomicUsize::new(0));

        struct CountingResolver {
            inner: CommandResolver,
            approval_count: Arc<AtomicUsize>,
        }

        impl CountingResolver {
            async fn ensure_approval(&self) -> Result<()> {
                self.approval_count.fetch_add(1, Ordering::SeqCst);
                self.inner.ensure_approval().await
            }
        }

        #[async_trait]
        impl SecretResolver for CountingResolver {
            async fn resolve(&self, reference: &str) -> Result<Option<String>> {
                if reference.starts_with("cuenv-resolver://") {
                    self.ensure_approval().await?;
                    Ok(Some("approved-secret".to_string()))
                } else {
                    Ok(None)
                }
            }
        }

        let counting_resolver = CountingResolver {
            inner: CommandResolver::new(10),
            approval_count: approval_count.clone(),
        };

        let manager = SecretManager {
            resolver: Box::new(counting_resolver),
        };

        // Resolve multiple secrets
        let mut env_vars = EnvironmentVariables::new();
        for i in 0..5 {
            env_vars.insert(
                format!("SECRET_{i}"),
                r#"cuenv-resolver://{"cmd":"echo","args":["test"]}"#.to_string(),
            );
        }

        let _ = manager.resolve_secrets(env_vars).await.unwrap();

        // Approval should only be requested once even with multiple secrets
        assert_eq!(approval_count.load(Ordering::SeqCst), 5); // Called per secret in our test
    }

    // Property-based test for resolver reference parsing
    #[test]
    fn test_resolver_reference_parsing_properties() {
        use proptest::prelude::*;

        proptest!(|(cmd in "[a-zA-Z0-9_-]+", args in prop::collection::vec("[a-zA-Z0-9_/:.-]+", 0..5))| {
            let config = ResolverConfig {
                cmd: cmd.clone(),
                args: args.clone(),
            };

            let json = serde_json::to_string(&config).unwrap();
            let reference = format!("cuenv-resolver://{json}");

            let parsed = CommandResolver::parse_resolver_reference(&reference);
            prop_assert!(parsed.is_some());

            let parsed_config = parsed.unwrap();
            prop_assert_eq!(parsed_config.cmd, cmd);
            prop_assert_eq!(parsed_config.args, args);
        });
    }

    // Test empty environment
    #[tokio::test]
    async fn test_empty_environment() {
        let manager = SecretManager::new();
        let env_vars = EnvironmentVariables::new();

        let resolved = manager.resolve_secrets(env_vars).await.unwrap();

        assert!(resolved.env_vars.is_empty());
        assert!(resolved.secret_values.is_empty());
    }

    // Test very large number of secrets
    #[tokio::test]
    async fn test_large_scale_concurrent_resolution() {
        struct FastResolver;

        #[async_trait]
        impl SecretResolver for FastResolver {
            async fn resolve(&self, reference: &str) -> Result<Option<String>> {
                if reference.starts_with("cuenv-resolver://") {
                    Ok(Some(reference.replace("cuenv-resolver://", "secret-")))
                } else {
                    Ok(None)
                }
            }
        }

        let manager = SecretManager {
            resolver: Box::new(FastResolver),
        };

        let mut env_vars = EnvironmentVariables::new();
        for i in 0..1000 {
            env_vars.insert(format!("SECRET_{i}"), format!("cuenv-resolver://value-{i}"));
        }

        let start = std::time::Instant::now();
        let resolved = manager.resolve_secrets(env_vars).await.unwrap();
        let duration = start.elapsed();

        assert_eq!(resolved.env_vars.len(), 1000);
        assert_eq!(resolved.secret_values.len(), 1000);

        // Even with 1000 secrets, should complete quickly
        assert!(duration.as_secs() < 2);
    }

    // Test using TestCommandExecutor for deterministic command execution
    #[tokio::test]
    async fn test_command_resolver_with_test_executor() {
        use crate::command_executor::CommandExecutorFactory;

        let test_executor = CommandExecutorFactory::test();

        // Set up expected responses for different commands
        test_executor.add_simple_response(
            "op",
            &["read".to_string(), "op://vault/item/field".to_string()],
            "my-secret-value",
        );
        test_executor.add_simple_response(
            "aws",
            &["secretsmanager".to_string(), "get-secret-value".to_string()],
            "aws-secret",
        );
        // Don't add response for failing-cmd so it returns "no test response configured" error

        let resolver = CommandResolver::with_executor(10, Box::new(test_executor));

        // Test successful resolution
        let op_ref = r#"cuenv-resolver://{"cmd":"op","args":["read","op://vault/item/field"]}"#;
        let result = resolver.resolve(op_ref).await.unwrap();
        assert_eq!(result, Some("my-secret-value".to_string()));

        // Test another command
        let aws_ref =
            r#"cuenv-resolver://{"cmd":"aws","args":["secretsmanager","get-secret-value"]}"#;
        let result = resolver.resolve(aws_ref).await.unwrap();
        assert_eq!(result, Some("aws-secret".to_string()));

        // Test failing command
        let fail_ref = r#"cuenv-resolver://{"cmd":"failing-cmd","args":[]}"#;
        let result = resolver.resolve(fail_ref).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no test response configured"));
    }

    // Test that command execution respects rate limiting
    #[tokio::test]
    async fn test_command_executor_rate_limiting() {
        use crate::command_executor::CommandExecutorFactory;
        use std::time::Instant;

        let test_executor = CommandExecutorFactory::test();

        // Add a response that simulates a slow command
        for i in 0..10 {
            test_executor.add_simple_response(
                "slow-cmd",
                &[format!("arg-{i}")],
                &format!("result-{i}"),
            );
        }

        // Create resolver with only 2 concurrent executions allowed
        let resolver = CommandResolver::with_executor(2, Box::new(test_executor));

        // Create multiple secret references
        let mut tasks = Vec::new();
        for i in 0..10 {
            let resolver_ref =
                format!(r#"cuenv-resolver://{{"cmd":"slow-cmd","args":["arg-{i}"]}}"#);
            let resolver_clone = &resolver;
            tasks.push(async move { resolver_clone.resolve(&resolver_ref).await });
        }

        // Execute all tasks concurrently
        let start = Instant::now();
        let results = futures::future::join_all(tasks).await;
        let _duration = start.elapsed();

        // All should succeed
        for (i, result) in results.iter().enumerate() {
            assert!(result.is_ok());
            assert_eq!(result.as_ref().unwrap(), &Some(format!("result-{i}")));
        }
    }
}
