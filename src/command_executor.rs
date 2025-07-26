use crate::audit::{audit_logger, AuditLogger};
use crate::errors::{Error, Result};
use crate::retry::convenience::retry_command;
use crate::security::SecurityValidator;
use crate::types::{CommandArguments, EnvironmentVariables};
use async_trait::async_trait;
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::process::Output;
use std::sync::Arc;

/// Trait for executing external commands
/// This abstraction allows for testing without mocking by providing
/// different implementations for production and test environments
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Execute a command with the given arguments
    /// Returns the output of the command
    async fn execute(&self, cmd: &str, args: &CommandArguments) -> Result<Output>;

    /// Execute a command with environment variables
    async fn execute_with_env(
        &self,
        cmd: &str,
        args: &CommandArguments,
        env: EnvironmentVariables,
    ) -> Result<Output>;
}

/// Production implementation that executes real commands
pub struct SystemCommandExecutor {
    allowed_commands: HashSet<String>,
    audit_logger: Option<Arc<AuditLogger>>,
    /// Whether to use retry logic for transient failures
    pub enable_retry: bool,
}

impl SystemCommandExecutor {
    /// Create a new system command executor with default allowed commands
    pub fn new() -> Self {
        Self {
            allowed_commands: SecurityValidator::default_command_allowlist(),
            audit_logger: audit_logger(),
            enable_retry: true,
        }
    }

    /// Create a new system command executor with custom allowed commands
    pub fn with_allowed_commands(allowed_commands: HashSet<String>) -> Self {
        Self {
            allowed_commands,
            audit_logger: audit_logger(),
            enable_retry: true,
        }
    }

    /// Create a SystemCommandExecutor without retry logic
    pub fn without_retry() -> Self {
        Self {
            allowed_commands: SecurityValidator::default_command_allowlist(),
            audit_logger: audit_logger(),
            enable_retry: false,
        }
    }

    /// Execute command once without retry
    async fn execute_once(&self, cmd: &str, args: &CommandArguments) -> Result<Output> {
        // Validate command against allowlist
        let validation_result = SecurityValidator::validate_command(cmd, &self.allowed_commands);

        // Log command execution attempt
        if let Some(ref logger) = self.audit_logger {
            let allowed = validation_result.is_ok();
            let reason = validation_result.as_ref().err().map(|e| e.to_string());
            let _ = logger
                .log_command_execution(cmd, args.as_slice(), allowed, reason)
                .await;
        }

        validation_result?;

        // Validate command arguments
        SecurityValidator::validate_command_args(args.as_slice())?;

        match std::process::Command::new(cmd)
            .args(args.as_slice())
            .output()
        {
            Ok(output) => Ok(output),
            Err(e) => Err(Error::command_execution(
                cmd,
                args.as_slice().to_vec(),
                format!("failed to execute command: {e}"),
                None,
            )),
        }
    }

    /// Execute command with environment once without retry
    async fn execute_with_env_once(
        &self,
        cmd: &str,
        args: &CommandArguments,
        env: EnvironmentVariables,
    ) -> Result<Output> {
        // Validate command against allowlist
        let validation_result = SecurityValidator::validate_command(cmd, &self.allowed_commands);

        // Log command execution attempt
        if let Some(ref logger) = self.audit_logger {
            let allowed = validation_result.is_ok();
            let reason = validation_result.as_ref().err().map(|e| e.to_string());
            let _ = logger
                .log_command_execution(cmd, args.as_slice(), allowed, reason)
                .await;
        }

        validation_result?;

        // Validate command arguments
        SecurityValidator::validate_command_args(args.as_slice())?;

        // Validate environment variable names
        for (key, _) in env.iter() {
            SecurityValidator::sanitize_env_var_name(key)?;
        }

        match std::process::Command::new(cmd)
            .args(args.as_slice())
            .envs(env.into_inner())
            .output()
        {
            Ok(output) => Ok(output),
            Err(e) => Err(Error::command_execution(
                cmd,
                args.as_slice().to_vec(),
                format!("failed to execute command with environment: {e}"),
                None,
            )),
        }
    }
}

impl Default for SystemCommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandExecutor for SystemCommandExecutor {
    async fn execute(&self, cmd: &str, args: &CommandArguments) -> Result<Output> {
        if self.enable_retry {
            retry_command(|| async { self.execute_once(cmd, args).await }).await
        } else {
            self.execute_once(cmd, args).await
        }
    }

    async fn execute_with_env(
        &self,
        cmd: &str,
        args: &CommandArguments,
        env: EnvironmentVariables,
    ) -> Result<Output> {
        if self.enable_retry {
            // Create an Arc to share the env across retry attempts without cloning
            let env_arc = Arc::new(env);
            retry_command(|| {
                let env_ref = Arc::clone(&env_arc);
                async move {
                    // Clone only when actually needed for the execution
                    self.execute_with_env_once(cmd, args, (*env_ref).clone())
                        .await
                }
            })
            .await
        } else {
            self.execute_with_env_once(cmd, args, env).await
        }
    }
}

/// Test implementation that simulates command execution
/// This provides deterministic behavior for testing
#[cfg(test)]
pub struct TestCommandExecutor {
    responses: std::sync::Arc<std::sync::Mutex<HashMap<String, TestResponse>>>,
}

#[cfg(test)]
#[derive(Clone)]
pub struct TestResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status_code: i32,
}

#[cfg(test)]
impl TestCommandExecutor {
    pub fn new() -> Self {
        Self {
            responses: std::sync::Arc::new(std::sync::Mutex::new(HashMap::with_capacity(10))),
        }
    }

    pub fn add_response(&self, cmd: &str, args: &[String], response: TestResponse) {
        let key = format!("{} {}", cmd, args.join(" "));
        match self.responses.lock() {
            Ok(mut responses) => {
                responses.insert(key, response);
            }
            Err(e) => {
                eprintln!("Failed to lock test responses: {}", e);
            }
        }
    }

    pub fn add_simple_response(&self, cmd: &str, args: &[String], stdout: &str) {
        self.add_response(
            cmd,
            args,
            TestResponse {
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
                status_code: 0,
            },
        );
    }

    pub fn add_error_response(&self, cmd: &str, args: &[String], stderr: &str) {
        self.add_response(
            cmd,
            args,
            TestResponse {
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
                status_code: 1,
            },
        );
    }
}

#[cfg(test)]
#[async_trait]
impl CommandExecutor for TestCommandExecutor {
    async fn execute(&self, cmd: &str, args: &CommandArguments) -> Result<Output> {
        let key = format!("{} {}", cmd, args.as_slice().join(" "));
        let responses = self
            .responses
            .lock()
            .map_err(|e| Error::configuration(format!("Failed to lock test responses: {}", e)))?;

        match responses.get(&key) {
            Some(response) => Ok(Output {
                status: exit_status::from_raw(response.status_code),
                stdout: response.stdout.to_vec(),
                stderr: response.stderr.to_vec(),
            }),
            None => Err(Error::configuration(format!(
                "no test response configured for command: {}",
                key
            ))),
        }
    }

    async fn execute_with_env(
        &self,
        cmd: &str,
        args: &CommandArguments,
        _env: EnvironmentVariables,
    ) -> Result<Output> {
        // For testing, we ignore env vars and just use the base execute
        self.execute(cmd, args).await
    }
}

/// Factory for creating command executors
pub struct CommandExecutorFactory;

impl CommandExecutorFactory {
    /// Create a production command executor with default allowed commands
    pub fn system() -> Box<dyn CommandExecutor> {
        Box::new(SystemCommandExecutor::new())
    }

    /// Create a production command executor with custom allowed commands
    pub fn system_with_allowed_commands(
        allowed_commands: HashSet<String>,
    ) -> Box<dyn CommandExecutor> {
        Box::new(SystemCommandExecutor::with_allowed_commands(
            allowed_commands,
        ))
    }

    /// Create a test command executor
    #[cfg(test)]
    pub fn test() -> TestCommandExecutor {
        TestCommandExecutor::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_test_executor_simple_response() {
        let executor = CommandExecutorFactory::test();
        executor.add_simple_response("echo", &["hello".to_string()], "hello\n");

        let args = CommandArguments::from_vec(vec!["hello".to_string()]);
        let output = executor
            .execute("echo", &args)
            .await
            .expect("Failed to execute echo command");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
        assert!(output.status.success());
    }

    #[tokio::test]
    async fn test_test_executor_error_response() {
        let executor = CommandExecutorFactory::test();
        executor.add_error_response("false", &[], "command failed");

        let args = CommandArguments::new();
        let output = executor
            .execute("false", &args)
            .await
            .expect("Failed to execute false command");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "command failed");
        assert!(!output.status.success());
    }

    #[tokio::test]
    async fn test_test_executor_missing_response() {
        let executor = CommandExecutorFactory::test();

        let args = CommandArguments::from_vec(vec!["cmd".to_string()]);
        let result = executor.execute("unknown", &args).await;
        assert!(result.is_err());
        let err = result.expect_err("Expected error for unknown command");
        assert!(err.to_string().contains("no test response configured"));
    }
}

// Platform-specific module for creating ExitStatus
#[cfg(test)]
mod exit_status {
    #[cfg(unix)]
    pub fn from_raw(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code)
    }

    #[cfg(windows)]
    pub fn from_raw(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }
}
