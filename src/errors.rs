use std::fmt;
use std::path::PathBuf;

/// Result type alias for cuenv operations
pub type Result<T> = std::result::Result<T, Error>;

/// Core error type for cuenv operations
#[derive(Debug)]
pub enum Error {
    /// CUE file parsing errors
    CueParse {
        path: PathBuf,
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Environment variable related errors
    Environment { variable: String, message: String },

    /// Secret resolution errors
    SecretResolution {
        reference: String,
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Command execution errors
    CommandExecution {
        command: String,
        args: Vec<String>,
        message: String,
        exit_code: Option<i32>,
    },

    /// Configuration errors
    Configuration { message: String },

    /// Shell expansion errors
    ShellExpansion { value: String, message: String },

    /// File system operations
    FileSystem {
        path: PathBuf,
        operation: String,
        source: std::io::Error,
    },

    /// JSON serialization/deserialization errors
    Json {
        message: String,
        source: serde_json::Error,
    },

    /// FFI errors from CUE operations
    Ffi { operation: String, message: String },

    /// Permission denied errors
    PermissionDenied { operation: String, message: String },

    /// Unsupported operation errors
    Unsupported { feature: String, message: String },

    /// Security validation errors
    Security { message: String },

    /// Network-related errors
    Network { endpoint: String, message: String },

    /// Operation timeout errors
    Timeout {
        operation: String,
        duration: std::time::Duration,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CueParse { path, message, .. } => {
                write!(
                    f,
                    "failed to parse CUE file '{}': {}",
                    path.display(),
                    message
                )
            }
            Error::Environment { variable, message } => {
                write!(f, "environment variable '{variable}' error: {message}")
            }
            Error::SecretResolution {
                reference, message, ..
            } => {
                write!(f, "failed to resolve secret '{reference}': {message}")
            }
            Error::CommandExecution {
                command,
                args,
                message,
                exit_code,
            } => {
                let args_str = args.join(" ");
                match exit_code {
                    Some(code) => {
                        if args_str.is_empty() {
                            write!(
                                f,
                                "command '{command}' failed with exit code {code}: {message}"
                            )
                        } else {
                            write!(f, "command '{command} {args_str}' failed with exit code {code}: {message}")
                        }
                    }
                    None => {
                        if args_str.is_empty() {
                            write!(f, "command '{command}' failed: {message}")
                        } else {
                            write!(f, "command '{command} {args_str}' failed: {message}")
                        }
                    }
                }
            }
            Error::Configuration { message } => {
                write!(f, "configuration error: {message}")
            }
            Error::ShellExpansion { value, message } => {
                write!(f, "failed to expand shell value '{value}': {message}")
            }
            Error::FileSystem {
                path,
                operation,
                source,
            } => {
                write!(
                    f,
                    "file system {} operation failed for '{}': {}",
                    operation,
                    path.display(),
                    source
                )
            }
            Error::Json { message, .. } => {
                write!(f, "JSON error: {message}")
            }
            Error::Ffi { operation, message } => {
                write!(f, "FFI operation '{operation}' failed: {message}")
            }
            Error::PermissionDenied { operation, message } => {
                write!(f, "permission denied for {operation}: {message}")
            }
            Error::Unsupported { feature, message } => {
                write!(f, "unsupported feature '{feature}': {message}")
            }
            Error::Security { message } => {
                write!(f, "security validation error: {message}")
            }
            Error::Network { endpoint, message } => {
                write!(f, "network error for '{}': {}", endpoint, message)
            }
            Error::Timeout {
                operation,
                duration,
            } => {
                write!(
                    f,
                    "operation '{}' timed out after {:?}",
                    operation, duration
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::CueParse { source, .. } => source
                .as_ref()
                .map(|e| e.as_ref() as &(dyn std::error::Error + 'static)),
            Error::SecretResolution { source, .. } => source
                .as_ref()
                .map(|e| e.as_ref() as &(dyn std::error::Error + 'static)),
            Error::FileSystem { source, .. } => Some(source),
            Error::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}

// Conversion implementations
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::FileSystem {
            path: PathBuf::new(),
            operation: "unknown".to_string(),
            source: error,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json {
            message: error.to_string(),
            source: error,
        }
    }
}

// Helper methods for creating errors with context
impl Error {
    /// Create a CUE parse error with context
    #[must_use]
    pub fn cue_parse(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Error::CueParse {
            path: path.into(),
            message: message.into(),
            source: None,
        }
    }

    /// Create a CUE parse error with a source error
    #[must_use]
    pub fn cue_parse_with_source(
        path: impl Into<PathBuf>,
        message: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Error::CueParse {
            path: path.into(),
            message: message.into(),
            source: Some(source.into()),
        }
    }

    /// Create an environment variable error
    #[must_use]
    pub fn environment(variable: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Environment {
            variable: variable.into(),
            message: message.into(),
        }
    }

    /// Create a secret resolution error
    #[must_use]
    pub fn secret_resolution(reference: impl Into<String>, message: impl Into<String>) -> Self {
        Error::SecretResolution {
            reference: reference.into(),
            message: message.into(),
            source: None,
        }
    }

    /// Create a command execution error
    #[must_use]
    pub fn command_execution(
        command: impl Into<String>,
        args: Vec<String>,
        message: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Error::CommandExecution {
            command: command.into(),
            args,
            message: message.into(),
            exit_code,
        }
    }

    /// Create a configuration error
    #[must_use]
    pub fn configuration(message: impl Into<String>) -> Self {
        Error::Configuration {
            message: message.into(),
        }
    }

    /// Create a shell expansion error
    #[must_use]
    pub fn shell_expansion(value: impl Into<String>, message: impl Into<String>) -> Self {
        Error::ShellExpansion {
            value: value.into(),
            message: message.into(),
        }
    }

    /// Create a file system error with context
    #[must_use]
    pub fn file_system(
        path: impl Into<PathBuf>,
        operation: impl Into<String>,
        source: std::io::Error,
    ) -> Self {
        Error::FileSystem {
            path: path.into(),
            operation: operation.into(),
            source,
        }
    }

    /// Create an FFI error
    #[must_use]
    pub fn ffi(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Ffi {
            operation: operation.into(),
            message: message.into(),
        }
    }

    /// Create a permission denied error
    #[must_use]
    pub fn permission_denied(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Error::PermissionDenied {
            operation: operation.into(),
            message: message.into(),
        }
    }

    /// Create an unsupported feature error
    #[must_use]
    pub fn unsupported(feature: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Unsupported {
            feature: feature.into(),
            message: message.into(),
        }
    }

    /// Create a security validation error
    #[must_use]
    pub fn security(message: impl Into<String>) -> Self {
        Error::Security {
            message: message.into(),
        }
    }

    /// Create a network error
    #[must_use]
    pub fn network(endpoint: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Network {
            endpoint: endpoint.into(),
            message: message.into(),
        }
    }

    /// Create a timeout error
    #[must_use]
    pub fn timeout(operation: impl Into<String>, duration: std::time::Duration) -> Self {
        Error::Timeout {
            operation: operation.into(),
            duration,
        }
    }
}

// Extension trait for adding context to Results
pub trait ResultExt<T> {
    /// Add context to a Result
    fn context(self, message: impl Into<String>) -> Result<T>;

    /// Add context with a lazy message
    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String;
}

impl<T, E> ResultExt<T> for std::result::Result<T, E>
where
    E: Into<Error>,
{
    fn context(self, message: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            let base_error = e.into();
            Error::Configuration {
                message: format!("{}: {}", message.into(), base_error),
            }
        })
    }

    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String,
    {
        self.map_err(|e| {
            let base_error = e.into();
            Error::Configuration {
                message: format!("{}: {}", f(), base_error),
            }
        })
    }
}
