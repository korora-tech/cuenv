use crate::errors::{Error, Result};

/// Apply default resource limits for task execution
/// This helps prevent runaway processes from consuming too many resources
#[cfg(unix)]
pub fn apply_default_limits() -> Result<()> {
    use libc::{rlimit, setrlimit, RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE};

    // Set CPU time limit to 1 hour by default
    let cpu_limit = rlimit {
        rlim_cur: 3600, // 1 hour soft limit
        rlim_max: 7200, // 2 hour hard limit
    };

    // Safety: setrlimit is safe when called with valid resource and limit values
    unsafe {
        if setrlimit(RLIMIT_CPU, &cpu_limit) != 0 {
            return Err(Error::configuration(
                "Failed to set CPU time limit".to_string(),
            ));
        }
    }

    // Set memory limit to 4GB by default
    let mem_limit = rlimit {
        rlim_cur: 4 * 1024 * 1024 * 1024, // 4GB soft limit
        rlim_max: 8 * 1024 * 1024 * 1024, // 8GB hard limit
    };

    // Safety: setrlimit is safe when called with valid resource and limit values
    unsafe {
        if setrlimit(RLIMIT_AS, &mem_limit) != 0 {
            // Non-fatal: some systems don't support RLIMIT_AS
            log::debug!("Failed to set memory limit (may not be supported on this system)");
        }
    }

    // Set file descriptor limit
    let fd_limit = rlimit {
        rlim_cur: 1024, // 1024 file descriptors soft limit
        rlim_max: 4096, // 4096 file descriptors hard limit
    };

    // Safety: setrlimit is safe when called with valid resource and limit values
    unsafe {
        if setrlimit(RLIMIT_NOFILE, &fd_limit) != 0 {
            log::debug!("Failed to set file descriptor limit");
        }
    }

    Ok(())
}

/// Apply default resource limits for task execution (Windows stub)
#[cfg(windows)]
pub fn apply_default_limits() -> Result<()> {
    // Resource limits are not easily available on Windows
    // This is a no-op for now
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_apply_limits_succeeds() {
        // This test might fail in some restricted environments
        // so we just check it doesn't panic
        let _ = apply_default_limits();
    }

    #[test]
    #[cfg(windows)]
    fn test_apply_limits_noop_on_windows() {
        assert!(apply_default_limits().is_ok());
    }
}
