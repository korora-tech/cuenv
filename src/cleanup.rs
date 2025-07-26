//! Resource cleanup and error recovery module
//!
//! This module provides RAII guards and cleanup utilities to ensure
//! proper resource cleanup in all scenarios including errors and panics.

use crate::errors::{Error, Result};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Global cleanup registry for tracking resources
static CLEANUP_REGISTRY: Lazy<Arc<Mutex<CleanupRegistry>>> =
    Lazy::new(|| Arc::new(Mutex::new(CleanupRegistry::new())));

/// Registry for tracking resources that need cleanup
pub struct CleanupRegistry {
    resources: HashMap<u64, CleanupResource>,
    next_id: u64,
}

/// A resource that needs cleanup
struct CleanupResource {
    description: String,
    cleanup_fn: Box<dyn FnOnce() + Send>,
}

impl CleanupRegistry {
    fn new() -> Self {
        Self {
            resources: HashMap::with_capacity(16), // Pre-allocate for typical usage
            next_id: 0,
        }
    }

    /// Register a resource for cleanup
    fn register<F>(&mut self, description: String, cleanup_fn: F) -> u64
    where
        F: FnOnce() + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;

        self.resources.insert(
            id,
            CleanupResource {
                description,
                cleanup_fn: Box::new(cleanup_fn),
            },
        );

        id
    }

    /// Unregister a resource (called when it's cleaned up normally)
    fn unregister(&mut self, id: u64) {
        self.resources.remove(&id);
    }

    /// Clean up all registered resources
    fn cleanup_all(&mut self) {
        let resources: Vec<_> = self.resources.drain().collect();
        for (_, resource) in resources {
            log::debug!("Emergency cleanup: {}", resource.description);
            (resource.cleanup_fn)();
        }
    }
}

/// RAII guard for temporary files
pub struct TempFileGuard {
    path: PathBuf,
    registry_id: Option<u64>,
    cleanup_on_drop: bool,
}

impl TempFileGuard {
    /// Create a new temporary file guard
    pub fn new(path: PathBuf) -> Self {
        let path_clone = path.clone();
        let description = format!("temporary file: {}", path.display());

        let registry_id = match CLEANUP_REGISTRY.lock() {
            Ok(mut registry) => Some(registry.register(description, move || {
                let _ = fs::remove_file(&path_clone);
            })),
            Err(e) => {
                log::error!("Failed to lock cleanup registry: {}", e);
                None
            }
        };

        Self {
            path,
            registry_id,
            cleanup_on_drop: true,
        }
    }

    /// Get the path to the temporary file
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the file after the guard is dropped
    pub fn keep(mut self) -> PathBuf {
        self.cleanup_on_drop = false;
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }
        self.path.clone()
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }

        if self.cleanup_on_drop && self.path.exists() {
            if let Err(e) = fs::remove_file(&self.path) {
                log::warn!(
                    "Failed to remove temporary file {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}

/// RAII guard for temporary directories
pub struct TempDirGuard {
    path: PathBuf,
    registry_id: Option<u64>,
    cleanup_on_drop: bool,
}

impl TempDirGuard {
    /// Create a new temporary directory guard
    pub fn new(path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&path)
            .map_err(|e| Error::file_system(path.clone(), "create temporary directory", e))?;

        let path_clone = path.clone();
        let description = format!("temporary directory: {}", path.display());

        let registry_id = match CLEANUP_REGISTRY.lock() {
            Ok(mut registry) => Some(registry.register(description, move || {
                let _ = fs::remove_dir_all(&path_clone);
            })),
            Err(e) => {
                log::error!("Failed to lock cleanup registry: {}", e);
                None
            }
        };

        Ok(Self {
            path,
            registry_id,
            cleanup_on_drop: true,
        })
    }

    /// Get the path to the temporary directory
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the directory after the guard is dropped
    pub fn keep(mut self) -> PathBuf {
        self.cleanup_on_drop = false;
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }
        self.path.clone()
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }

        if self.cleanup_on_drop && self.path.exists() {
            if let Err(e) = fs::remove_dir_all(&self.path) {
                log::warn!(
                    "Failed to remove temporary directory {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}

/// RAII guard for process cleanup
pub struct ProcessGuard {
    child: Option<std::process::Child>,
    registry_id: Option<u64>,
    timeout: Duration,
    started_at: Instant,
}

impl ProcessGuard {
    /// Create a new process guard
    pub fn new(child: std::process::Child, timeout: Duration) -> Self {
        let pid = child.id();
        let description = format!("process: PID {}", pid);

        let registry_id = match CLEANUP_REGISTRY.lock() {
            Ok(mut registry) => Some(registry.register(description, move || {
                // Try to kill the process
                #[cfg(unix)]
                {
                    let _ = std::process::Command::new("kill")
                        .args(&["-TERM", &pid.to_string()])
                        .status();

                    std::thread::sleep(Duration::from_millis(100));

                    let _ = std::process::Command::new("kill")
                        .args(&["-9", &pid.to_string()])
                        .status();
                }
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(&["/F", "/PID", &pid.to_string()])
                        .status();
                }
            })),
            Err(e) => {
                log::error!("Failed to lock cleanup registry: {}", e);
                None
            }
        };

        Self {
            child: Some(child),
            registry_id,
            timeout,
            started_at: Instant::now(),
        }
    }

    /// Wait for the process to complete with timeout
    pub fn wait_with_timeout(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(child) = self.child.as_mut() {
            let remaining = self.timeout.saturating_sub(self.started_at.elapsed());

            // Check if already timed out
            if remaining.is_zero() {
                self.kill()?;
                return Err(Error::configuration("Process timed out"));
            }

            // Try to wait with timeout
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.child = None;
                    if let Some(id) = self.registry_id.take() {
                        if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                            registry.unregister(id);
                        } else {
                            log::error!("Failed to lock cleanup registry for unregister");
                        }
                    }
                    Ok(status)
                }
                Ok(None) => {
                    // Still running, implement polling wait
                    let poll_interval = Duration::from_millis(100);
                    let deadline = Instant::now() + remaining;

                    loop {
                        std::thread::sleep(poll_interval);

                        match child.try_wait() {
                            Ok(Some(status)) => {
                                self.child = None;
                                if let Some(id) = self.registry_id.take() {
                                    if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                                        registry.unregister(id);
                                    } else {
                                        log::error!(
                                            "Failed to lock cleanup registry for unregister"
                                        );
                                    }
                                }
                                return Ok(status);
                            }
                            Ok(None) => {
                                if Instant::now() >= deadline {
                                    self.kill()?;
                                    return Err(Error::configuration("Process timed out"));
                                }
                            }
                            Err(e) => {
                                return Err(Error::configuration(format!(
                                    "Failed to wait for process: {}",
                                    e
                                )))
                            }
                        }
                    }
                }
                Err(e) => Err(Error::configuration(format!(
                    "Failed to wait for process: {}",
                    e
                ))),
            }
        } else {
            Err(Error::configuration("Process already consumed"))
        }
    }

    /// Kill the process
    pub fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            if let Some(id) = self.registry_id.take() {
                if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                    registry.unregister(id);
                } else {
                    log::error!("Failed to lock cleanup registry for unregister");
                }
            }

            child
                .kill()
                .map_err(|e| Error::configuration(format!("Failed to kill process: {}", e)))?;
        }
        Ok(())
    }

    /// Take ownership of the child process
    pub fn into_inner(mut self) -> Option<std::process::Child> {
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }
        self.child.take()
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(id) = self.registry_id.take() {
            if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
                registry.unregister(id);
            } else {
                log::error!("Failed to lock cleanup registry for unregister");
            }
        }

        if let Some(mut child) = self.child.take() {
            // Try graceful shutdown first
            match child.try_wait() {
                Ok(Some(_)) => {} // Already exited
                _ => {
                    // Send SIGTERM on Unix, just kill on Windows
                    #[cfg(unix)]
                    {
                        let pid = child.id();
                        let _ = std::process::Command::new("kill")
                            .args(&["-TERM", &pid.to_string()])
                            .status();

                        // Give it a moment to exit gracefully
                        std::thread::sleep(Duration::from_millis(100));

                        // Check if it's still running
                        if child.try_wait().ok().flatten().is_none() {
                            let _ = child.kill();
                        }
                    }
                    #[cfg(windows)]
                    {
                        let _ = child.kill();
                    }
                }
            }
        }
    }
}

/// Initialize cleanup handling (called once at startup)
pub fn init_cleanup_handler() {
    // Register signal handlers for graceful shutdown
    #[cfg(unix)]
    {
        use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};
        use std::thread;

        let registry = Arc::downgrade(&CLEANUP_REGISTRY);

        thread::spawn(move || {
            let mut signals = match Signals::new(&[SIGINT, SIGTERM]) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Failed to register signal handlers: {}", e);
                    return;
                }
            };

            for sig in signals.forever() {
                log::info!("Received signal {}, cleaning up resources...", sig);

                if let Some(registry) = registry.upgrade() {
                    if let Ok(mut reg) = registry.lock() {
                        reg.cleanup_all();
                    } else {
                        log::error!("Failed to lock cleanup registry in signal handler");
                    }
                }

                std::process::exit(128 + sig);
            }
        });
    }

    // Register panic handler
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Clean up resources before panicking
        if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
            registry.cleanup_all();
        } else {
            eprintln!("Failed to lock cleanup registry in panic handler");
        }

        // Call the original panic handler
        original_hook(panic_info);
    }));
}

/// Clean up all registered resources (for emergency cleanup)
pub fn cleanup_all_resources() {
    if let Ok(mut registry) = CLEANUP_REGISTRY.lock() {
        registry.cleanup_all();
    } else {
        log::error!("Failed to lock cleanup registry for cleanup_all_resources");
    }
}

/// Scoped cleanup guard that runs a function on drop
pub struct ScopedCleanup<F: FnOnce()> {
    cleanup_fn: Option<F>,
}

impl<F: FnOnce()> ScopedCleanup<F> {
    /// Create a new scoped cleanup guard
    pub fn new(cleanup_fn: F) -> Self {
        Self {
            cleanup_fn: Some(cleanup_fn),
        }
    }

    /// Cancel the cleanup
    pub fn cancel(mut self) {
        self.cleanup_fn = None;
    }
}

impl<F: FnOnce()> Drop for ScopedCleanup<F> {
    fn drop(&mut self) {
        if let Some(cleanup_fn) = self.cleanup_fn.take() {
            cleanup_fn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_temp_file_guard() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        {
            File::create(&file_path).unwrap();
            let _guard = TempFileGuard::new(file_path.clone());
            assert!(file_path.exists());
        }

        // File should be deleted after guard is dropped
        assert!(!file_path.exists());
    }

    #[test]
    fn test_temp_file_guard_keep() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        {
            File::create(&file_path).unwrap();
            let guard = TempFileGuard::new(file_path.clone());
            let _kept_path = guard.keep();
            assert!(file_path.exists());
        }

        // File should still exist after guard is dropped
        assert!(file_path.exists());
    }

    #[test]
    fn test_temp_dir_guard() {
        let temp_dir = tempdir().unwrap();
        let dir_path = temp_dir.path().join("test_dir");

        {
            let _guard = TempDirGuard::new(dir_path.clone()).unwrap();
            assert!(dir_path.exists());

            // Create a file in the directory
            File::create(dir_path.join("test.txt")).unwrap();
        }

        // Directory and its contents should be deleted
        assert!(!dir_path.exists());
    }

    #[test]
    fn test_scoped_cleanup() {
        let mut cleaned = false;

        {
            let _cleanup = ScopedCleanup::new(|| {
                cleaned = true;
            });
        }

        assert!(cleaned);
    }

    #[test]
    fn test_scoped_cleanup_cancel() {
        let mut cleaned = false;

        {
            let cleanup = ScopedCleanup::new(|| {
                cleaned = true;
            });
            cleanup.cancel();
        }

        assert!(!cleaned);
    }

    #[test]
    fn test_process_guard_timeout() {
        // Create a long-running process
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();

        let mut guard = ProcessGuard::new(child, Duration::from_millis(100));

        // Should timeout
        let result = guard.wait_with_timeout();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }
}
