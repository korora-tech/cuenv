use crate::errors::{Error, Result};
use fs2::FileExt;
use once_cell::sync::Lazy;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

/// Global mutex for thread-safe environment variable access
static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Get the lock file path for cuenv instances
fn get_lock_file_path() -> PathBuf {
    let xdg_runtime = env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let temp_dir = env::temp_dir();
            temp_dir.join(format!("cuenv-{}", users::get_current_uid()))
        });

    // Ensure the directory exists
    let _ = fs::create_dir_all(&xdg_runtime);
    xdg_runtime.join("cuenv.lock")
}

/// Thread-safe environment variable operations
pub struct SyncEnv;

impl SyncEnv {
    /// Set an environment variable with thread safety
    pub fn set_var<K: AsRef<str>, V: AsRef<str>>(key: K, value: V) -> Result<()> {
        let _guard = ENV_MUTEX.lock().map_err(|e| {
            Error::environment(
                "ENV_MUTEX",
                format!("Failed to acquire environment lock: {}", e),
            )
        })?;

        env::set_var(key.as_ref(), value.as_ref());
        Ok(())
    }

    /// Get an environment variable with thread safety
    pub fn var<K: AsRef<str>>(key: K) -> Result<Option<String>> {
        let _guard = ENV_MUTEX.lock().map_err(|e| {
            Error::environment(
                "ENV_MUTEX",
                format!("Failed to acquire environment lock: {}", e),
            )
        })?;

        Ok(env::var(key.as_ref()).ok())
    }

    /// Remove an environment variable with thread safety
    pub fn remove_var<K: AsRef<str>>(key: K) -> Result<()> {
        let _guard = ENV_MUTEX.lock().map_err(|e| {
            Error::environment(
                "ENV_MUTEX",
                format!("Failed to acquire environment lock: {}", e),
            )
        })?;

        env::remove_var(key.as_ref());
        Ok(())
    }

    /// Get all environment variables with thread safety
    pub fn vars() -> Result<Vec<(String, String)>> {
        let _guard = ENV_MUTEX.lock().map_err(|e| {
            Error::environment(
                "ENV_MUTEX",
                format!("Failed to acquire environment lock: {}", e),
            )
        })?;

        Ok(env::vars().collect())
    }
}

/// File-based lock for concurrent cuenv instances
pub struct InstanceLock {
    file: Option<File>,
}

impl InstanceLock {
    /// Try to acquire an exclusive lock for cuenv operations
    pub fn try_acquire() -> Result<Self> {
        let lock_path = get_lock_file_path();

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::file_system(&lock_path, "open lock file", e))?;

        // Try to acquire exclusive lock
        file.try_lock_exclusive()
            .map_err(|_| Error::configuration("Another cuenv instance is already running"))?;

        Ok(Self { file: Some(file) })
    }

    /// Acquire an exclusive lock, waiting if necessary
    pub fn acquire() -> Result<Self> {
        let lock_path = get_lock_file_path();

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::file_system(&lock_path, "open lock file", e))?;

        // Acquire exclusive lock, blocking until available
        file.lock_exclusive()
            .map_err(|_| Error::configuration("Failed to acquire exclusive lock"))?;

        Ok(Self { file: Some(file) })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_thread_safe_env_operations() {
        let key = format!("TEST_SYNC_ENV_{}", uuid::Uuid::new_v4());

        // Test setting and getting
        SyncEnv::set_var(&key, "value1").unwrap();
        assert_eq!(SyncEnv::var(&key).unwrap(), Some("value1".to_string()));

        // Test concurrent access
        let key_clone = key.clone();
        let handle = thread::spawn(move || {
            SyncEnv::set_var(&key_clone, "value2").unwrap();
        });

        handle.join().unwrap();
        assert_eq!(SyncEnv::var(&key).unwrap(), Some("value2".to_string()));

        // Cleanup
        SyncEnv::remove_var(&key).unwrap();
        assert_eq!(SyncEnv::var(&key).unwrap(), None);
    }

    #[test]
    fn test_instance_lock() {
        // First lock should succeed
        let lock1 = InstanceLock::try_acquire().unwrap();

        // Second lock should fail
        assert!(InstanceLock::try_acquire().is_err());

        // Drop first lock
        drop(lock1);

        // Now second lock should succeed
        let _lock2 = InstanceLock::try_acquire().unwrap();
    }

    #[test]
    fn test_concurrent_env_modifications() {
        let base_key = format!("TEST_CONCURRENT_{}", uuid::Uuid::new_v4());
        let num_threads = 10;
        let iterations = 100;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let key = format!("{}_{}", base_key, i);
                thread::spawn(move || {
                    for j in 0..iterations {
                        let value = format!("thread_{}_iter_{}", i, j);
                        SyncEnv::set_var(&key, &value).unwrap();

                        // Verify the value was set correctly
                        let retrieved = SyncEnv::var(&key).unwrap();
                        assert_eq!(retrieved, Some(value));

                        // Small delay to increase chance of race conditions
                        thread::sleep(Duration::from_micros(10));
                    }

                    // Cleanup
                    SyncEnv::remove_var(&key).unwrap();
                })
            })
            .collect();

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all variables were cleaned up
        for i in 0..num_threads {
            let key = format!("{}_{}", base_key, i);
            assert_eq!(SyncEnv::var(&key).unwrap(), None);
        }
    }
}
