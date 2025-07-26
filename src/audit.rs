use crate::errors::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Audit event severity levels
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuditLevel {
    Info,
    Warning,
    Critical,
}

/// Types of audit events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditEventType {
    /// Hook execution events
    HookExecution {
        hook_type: String,
        command: String,
        args: Vec<String>,
        success: bool,
        duration_ms: u64,
    },
    /// Secret resolution events
    SecretResolution {
        key: String,
        resolver_type: String,
        success: bool,
        error: Option<String>,
    },
    /// File operation events
    FileOperation {
        operation: String,
        path: PathBuf,
        success: bool,
        error: Option<String>,
    },
    /// Command execution events
    CommandExecution {
        command: String,
        args: Vec<String>,
        allowed: bool,
        reason: Option<String>,
    },
    /// Security validation events
    SecurityValidation {
        validation_type: String,
        target: String,
        passed: bool,
        details: Option<String>,
    },
    /// Environment state changes
    EnvironmentStateChange {
        action: String,
        directory: PathBuf,
        environment: Option<String>,
        capabilities: Vec<String>,
    },
    /// Rate limit events
    RateLimitEvent {
        resource: String,
        limit: usize,
        current: usize,
        blocked: bool,
    },
}

/// Audit log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub level: AuditLevel,
    pub event_type: AuditEventType,
    pub user: String,
    pub session_id: String,
    pub metadata: HashMap<String, String>,
}

/// Audit logger configuration
#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub enabled: bool,
    pub log_file: Option<PathBuf>,
    pub min_level: AuditLevel,
    pub include_metadata: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_file: None,
            min_level: AuditLevel::Info,
            include_metadata: true,
        }
    }
}

/// Audit logger for tracking security-sensitive operations
pub struct AuditLogger {
    config: AuditConfig,
    session_id: String,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

impl AuditLogger {
    /// Create a new audit logger
    pub fn new(config: AuditConfig) -> Result<Self> {
        let session_id = uuid::Uuid::new_v4().to_string();

        let writer: Option<Box<dyn Write + Send>> = if let Some(ref path) = config.log_file {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| crate::errors::Error::FileSystem {
                    path: path.clone(),
                    operation: "open".to_string(),
                    source: e,
                })?;
            Some(Box::new(file))
        } else {
            None
        };

        Ok(Self {
            config,
            session_id,
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    /// Get the current session ID
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Log an audit event
    pub async fn log(&self, level: AuditLevel, event_type: AuditEventType) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        // Check if event meets minimum level
        if !self.should_log(level) {
            return Ok(());
        }

        let entry = self.create_entry(level, event_type);
        self.write_entry(&entry).await
    }

    /// Log a hook execution event
    pub async fn log_hook_execution(
        &self,
        hook_type: &str,
        command: &str,
        args: &[String],
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        let level = if success {
            AuditLevel::Info
        } else {
            AuditLevel::Warning
        };

        self.log(
            level,
            AuditEventType::HookExecution {
                hook_type: hook_type.to_string(),
                command: command.to_string(),
                args: args.to_vec(),
                success,
                duration_ms,
            },
        )
        .await
    }

    /// Log a secret resolution event
    pub async fn log_secret_resolution(
        &self,
        key: &str,
        resolver_type: &str,
        success: bool,
        error: Option<String>,
    ) -> Result<()> {
        let level = if success {
            AuditLevel::Info
        } else {
            AuditLevel::Warning
        };

        self.log(
            level,
            AuditEventType::SecretResolution {
                key: key.to_string(),
                resolver_type: resolver_type.to_string(),
                success,
                error,
            },
        )
        .await
    }

    /// Log a file operation event
    pub async fn log_file_operation(
        &self,
        operation: &str,
        path: &Path,
        success: bool,
        error: Option<String>,
    ) -> Result<()> {
        let level = if success {
            AuditLevel::Info
        } else {
            AuditLevel::Warning
        };

        self.log(
            level,
            AuditEventType::FileOperation {
                operation: operation.to_string(),
                path: path.to_path_buf(),
                success,
                error,
            },
        )
        .await
    }

    /// Log a command execution event
    pub async fn log_command_execution(
        &self,
        command: &str,
        args: &[String],
        allowed: bool,
        reason: Option<String>,
    ) -> Result<()> {
        let level = if allowed {
            AuditLevel::Info
        } else {
            AuditLevel::Critical
        };

        self.log(
            level,
            AuditEventType::CommandExecution {
                command: command.to_string(),
                args: args.to_vec(),
                allowed,
                reason,
            },
        )
        .await
    }

    /// Log a security validation event
    pub async fn log_security_validation(
        &self,
        validation_type: &str,
        target: &str,
        passed: bool,
        details: Option<String>,
    ) -> Result<()> {
        let level = if passed {
            AuditLevel::Info
        } else {
            AuditLevel::Critical
        };

        self.log(
            level,
            AuditEventType::SecurityValidation {
                validation_type: validation_type.to_string(),
                target: target.to_string(),
                passed,
                details,
            },
        )
        .await
    }

    /// Log an environment state change
    pub async fn log_environment_change(
        &self,
        action: &str,
        directory: &Path,
        environment: Option<&str>,
        capabilities: &[String],
    ) -> Result<()> {
        self.log(
            AuditLevel::Info,
            AuditEventType::EnvironmentStateChange {
                action: action.to_string(),
                directory: directory.to_path_buf(),
                environment: environment.map(|s| s.to_string()),
                capabilities: capabilities.to_vec(),
            },
        )
        .await
    }

    /// Log a rate limit event
    pub async fn log_rate_limit(
        &self,
        resource: &str,
        limit: usize,
        current: usize,
        blocked: bool,
    ) -> Result<()> {
        let level = if blocked {
            AuditLevel::Warning
        } else {
            AuditLevel::Info
        };

        self.log(
            level,
            AuditEventType::RateLimitEvent {
                resource: resource.to_string(),
                limit,
                current,
                blocked,
            },
        )
        .await
    }

    // Private methods

    fn should_log(&self, level: AuditLevel) -> bool {
        match (self.config.min_level, level) {
            (AuditLevel::Info, _) => true,
            (AuditLevel::Warning, AuditLevel::Info) => false,
            (AuditLevel::Warning, _) => true,
            (AuditLevel::Critical, AuditLevel::Critical) => true,
            (AuditLevel::Critical, _) => false,
        }
    }

    fn create_entry(&self, level: AuditLevel, event_type: AuditEventType) -> AuditEntry {
        let mut metadata = HashMap::new();

        if self.config.include_metadata {
            // Add system metadata
            if let Ok(hostname) = hostname::get() {
                metadata.insert(
                    "hostname".to_string(),
                    hostname.to_string_lossy().to_string(),
                );
            }

            // Add process metadata
            metadata.insert("pid".to_string(), std::process::id().to_string());

            // Add environment metadata
            if let Ok(cwd) = std::env::current_dir() {
                metadata.insert("cwd".to_string(), cwd.display().to_string());
            }
        }

        AuditEntry {
            timestamp: Utc::now(),
            level,
            event_type,
            user: whoami::username(),
            session_id: self.session_id.clone(),
            metadata,
        }
    }

    async fn write_entry(&self, entry: &AuditEntry) -> Result<()> {
        let json = serde_json::to_string(entry).map_err(|e| crate::errors::Error::Json {
            message: "Failed to serialize audit entry".to_string(),
            source: e,
        })?;

        let mut writer = self.writer.lock().await;

        if let Some(ref mut w) = *writer {
            writeln!(w, "{}", json).map_err(|e| crate::errors::Error::FileSystem {
                path: PathBuf::from("audit.log"),
                operation: "write".to_string(),
                source: e,
            })?;
            w.flush().map_err(|e| crate::errors::Error::FileSystem {
                path: PathBuf::from("audit.log"),
                operation: "flush".to_string(),
                source: e,
            })?;
        } else {
            // If no file configured, log to stderr
            eprintln!("AUDIT: {}", json);
        }

        Ok(())
    }
}

/// Global audit logger instance
static AUDIT_LOGGER: once_cell::sync::OnceCell<Arc<AuditLogger>> = once_cell::sync::OnceCell::new();

/// Initialize the global audit logger
pub fn init_audit_logger(config: AuditConfig) -> Result<()> {
    let logger = Arc::new(AuditLogger::new(config)?);
    AUDIT_LOGGER.set(logger).map_err(|_| {
        crate::errors::Error::configuration("Audit logger already initialized".to_string())
    })?;
    Ok(())
}

/// Get the global audit logger
pub fn audit_logger() -> Option<Arc<AuditLogger>> {
    AUDIT_LOGGER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_audit_logger_creation() {
        let config = AuditConfig::default();
        let logger = AuditLogger::new(config).unwrap();
        assert!(!logger.session_id().is_empty());
    }

    #[tokio::test]
    async fn test_audit_logging_to_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let config = AuditConfig {
            enabled: true,
            log_file: Some(temp_file.path().to_path_buf()),
            min_level: AuditLevel::Info,
            include_metadata: true,
        };

        let logger = AuditLogger::new(config).unwrap();

        // Log a test event
        logger
            .log_command_execution("test", &["arg1".to_string()], true, None)
            .await
            .unwrap();

        // Read the log file
        let content = std::fs::read_to_string(temp_file.path()).unwrap();
        assert!(!content.is_empty());

        // Verify JSON structure
        let entry: AuditEntry = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry.session_id, logger.session_id());

        match entry.event_type {
            AuditEventType::CommandExecution {
                command,
                args,
                allowed,
                ..
            } => {
                assert_eq!(command, "test");
                assert_eq!(args, vec!["arg1"]);
                assert!(allowed);
            }
            _ => panic!("Unexpected event type"),
        }
    }

    #[tokio::test]
    async fn test_audit_level_filtering() {
        let config = AuditConfig {
            enabled: true,
            log_file: None,
            min_level: AuditLevel::Warning,
            include_metadata: false,
        };

        let logger = AuditLogger::new(config).unwrap();

        // Info level should not be logged
        assert!(logger.should_log(AuditLevel::Info) == false);

        // Warning and Critical should be logged
        assert!(logger.should_log(AuditLevel::Warning));
        assert!(logger.should_log(AuditLevel::Critical));
    }

    #[tokio::test]
    async fn test_disabled_audit_logger() {
        let config = AuditConfig {
            enabled: false,
            ..Default::default()
        };

        let logger = AuditLogger::new(config).unwrap();

        // Should not error even when disabled
        logger
            .log_hook_execution("test", "echo", &[], true, 100)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_various_event_types() {
        let temp_file = NamedTempFile::new().unwrap();
        let config = AuditConfig {
            enabled: true,
            log_file: Some(temp_file.path().to_path_buf()),
            min_level: AuditLevel::Info,
            include_metadata: false,
        };

        let logger = AuditLogger::new(config).unwrap();

        // Test different event types
        logger
            .log_hook_execution("on_enter", "echo", &["hello".to_string()], true, 50)
            .await
            .unwrap();

        logger
            .log_secret_resolution("API_KEY", "command", false, Some("Failed".to_string()))
            .await
            .unwrap();

        logger
            .log_file_operation("read", Path::new("/tmp/test"), true, None)
            .await
            .unwrap();

        logger
            .log_security_validation(
                "path",
                "/etc/passwd",
                false,
                Some("Outside allowed paths".to_string()),
            )
            .await
            .unwrap();

        logger.log_rate_limit("hooks", 10, 11, true).await.unwrap();

        // Verify all events were logged
        let content = std::fs::read_to_string(temp_file.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 5);
    }
}
