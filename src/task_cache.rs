use crate::cache::CacheEngine;
use crate::cue_parser::TaskConfig;
use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Represents a cached task execution result
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedTaskResult {
    /// Hash of the task configuration and inputs
    pub cache_key: String,
    /// Timestamp when task was executed
    pub executed_at: SystemTime,
    /// Exit code of the task
    pub exit_code: i32,
    /// Hash of output files (if any)
    pub output_hashes: HashMap<String, String>,
}

impl Default for CachedTaskResult {
    fn default() -> Self {
        Self {
            cache_key: String::new(),
            executed_at: SystemTime::UNIX_EPOCH,
            exit_code: 0,
            output_hashes: HashMap::new(),
        }
    }
}

/// Task cache manager using moon-style caching infrastructure
pub struct TaskCache {
    cache_engine: CacheEngine,
}

impl TaskCache {
    /// Create a new task cache instance
    pub fn new() -> Result<Self> {
        let cache_engine = CacheEngine::new()?;
        Ok(Self { cache_engine })
    }

    /// Generate a cache key for a task based on its configuration and inputs
    pub fn generate_cache_key(
        &self,
        task_name: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
    ) -> Result<String> {
        let mut hasher = self.cache_engine.hash.create_hasher(&format!("task:{}", task_name));

        // Use custom cache key if provided
        if let Some(custom_key) = &task_config.cache_key {
            hasher.hash_content(custom_key)?;
        } else {
            // Hash task name and configuration
            hasher.hash_content(task_name)?;
            hasher.hash_content(task_config)?;
        }

        // Hash input files if specified
        if let Some(inputs) = &task_config.inputs {
            for input_pattern in inputs {
                hasher.hash_glob(input_pattern, working_dir)?;
            }
        }

        let hash = hasher.generate_hash()?;
        
        // Save the hash manifest
        self.cache_engine.hash.save_manifest(&hasher, &hash)?;
        
        Ok(hash)
    }

    /// Check if a task result is cached and still valid
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
        if !self.cache_engine.is_readable() {
            return Ok(None);
        }

        let cache_item = self.cache_engine.cache::<CachedTaskResult>(
            format!("tasks/{}", cache_key)
        )?;

        if cache_item.path.exists() {
            let cached_result = cache_item.data;
            
            // Verify cache key matches
            if cached_result.cache_key != cache_key {
                return Ok(None);
            }

            // Verify output files still exist and have correct hashes
            if let Some(outputs) = &task_config.outputs {
                for output_pattern in outputs {
                    let output_files = self.expand_glob(output_pattern, working_dir)?;
                    
                    // Check if we have any expected outputs from cache but no actual files
                    if output_files.is_empty() && !cached_result.output_hashes.is_empty() {
                        log::debug!("Cache miss: expected output files for pattern '{}' but found none", output_pattern);
                        return Ok(None);
                    }
                    
                    for output_file in output_files {
                        let relative_path = output_file
                            .strip_prefix(working_dir)
                            .unwrap_or(&output_file)
                            .to_string_lossy()
                            .to_string();

                        // Check if output file exists
                        if !output_file.exists() {
                            log::debug!("Cache miss: output file {:?} no longer exists", output_file);
                            return Ok(None);
                        }

                        // Check if output file hash matches
                        if let Some(expected_hash) = cached_result.output_hashes.get(&relative_path) {
                            let current_hash = self.hash_file(&output_file)?;
                            if &current_hash != expected_hash {
                                log::debug!(
                                    "Cache miss: output file {:?} hash changed",
                                    output_file
                                );
                                return Ok(None);
                            }
                        }
                    }
                }
                
                // Additional check: verify all cached output files still exist
                for (cached_path, _) in &cached_result.output_hashes {
                    let full_path = working_dir.join(cached_path);
                    if !full_path.exists() {
                        log::debug!("Cache miss: cached output file {:?} no longer exists", full_path);
                        return Ok(None);
                    }
                }
            }

            Ok(Some(cached_result))
        } else {
            Ok(None)
        }
    }

    /// Save a task execution result to cache
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
        if !self.cache_engine.is_writable() {
            return Ok(());
        }

        // Only cache successful executions
        if exit_code != 0 {
            return Ok(());
        }

        // Hash output files
        let mut output_hashes = HashMap::new();
        if let Some(outputs) = &task_config.outputs {
            for output_pattern in outputs {
                let output_files = self.expand_glob(output_pattern, working_dir)?;
                for output_file in output_files {
                    let relative_path = output_file
                        .strip_prefix(working_dir)
                        .unwrap_or(&output_file)
                        .to_string_lossy()
                        .to_string();
                    let file_hash = self.hash_file(&output_file)?;
                    output_hashes.insert(relative_path, file_hash);
                }
            }
        }

        let cached_result = CachedTaskResult {
            cache_key: cache_key.to_string(),
            executed_at: SystemTime::now(),
            exit_code,
            output_hashes,
        };

        let mut cache_item = self.cache_engine.cache::<CachedTaskResult>(
            format!("tasks/{}", cache_key)
        )?;
        cache_item.data = cached_result;
        cache_item.save()?;

        Ok(())
    }

    /// Clear all cached task results
    pub fn clear(&self) -> Result<()> {
        self.cache_engine.clear()
    }

    /// Expand a glob pattern to a list of files
    fn expand_glob(&self, pattern: &str, working_dir: &Path) -> Result<Vec<PathBuf>> {
        let full_pattern = working_dir.join(pattern);

        if full_pattern.is_file() {
            Ok(vec![full_pattern])
        } else if full_pattern.is_dir() {
            let mut files = Vec::new();
            self.collect_files_recursive(&full_pattern, &mut files)?;
            Ok(files)
        } else {
            let parent = full_pattern.parent().unwrap_or(working_dir);
            if parent.exists() {
                let mut files = Vec::new();
                let entries = fs::read_dir(parent).map_err(|e| {
                    Error::file_system(parent.to_path_buf(), "read directory", e)
                })?;

                for entry in entries {
                    let entry = entry.map_err(|e| {
                        Error::file_system(parent.to_path_buf(), "read directory entry", e)
                    })?;
                    let path = entry.path();
                    
                    if path.is_file() {
                        if let Some(filename) = path.file_name() {
                            let pattern_name = full_pattern
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy();
                            if pattern_name.contains('*') {
                                let pattern_prefix = pattern_name.trim_end_matches('*');
                                if filename.to_string_lossy().starts_with(pattern_prefix) {
                                    files.push(path);
                                }
                            } else if filename == full_pattern.file_name().unwrap_or_default() {
                                files.push(path);
                            }
                        }
                    }
                }
                Ok(files)
            } else {
                Ok(Vec::new())
            }
        }
    }

    /// Recursively collect all files in a directory
    fn collect_files_recursive(&self, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let entries = fs::read_dir(dir).map_err(|e| {
            Error::file_system(dir.to_path_buf(), "read directory", e)
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                Error::file_system(dir.to_path_buf(), "read directory entry", e)
            })?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                self.collect_files_recursive(&path, files)?;
            }
        }
        Ok(())
    }

    /// Calculate SHA256 hash of a file
    fn hash_file(&self, file_path: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        
        let content = fs::read(file_path).map_err(|e| {
            Error::file_system(file_path.to_path_buf(), "read file for hashing", e)
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let result = hasher.finalize();
        Ok(format!("{:x}", result))
    }
}

impl Default for TaskCache {
    fn default() -> Self {
        Self::new().expect("Failed to create default task cache")
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
            inputs: Some(vec!["src/*".to_string()]),
            outputs: Some(vec!["build/output.txt".to_string()]),
            cache: Some(true),
            cache_key: None,
        }
    }

    #[test]
    fn test_cache_key_generation() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let task_config = create_test_task_config();

        // Create some input files
        let src_dir = temp_dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("file1.txt"), "content1").unwrap();

        let cache_key1 = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Same config should produce same key
        let cache_key2 = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();
        assert_eq!(cache_key1, cache_key2);

        // Different task name should produce different key
        let cache_key3 = cache
            .generate_cache_key("different_task", &task_config, temp_dir.path())
            .unwrap();
        assert_ne!(cache_key1, cache_key3);
    }

    #[test]
    fn test_cache_invalidation_on_input_change() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let task_config = create_test_task_config();

        // Create initial input file
        let src_dir = temp_dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("file1.txt"), "content1").unwrap();

        let cache_key1 = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Modify input file
        fs::write(src_dir.join("file1.txt"), "modified content").unwrap();

        let cache_key2 = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Cache keys should be different
        assert_ne!(cache_key1, cache_key2);
    }

    #[test]
    fn test_custom_cache_key() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let mut task_config = create_test_task_config();
        task_config.cache_key = Some("custom_key_123".to_string());

        let cache_key = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Should use custom key in hash
        assert!(!cache_key.is_empty());
    }

    #[test]
    fn test_save_and_retrieve_cache() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let task_config = create_test_task_config();

        // Create test files
        let src_dir = temp_dir.path().join("src");
        let build_dir = temp_dir.path().join("build");
        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&build_dir).unwrap();
        fs::write(src_dir.join("file1.txt"), "content1").unwrap();
        fs::write(build_dir.join("output.txt"), "build output").unwrap();

        let cache_key = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Save result to cache
        cache
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();

        // Retrieve from cache
        let cached_result = cache
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();

        assert!(cached_result.is_some());
        let result = cached_result.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(!result.output_hashes.is_empty());
    }

    #[test]
    fn test_cache_miss_on_missing_output() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let task_config = create_test_task_config();

        // Create test files
        let src_dir = temp_dir.path().join("src");
        let build_dir = temp_dir.path().join("build");
        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&build_dir).unwrap();
        fs::write(src_dir.join("file1.txt"), "content1").unwrap();
        fs::write(build_dir.join("output.txt"), "build output").unwrap();

        let cache_key = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Save result to cache
        cache
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();

        // Remove output file
        fs::remove_file(build_dir.join("output.txt")).unwrap();

        // Should be cache miss
        let cached_result = cache
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();

        assert!(cached_result.is_none());
    }

    #[test]
    fn test_disabled_cache() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let mut task_config = create_test_task_config();
        task_config.cache = Some(false);

        let cache_key = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Should not save when cache is disabled
        cache
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();

        // Should return None when cache is disabled
        let cached_result = cache
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();

        assert!(cached_result.is_none());
    }

    #[test]
    fn test_failed_task_not_cached() {
        let temp_dir = TempDir::new().unwrap();
        let cache = TaskCache::new().unwrap();
        let task_config = create_test_task_config();

        let cache_key = cache
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        // Save failed result (exit code 1)
        cache
            .save_result(&cache_key, &task_config, temp_dir.path(), 1)
            .unwrap();

        // Should not be cached
        let cached_result = cache
            .get_cached_result(&cache_key, &task_config, temp_dir.path())
            .unwrap();

        assert!(cached_result.is_none());
    }
}