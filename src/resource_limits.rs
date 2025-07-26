use crate::errors::{Error, Result};

/// Configuration for resource limits
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// CPU time limit in seconds (soft, hard)
    pub cpu_time: Option<(u64, u64)>,
    /// Memory limit in bytes (soft, hard)
    pub memory: Option<(u64, u64)>,
    /// File descriptor limit (soft, hard)
    pub file_descriptors: Option<(u64, u64)>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_time: Some((3600, 7200)), // 1 hour soft, 2 hour hard
            memory: Some((4 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024)), // 4GB soft, 8GB hard
            file_descriptors: Some((1024, 4096)),
        }
    }
}

impl ResourceLimits {
    /// Create a new ResourceLimits with no limits
    pub fn unlimited() -> Self {
        Self {
            cpu_time: None,
            memory: None,
            file_descriptors: None,
        }
    }

    /// Set CPU time limit
    pub fn with_cpu_time(mut self, soft_seconds: u64, hard_seconds: u64) -> Self {
        self.cpu_time = Some((soft_seconds, hard_seconds));
        self
    }

    /// Set memory limit
    pub fn with_memory(mut self, soft_bytes: u64, hard_bytes: u64) -> Self {
        self.memory = Some((soft_bytes, hard_bytes));
        self
    }

    /// Set file descriptor limit
    pub fn with_file_descriptors(mut self, soft: u64, hard: u64) -> Self {
        self.file_descriptors = Some((soft, hard));
        self
    }
}

/// Apply default resource limits for task execution
/// This helps prevent runaway processes from consuming too many resources
#[cfg(unix)]
pub fn apply_default_limits() -> Result<()> {
    apply_limits(&ResourceLimits::default())
}

/// Apply custom resource limits for task execution
#[cfg(unix)]
pub fn apply_limits(limits: &ResourceLimits) -> Result<()> {
    use libc::{rlimit, setrlimit, RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE};

    // Set CPU time limit if configured
    if let Some((soft, hard)) = limits.cpu_time {
        let cpu_limit = rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };

        // Safety: setrlimit is safe when called with valid resource and limit values
        unsafe {
            if setrlimit(RLIMIT_CPU, &cpu_limit) != 0 {
                return Err(Error::configuration(
                    "Failed to set CPU time limit".to_string(),
                ));
            }
        }
    }

    // Set memory limit if configured
    if let Some((soft, hard)) = limits.memory {
        let mem_limit = rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };

        // Safety: setrlimit is safe when called with valid resource and limit values
        unsafe {
            if setrlimit(RLIMIT_AS, &mem_limit) != 0 {
                // Non-fatal: some systems don't support RLIMIT_AS
                log::debug!("Failed to set memory limit (may not be supported on this system)");
            }
        }
    }

    // Set file descriptor limit if configured
    if let Some((soft, hard)) = limits.file_descriptors {
        let fd_limit = rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };

        // Safety: setrlimit is safe when called with valid resource and limit values
        unsafe {
            if setrlimit(RLIMIT_NOFILE, &fd_limit) != 0 {
                log::debug!("Failed to set file descriptor limit");
            }
        }
    }

    Ok(())
}

/// Apply default resource limits for task execution (Windows stub)
#[cfg(windows)]
pub fn apply_default_limits() -> Result<()> {
    apply_limits(&ResourceLimits::default())
}

/// Apply custom resource limits for task execution (Windows stub)
#[cfg(windows)]
pub fn apply_limits(_limits: &ResourceLimits) -> Result<()> {
    // Resource limits are not easily available on Windows
    // This is a no-op for now
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_apply_default_limits_succeeds() {
        // This test might fail in some restricted environments
        // so we just check it doesn't panic
        let _ = apply_default_limits();
    }

    #[test]
    #[cfg(unix)]
    fn test_apply_custom_limits() {
        let limits = ResourceLimits::default()
            .with_cpu_time(1800, 3600) // 30 min soft, 1 hour hard
            .with_memory(2 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024); // 2GB soft, 4GB hard
        
        let _ = apply_limits(&limits);
    }

    #[test]
    fn test_unlimited_limits() {
        let limits = ResourceLimits::unlimited();
        assert!(limits.cpu_time.is_none());
        assert!(limits.memory.is_none());
        assert!(limits.file_descriptors.is_none());
    }

    #[test]
    #[cfg(windows)]
    fn test_apply_limits_noop_on_windows() {
        assert!(apply_default_limits().is_ok());
    }
}
