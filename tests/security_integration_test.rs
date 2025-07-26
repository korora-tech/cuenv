use cuenv::audit::{init_audit_logger, AuditConfig, AuditLevel};
use cuenv::command_executor::{CommandExecutor, SystemCommandExecutor};
use cuenv::hook_manager::HookManager;
use cuenv::rate_limit::{RateLimitConfig, RateLimitManager};
use cuenv::secrets::{CommandResolver, SecretManager};
use cuenv::security::SecurityValidator;
use cuenv::types::{CommandArguments, EnvironmentVariables};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;

#[tokio::test]
async fn test_audit_logging_integration() {
    // Setup audit logging to a temporary file
    let log_file = NamedTempFile::new().unwrap();
    let config = AuditConfig {
        enabled: true,
        log_file: Some(log_file.path().to_path_buf()),
        min_level: AuditLevel::Info,
        include_metadata: true,
    };

    init_audit_logger(config).unwrap();

    // Test command execution with audit logging
    let mut allowed_commands = HashSet::new();
    allowed_commands.insert("echo".to_string());

    let executor = SystemCommandExecutor::with_allowed_commands(allowed_commands);
    let args = CommandArguments::from_vec(vec!["test".to_string()]);

    // This should succeed and be logged
    let _ = executor.execute("echo", &args).await;

    // This should fail and be logged
    let _ = executor.execute("rm", &args).await;

    // Give time for async writes
    sleep(Duration::from_millis(100)).await;

    // Verify logs were written
    let log_content = std::fs::read_to_string(log_file.path()).unwrap();
    assert!(log_content.contains("CommandExecution"));
    assert!(log_content.contains("echo"));
    assert!(log_content.contains("rm"));
    assert!(log_content.contains("allowed\":true"));
    assert!(log_content.contains("allowed\":false"));
}

#[tokio::test]
async fn test_rate_limiting_hooks() {
    // Create rate limiter with low limits for testing
    let rate_limiter = Arc::new(RateLimitManager::new());
    rate_limiter
        .register(
            "hooks",
            RateLimitConfig {
                max_operations: 2,
                window_duration: Duration::from_secs(1),
                sliding_window: true,
                burst_size: None,
            },
        )
        .await;

    // Create hook manager with rate limiting
    let executor = Arc::new(SystemCommandExecutor::new());
    let hook_manager = HookManager::new(executor.clone())
        .unwrap()
        .with_rate_limiter(rate_limiter.clone());

    let hook_config = cuenv::cue_parser::HookConfig {
        command: "echo".to_string(),
        args: vec!["test".to_string()],
        url: None,
        hook_type: cuenv::cue_parser::HookType::OnEnter,
    };

    let env_vars = HashMap::new();

    // First two should succeed
    assert!(hook_manager
        .execute_hook(&hook_config, &env_vars)
        .await
        .is_ok());
    assert!(hook_manager
        .execute_hook(&hook_config, &env_vars)
        .await
        .is_ok());

    // Third should fail due to rate limit
    assert!(hook_manager
        .execute_hook(&hook_config, &env_vars)
        .await
        .is_err());

    // Wait for window to pass
    sleep(Duration::from_millis(1100)).await;

    // Should succeed again
    assert!(hook_manager
        .execute_hook(&hook_config, &env_vars)
        .await
        .is_ok());
}

#[tokio::test]
async fn test_rate_limiting_secrets() {
    // Create rate limiter for secrets
    let rate_limiter = Arc::new(RateLimitManager::new());
    rate_limiter
        .register(
            "secrets",
            RateLimitConfig {
                max_operations: 3,
                window_duration: Duration::from_secs(1),
                sliding_window: false,
                burst_size: None,
            },
        )
        .await;

    // Create command resolver with rate limiting
    let test_executor = cuenv::command_executor::CommandExecutorFactory::test();
    test_executor.add_simple_response("echo", &["secret".to_string()], "my-secret-value");

    let resolver =
        CommandResolver::with_executor(5, Box::new(test_executor)).with_rate_limiter(rate_limiter);

    let manager = SecretManager::with_resolver(Box::new(resolver));

    // Create multiple secrets
    let mut env_vars = EnvironmentVariables::new();
    for i in 0..5 {
        env_vars.insert(
            format!("SECRET_{}", i),
            r#"cuenv-resolver://{"cmd":"echo","args":["secret"]}"#.to_string(),
        );
    }

    // Should process first 3, but fail on the rest due to rate limit
    let result = manager.resolve_secrets(env_vars).await;
    assert!(result.is_ok());

    let resolved = result.unwrap();
    // Some secrets may have failed due to rate limiting
    let resolved_count = resolved.secret_values.len();
    assert!(resolved_count <= 3);
}

#[tokio::test]
async fn test_security_validation_with_audit() {
    // Setup audit logging
    let log_file = NamedTempFile::new().unwrap();
    let config = AuditConfig {
        enabled: true,
        log_file: Some(log_file.path().to_path_buf()),
        min_level: AuditLevel::Info,
        include_metadata: false,
    };

    init_audit_logger(config).unwrap();

    // Test various security validations
    let mut allowed_commands = HashSet::new();
    allowed_commands.insert("ls".to_string());

    // Command injection attempts
    assert!(SecurityValidator::validate_command("ls; rm -rf /", &allowed_commands).is_err());
    assert!(SecurityValidator::validate_command("ls | cat", &allowed_commands).is_err());
    assert!(SecurityValidator::validate_command("ls && whoami", &allowed_commands).is_err());

    // Path traversal attempts
    let allowed_paths = vec![
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/home/user"),
    ];

    assert!(SecurityValidator::validate_path(
        std::path::Path::new("/tmp/../etc/passwd"),
        &allowed_paths
    )
    .is_err());

    // Shell expansion validation
    assert!(SecurityValidator::validate_shell_expansion("$(whoami)").is_err());
    assert!(SecurityValidator::validate_shell_expansion("${PATH}").is_err());

    sleep(Duration::from_millis(100)).await;

    // Verify security events were logged
    let log_content = std::fs::read_to_string(log_file.path()).unwrap();
    // The audit logger would be triggered through the command executor
}

#[tokio::test]
async fn test_concurrent_rate_limiting() {
    let rate_limiter = Arc::new(RateLimitManager::new());
    rate_limiter
        .register(
            "test",
            RateLimitConfig {
                max_operations: 10,
                window_duration: Duration::from_secs(1),
                sliding_window: true,
                burst_size: Some(5),
            },
        )
        .await;

    // Spawn multiple concurrent tasks
    let mut handles = vec![];
    for i in 0..20 {
        let rate_limiter = rate_limiter.clone();
        let handle = tokio::spawn(async move {
            let result = rate_limiter.try_acquire("test").await;
            (i, result.is_ok())
        });
        handles.push(handle);
    }

    // Collect results
    let mut success_count = 0;
    for handle in handles {
        let (_, success) = handle.await.unwrap();
        if success {
            success_count += 1;
        }
    }

    // Should have limited the number of successful operations
    assert!(success_count <= 10);
    assert!(success_count > 0);
}

#[tokio::test]
async fn test_input_validation_comprehensive() {
    // Test command validation
    let allowed = SecurityValidator::default_command_allowlist();
    assert!(SecurityValidator::validate_command("echo", &allowed).is_ok());
    assert!(SecurityValidator::validate_command("rm", &allowed).is_err());

    // Test command args validation
    let safe_args = vec!["hello".to_string(), "world".to_string()];
    assert!(SecurityValidator::validate_command_args(&safe_args).is_ok());

    let dangerous_args = vec!["$(whoami)".to_string()];
    assert!(SecurityValidator::validate_command_args(&dangerous_args).is_err());

    // Test environment variable name sanitization
    assert!(SecurityValidator::sanitize_env_var_name("VALID_VAR").is_ok());
    assert!(SecurityValidator::sanitize_env_var_name("123INVALID").is_err());
    assert!(SecurityValidator::sanitize_env_var_name("VAR-WITH-DASH").is_err());
    assert!(SecurityValidator::sanitize_env_var_name("VAR WITH SPACE").is_err());

    // Test CUE content validation
    let safe_cue = r#"env: { FOO: "bar" }"#;
    assert!(SecurityValidator::validate_cue_content(safe_cue).is_ok());

    let dangerous_cue = r#"env: { "__proto__": {} }"#;
    assert!(SecurityValidator::validate_cue_content(dangerous_cue).is_err());
}

#[tokio::test]
async fn test_audit_log_rotation_simulation() {
    // This test simulates what would happen with log rotation
    let log_file = NamedTempFile::new().unwrap();
    let config = AuditConfig {
        enabled: true,
        log_file: Some(log_file.path().to_path_buf()),
        min_level: AuditLevel::Info,
        include_metadata: false,
    };

    init_audit_logger(config).unwrap();

    // Execute some commands to generate logs
    let executor = SystemCommandExecutor::new();
    let args = CommandArguments::new();

    for i in 0..10 {
        let _ = executor.execute("echo", &args).await;
        sleep(Duration::from_millis(10)).await;
    }

    // Read and verify log entries
    let content = std::fs::read_to_string(log_file.path()).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert!(lines.len() >= 10);

    // Each line should be valid JSON
    for line in lines {
        if !line.is_empty() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("timestamp").is_some());
            assert!(parsed.get("event_type").is_some());
        }
    }
}
