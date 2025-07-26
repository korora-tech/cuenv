//! Thread-safe cache manager with file locking support
//!
//! This module provides a centralized cache manager that ensures thread-safe
//! access to cache resources using file locking and proper synchronization.

use crate::atomic_file::write_atomic_string;
use crate::cache::CacheEngine;
use crate::cache::CachedTaskResult;
use crate::cue_parser::TaskConfig;
use crate::errors::{Error, Result};
use fs2::FileExt;
use globset::{Glob, GlobSetBuilder};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Cache version for migration support
const CACHE_VERSION: u32 = 1;

/// A lock guard that automatically releases and cleans up on drop
struct CacheLockGuard {
    _file: File,
    path: PathBuf,
}

impl Drop for CacheLockGuard {
    fn drop(&mut self) {
        // File unlock happens automatically when File is dropped
        // Try to remove the lock file
        if let Err(e) = fs::remove_file(&self.path) {
            // Only log if it's not a "file not found" error (another process may have cleaned it up)
            if e.kind() != std::io::ErrorKind::NotFound {
                log::debug!("Failed to remove lock file {:?}: {}", self.path, e);
            }
        }
    }
}

/// Statistics for cache operations
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CacheStatistics {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub errors: u64,
    pub lock_contentions: u64,
    pub total_bytes_saved: u64,
    pub last_cleanup: Option<SystemTime>,
}

/// Thread-safe cache manager
pub struct CacheManager {
    /// Underlying cache engine
    engine: Arc<CacheEngine>,

    /// Statistics for monitoring
    stats: Arc<RwLock<CacheStatistics>>,

    /// Cache version for migration support
    version: u32,
}

impl CacheManager {
    /// Create a new cache manager
    pub fn new() -> Result<Self> {
        let engine = Arc::new(CacheEngine::new()?);
        let stats = Arc::new(RwLock::new(CacheStatistics::default()));

        let manager = Self {
            engine,
            stats,
            version: CACHE_VERSION,
        };

        // Check and migrate cache if needed
        manager.check_and_migrate()?;

        Ok(manager)
    }

    /// Check cache version and migrate if necessary
    fn check_and_migrate(&self) -> Result<()> {
        let version_file = self.engine.cache_dir.join("VERSION");

        if version_file.exists() {
            let content = fs::read_to_string(&version_file)
                .map_err(|e| Error::file_system(&version_file, "read version file", e))?;

            let file_version: u32 = content
                .trim()
                .parse()
                .map_err(|_| Error::configuration("Invalid cache version format".to_string()))?;

            if file_version < self.version {
                log::info!(
                    "Migrating cache from version {} to {}",
                    file_version,
                    self.version
                );
                self.migrate_cache(file_version)?;
            } else if file_version > self.version {
                return Err(Error::configuration(format!(
                    "Cache version {} is newer than supported version {}",
                    file_version, self.version
                )));
            }
        } else {
            // Write current version atomically
            write_atomic_string(&version_file, &self.version.to_string())?;
        }

        Ok(())
    }

    /// Migrate cache from old version to current
    fn migrate_cache(&self, _old_version: u32) -> Result<()> {
        // For now, we just clear the cache on version change
        // In the future, we can implement specific migration logic
        log::warn!("Cache migration not implemented, clearing cache");
        self.engine.clear()?;

        // Write new version atomically
        let version_file = self.engine.cache_dir.join("VERSION");
        write_atomic_string(&version_file, &self.version.to_string())?;

        Ok(())
    }

    /// Acquire a lock for a cache key
    fn acquire_lock(&self, cache_key: &str) -> Result<CacheLockGuard> {
        let lock_path = self
            .engine
            .cache_dir
            .join("locks")
            .join(format!("{}.lock", cache_key));

        // Ensure lock directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::file_system(parent.to_path_buf(), "create lock directory", e)
            })?;
        }

        // Open or create lock file
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::file_system(&lock_path, "open lock file", e))?;

        // Try to acquire exclusive lock with exponential backoff
        let start = std::time::Instant::now();
        let mut backoff_ms = 10u64; // Start with 10ms
        const MAX_BACKOFF_MS: u64 = 1000; // Cap at 1 second
        const TIMEOUT_SECS: u64 = 30;

        let mut contention_recorded = false;

        loop {
            match lock_file.try_lock_exclusive() {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() > Duration::from_secs(TIMEOUT_SECS) {
                        return Err(Error::configuration(format!(
                            "Timeout waiting for lock on cache key: {} (waited {}s)",
                            cache_key, TIMEOUT_SECS
                        )));
                    }

                    // Record contention only once per acquisition attempt
                    if !contention_recorded {
                        match self.stats.write() {
                            Ok(mut stats) => {
                                stats.lock_contentions += 1;
                            }
                            Err(_) => {
                                log::error!("Failed to update lock contention statistics due to poisoned mutex");
                            }
                        }
                        contention_recorded = true;
                    }

                    // Exponential backoff with jitter
                    let jitter = backoff_ms / 4; // 25% jitter
                    let mut rng = rand::thread_rng();
                    let sleep_ms = backoff_ms + rng.gen_range(0..jitter);
                    std::thread::sleep(Duration::from_millis(sleep_ms));

                    // Double the backoff time for next iteration, up to max
                    backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                }
                Err(e) => {
                    return Err(Error::file_system(lock_path, "acquire exclusive lock", e));
                }
            }
        }

        Ok(CacheLockGuard {
            _file: lock_file,
            path: lock_path,
        })
    }

    /// Generate a cache key for a task
    pub fn generate_cache_key(
        &self,
        task_name: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
    ) -> Result<String> {
        let mut hasher = self
            .engine
            .hash
            .create_hasher(&format!("task:{}", task_name));

        // Add namespace and version to prevent collisions
        hasher.hash_content("cuenv_cache_v1")?;
        hasher.hash_content(self.version)?;

        // Hash the working directory to ensure cache isolation between projects
        hasher.hash_content(working_dir.to_string_lossy().as_ref())?;

        // Use custom cache key if provided
        if let Some(custom_key) = &task_config.cache_key {
            // Still include task name as namespace even with custom key
            hasher.hash_content(task_name)?;
            hasher.hash_content(custom_key)?;
        } else {
            // Hash task name and configuration
            hasher.hash_content(task_name)?;
            hasher.hash_content(task_config)?;
        }

        // Hash input files using proper glob support
        if let Some(inputs) = &task_config.inputs {
            let globset = build_globset(inputs)?;
            let files = expand_globs_with_globset(&globset, working_dir)?;

            // Sort files for consistent hashing
            let mut sorted_files = files;
            sorted_files.sort();

            for file in sorted_files {
                hasher.hash_file(&file)?;
            }
        }

        let hash = hasher.generate_hash()?;

        // Save the hash manifest
        self.engine.hash.save_manifest(&hasher, &hash)?;

        Ok(hash)
    }

    /// Get cached task result with proper locking
    pub fn get_cached_result(
        &self,
        cache_key: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
    ) -> Result<Option<CachedTaskResult>> {
        // Return None if caching is disabled
        if !task_config.cache.unwrap_or(false) {
            return Ok(None);
        }

        // Return None if cache is not readable
        if !self.engine.is_readable() {
            return Ok(None);
        }

        // Acquire lock
        let _lock = self.acquire_lock(cache_key)?;

        let cache_path = self
            .engine
            .cache_dir
            .join("tasks")
            .join(format!("{}.json", cache_key));

        // Read and parse cache file (avoiding TOCTOU by directly attempting to open)
        let content = match fs::read_to_string(&cache_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match self.stats.write() {
                    Ok(mut stats) => stats.misses += 1,
                    Err(_) => log::error!("Failed to update miss statistics due to poisoned mutex"),
                }
                return Ok(None);
            }
            Err(e) => {
                return Err(Error::file_system(&cache_path, "read cache file", e));
            }
        };

        let cached_result: CachedTaskResult =
            serde_json::from_str(&content).map_err(|e| Error::Json {
                message: "Failed to parse cached task result".to_string(),
                source: e,
            })?;

        // Verify cache key matches
        if cached_result.cache_key != cache_key {
            match self.stats.write() {
                Ok(mut stats) => stats.misses += 1,
                Err(_) => log::error!("Failed to update miss statistics due to poisoned mutex"),
            }
            return Ok(None);
        }

        // Verify output files still exist and have correct hashes
        if let Some(outputs) = &task_config.outputs {
            let globset = build_globset(outputs)?;
            let output_files = expand_globs_with_globset(&globset, working_dir)?;

            // Check if we have any expected outputs from cache but no actual files
            if output_files.is_empty() && !cached_result.output_files.is_empty() {
                log::debug!("Cache miss: expected output files but found none");
                match self.stats.write() {
                    Ok(mut stats) => stats.misses += 1,
                    Err(_) => log::error!("Failed to update miss statistics due to poisoned mutex"),
                }
                return Ok(None);
            }

            for output_file in output_files {
                let relative_path = output_file
                    .strip_prefix(working_dir)
                    .unwrap_or(&output_file)
                    .to_string_lossy()
                    .to_string();

                // Check if output file hash matches (combines existence check with hash check)
                if let Some(expected_hash) = cached_result.output_files.get(&relative_path) {
                    match hash_file(&output_file) {
                        Ok(current_hash) => {
                            if &current_hash != expected_hash {
                                log::debug!(
                                    "Cache miss: output file {:?} hash changed",
                                    output_file
                                );
                                match self.stats.write() {
                                    Ok(mut stats) => stats.misses += 1,
                                    Err(_) => log::error!(
                                        "Failed to update miss statistics due to poisoned mutex"
                                    ),
                                }
                                return Ok(None);
                            }
                        }
                        Err(e) => {
                            // File doesn't exist or can't be read
                            log::debug!("Cache miss: output file {:?} error: {}", output_file, e);
                            match self.stats.write() {
                                Ok(mut stats) => stats.misses += 1,
                                Err(_) => log::error!(
                                    "Failed to update miss statistics due to poisoned mutex"
                                ),
                            }
                            return Ok(None);
                        }
                    }
                }
            }

            // Additional check: verify all cached output files still exist
            for (cached_path, _) in &cached_result.output_files {
                let full_path = working_dir.join(cached_path);
                // Use metadata check instead of exists() to avoid TOCTOU
                if full_path.metadata().is_err() {
                    log::debug!(
                        "Cache miss: cached output file {:?} no longer exists",
                        full_path
                    );
                    match self.stats.write() {
                        Ok(mut stats) => stats.misses += 1,
                        Err(_) => {
                            log::error!("Failed to update miss statistics due to poisoned mutex")
                        }
                    }
                    return Ok(None);
                }
            }
        }

        match self.stats.write() {
            Ok(mut stats) => stats.hits += 1,
            Err(_) => log::error!("Failed to update hit statistics due to poisoned mutex"),
        }

        Ok(Some(cached_result))
    }

    /// Save task result to cache with proper locking
    pub fn save_result(
        &self,
        cache_key: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
        exit_code: i32,
    ) -> Result<()> {
        // Don't cache if caching is disabled
        if !task_config.cache.unwrap_or(false) {
            return Ok(());
        }

        // Don't cache if cache is not writable
        if !self.engine.is_writable() {
            return Ok(());
        }

        // Only cache successful executions
        if exit_code != 0 {
            return Ok(());
        }

        // Acquire lock
        let _lock = self.acquire_lock(cache_key)?;

        // Hash output files
        let mut output_files =
            HashMap::with_capacity(task_config.outputs.as_ref().map_or(0, |o| o.len()));
        if let Some(outputs) = &task_config.outputs {
            let globset = build_globset(outputs)?;
            let files = expand_globs_with_globset(&globset, working_dir)?;

            for output_file in files {
                let relative_path = output_file
                    .strip_prefix(working_dir)
                    .unwrap_or(&output_file)
                    .to_string_lossy()
                    .to_string();
                let file_hash = hash_file(&output_file)?;
                output_files.insert(relative_path, file_hash);
            }
        }

        let cached_result = CachedTaskResult {
            cache_key: cache_key.to_string(),
            executed_at: SystemTime::now(),
            exit_code,
            stdout: None, // Placeholder for actual stdout
            stderr: None, // Placeholder for actual stderr
            output_files,
        };

        // Ensure cache directory exists
        let cache_dir = self.engine.cache_dir.join("tasks");
        fs::create_dir_all(&cache_dir)
            .map_err(|e| Error::file_system(&cache_dir, "create cache directory", e))?;

        // Write cache file atomically
        let cache_path = cache_dir.join(format!("{}.json", cache_key));
        let content = serde_json::to_string_pretty(&cached_result).map_err(|e| Error::Json {
            message: "Failed to serialize cache result".to_string(),
            source: e,
        })?;

        write_atomic_string(&cache_path, &content)?;

        match self.stats.write() {
            Ok(mut stats) => {
                stats.writes += 1;
                stats.total_bytes_saved += content.len() as u64;
            }
            Err(_) => log::error!("Failed to update write statistics due to poisoned mutex"),
        }

        Ok(())
    }

    /// Get cache statistics
    pub fn get_statistics(&self) -> Result<CacheStatistics> {
        let guard = self.stats.read().map_err(|_| {
            Error::configuration(
                "Statistics lock was poisoned - cache may be in inconsistent state".to_string(),
            )
        })?;
        Ok(guard.clone())
    }

    /// Clear all cache entries
    pub fn clear(&self) -> Result<()> {
        // Clear the cache engine
        self.engine.clear()?;

        // Reset statistics
        match self.stats.write() {
            Ok(mut stats) => *stats = CacheStatistics::default(),
            Err(_) => log::error!("Failed to reset statistics due to poisoned mutex"),
        }

        Ok(())
    }

    /// Clean up stale cache entries
    pub fn cleanup(&self, max_age: Duration) -> Result<(usize, u64)> {
        let result = self.engine.clean_stale_cache(max_age)?;

        // Also clean up stale lock files
        self.cleanup_stale_locks(max_age)?;

        match self.stats.write() {
            Ok(mut stats) => stats.last_cleanup = Some(SystemTime::now()),
            Err(_) => log::error!("Failed to update cleanup timestamp due to poisoned mutex"),
        }

        Ok(result)
    }

    /// Clean up stale lock files
    fn cleanup_stale_locks(&self, max_age: Duration) -> Result<()> {
        let locks_dir = self.engine.cache_dir.join("locks");
        if !locks_dir.exists() {
            return Ok(());
        }

        let cutoff_time = SystemTime::now() - max_age;

        let entries = fs::read_dir(&locks_dir)
            .map_err(|e| Error::file_system(&locks_dir, "read locks directory", e))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| Error::file_system(&locks_dir, "read lock directory entry", e))?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("lock") {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified < cutoff_time {
                            // This lock file is stale, remove it
                            match fs::remove_file(&path) {
                                Ok(()) => log::debug!("Removed stale lock file: {:?}", path),
                                Err(e) => {
                                    log::warn!("Failed to remove stale lock file {:?}: {}", path, e)
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Print cache statistics summary
    pub fn print_statistics(&self) -> Result<()> {
        let stats = self.get_statistics()?;
        let total_requests = stats.hits + stats.misses;
        let hit_rate = if total_requests > 0 {
            (stats.hits as f64 / total_requests as f64) * 100.0
        } else {
            0.0
        };

        println!("Cache Statistics:");
        println!("  Total Requests: {}", total_requests);
        println!("  Hits:          {} ({:.1}%)", stats.hits, hit_rate);
        println!("  Misses:        {}", stats.misses);
        println!("  Writes:        {}", stats.writes);
        println!("  Errors:        {}", stats.errors);
        println!("  Lock Contentions: {}", stats.lock_contentions);
        println!(
            "  Total Bytes Saved: {}",
            format_bytes(stats.total_bytes_saved)
        );

        if let Some(last_cleanup) = stats.last_cleanup {
            if let Ok(duration) = SystemTime::now().duration_since(last_cleanup) {
                println!("  Last Cleanup: {} ago", format_duration(duration));
            }
        }

        Ok(())
    }
}

/// Build a globset from patterns
fn build_globset(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();

    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| {
            Error::configuration(format!("Invalid glob pattern '{}': {}", pattern, e))
        })?;
        builder.add(glob);
    }

    builder
        .build()
        .map_err(|e| Error::configuration(format!("Failed to build globset: {}", e)))
}

/// Expand glob patterns using globset
fn expand_globs_with_globset(globset: &globset::GlobSet, base_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_directory(base_dir, base_dir, globset, &mut files)?;
    Ok(files)
}

/// Recursively walk directory and collect matching files
fn walk_directory(
    dir: &Path,
    base_dir: &Path,
    globset: &globset::GlobSet,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    // Validate that dir is within base_dir to prevent traversal
    let canonical_dir = dir
        .canonicalize()
        .map_err(|e| Error::file_system(dir, "canonicalize directory", e))?;
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| Error::file_system(base_dir, "canonicalize base directory", e))?;

    if !canonical_dir.starts_with(&canonical_base) {
        return Err(Error::configuration(format!(
            "Directory traversal detected: {:?} is outside of base directory {:?}",
            dir, base_dir
        )));
    }

    let entries = fs::read_dir(dir).map_err(|e| Error::file_system(dir, "read directory", e))?;

    for entry in entries {
        let entry = entry.map_err(|e| Error::file_system(dir, "read directory entry", e))?;
        let path = entry.path();

        if path.is_file() {
            // Get relative path for matching
            let relative = path.strip_prefix(base_dir).unwrap_or(&path);
            if globset.is_match(relative) {
                files.push(path);
            }
        } else if path.is_dir() {
            // Skip symlinks to prevent traversal
            if !entry
                .metadata()
                .map_err(|e| Error::file_system(&path, "get metadata", e))?
                .file_type()
                .is_symlink()
            {
                walk_directory(&path, base_dir, globset, files)?;
            }
        }
    }

    Ok(())
}

/// Calculate SHA256 hash of a file
fn hash_file(file_path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let content = fs::read(file_path)
        .map_err(|e| Error::file_system(file_path, "read file for hashing", e))?;

    let mut hasher = Sha256::new();
    hasher.update(&content);
    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

/// Format bytes in human-readable format
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    format!("{:.2} {}", size, UNITS[unit_index])
}

/// Format duration in human-readable format
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();

    if secs < 60 {
        format!("{} seconds", secs)
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else if secs < 86400 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

impl Default for CacheManager {
    fn default() -> Self {
        Self::new().expect("Failed to create default cache manager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_task_config() -> TaskConfig {
        TaskConfig {
            description: Some("Test task".to_string()),
            command: Some("echo hello".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: Some(vec!["src/**/*.rs".to_string()]),
            outputs: Some(vec!["build/**/*.o".to_string()]),
            cache: Some(true),
            cache_key: None,
            timeout: None,
            security: None,
        }
    }

    #[test]
    fn test_cache_manager_creation() {
        let _manager = CacheManager::new().unwrap();
    }

    #[test]
    fn test_cache_key_generation_with_globset() {
        let manager = CacheManager::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let task_config = create_test_task_config();

        // Create nested source files
        let src_dir = temp_dir.path().join("src");
        let sub_dir = src_dir.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
        fs::write(sub_dir.join("lib.rs"), "pub fn test() {}").unwrap();
        fs::write(src_dir.join("data.txt"), "not a rust file").unwrap();

        let cache_key = manager
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        assert!(!cache_key.is_empty());

        // Should only hash .rs files
        // Verify by modifying non-rs file - key should not change
        let cache_key2 = manager
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();
        assert_eq!(cache_key, cache_key2);

        fs::write(src_dir.join("data.txt"), "modified").unwrap();
        let cache_key3 = manager
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();
        assert_eq!(cache_key, cache_key3); // Should not change

        // Modify .rs file - key should change
        fs::write(src_dir.join("main.rs"), "fn main() { println!(\"hi\"); }").unwrap();
        let cache_key4 = manager
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();
        assert_ne!(cache_key, cache_key4);
    }

    #[test]
    fn test_concurrent_cache_access() {
        let manager = Arc::new(CacheManager::new().unwrap());
        let temp_dir = Arc::new(TempDir::new().unwrap());
        let task_config = Arc::new(create_test_task_config());

        // Create test files
        let build_dir = temp_dir.path().join("build");
        fs::create_dir_all(&build_dir).unwrap();

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let manager = Arc::clone(&manager);
                let temp_dir = Arc::clone(&temp_dir);
                let task_config = Arc::clone(&task_config);
                let build_dir = build_dir.clone();

                std::thread::spawn(move || {
                    let cache_key = format!("test_key_{}", i % 3); // Share some keys

                    // Create output file
                    let output_file = build_dir.join(format!("output_{}.o", i));
                    fs::write(&output_file, format!("content {}", i)).unwrap();

                    // Save to cache
                    manager
                        .save_result(&cache_key, &task_config, temp_dir.path(), 0)
                        .unwrap();

                    // Read from cache
                    let result = manager
                        .get_cached_result(&cache_key, &task_config, temp_dir.path())
                        .unwrap();

                    assert!(result.is_some());
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Check statistics
        let stats = manager.get_statistics().unwrap();
        assert!(stats.writes > 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn test_cache_statistics() {
        let manager = CacheManager::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let task_config = create_test_task_config();

        // Initial stats should be zero
        let stats = manager.get_statistics().unwrap();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);

        // Cache miss
        let cache_key = "nonexistent";
        let result = manager
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();
        assert!(result.is_none());

        let stats = manager.get_statistics().unwrap();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 1);

        // Save and retrieve (cache hit)
        let build_dir = temp_dir.path().join("build");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(build_dir.join("test.o"), "content").unwrap();

        manager
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();

        let result = manager
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();
        assert!(result.is_some());

        let stats = manager.get_statistics().unwrap();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.writes, 1);
        assert!(stats.total_bytes_saved > 0);
    }

    #[test]
    fn test_cache_version_migration() {
        let temp_dir = TempDir::new().unwrap();

        // Set a custom cache directory for this test
        std::env::set_var("CUENV_CACHE_DIR", temp_dir.path());

        // Create manager - should write version file
        let manager1 = CacheManager::new().unwrap();
        drop(manager1);

        let version_file = temp_dir.path().join(".cache/cuenv/VERSION");
        assert!(version_file.exists());

        // Simulate old version
        fs::write(&version_file, "0").unwrap();

        // Create new manager - should trigger migration
        let manager2 = CacheManager::new().unwrap();

        // Version should be updated
        let content = fs::read_to_string(&version_file).unwrap();
        assert_eq!(content, CACHE_VERSION.to_string());

        // Clean up
        std::env::remove_var("CUENV_CACHE_DIR");
    }
}
