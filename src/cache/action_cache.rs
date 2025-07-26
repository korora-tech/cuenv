//! Action cache implementation for deterministic task execution
//!
//! This module provides caching for task actions, including memoization
//! of results and integration with content-addressed storage.

use crate::cache::concurrent_cache::ConcurrentCache;
use crate::cache::content_addressed_store::ContentAddressedStore;
use crate::cue_parser::TaskConfig;
use crate::errors::{Error, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Result of a cached action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// Exit code of the action
    pub exit_code: i32,
    /// Stdout content hash (stored in CAS)
    pub stdout_hash: Option<String>,
    /// Stderr content hash (stored in CAS)
    pub stderr_hash: Option<String>,
    /// Output file hashes (path -> CAS hash)
    pub output_files: HashMap<String, String>,
    /// When this action was executed
    pub executed_at: SystemTime,
    /// Duration of execution in milliseconds
    pub duration_ms: u64,
}

/// Action digest computation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDigest {
    /// Hash of the action
    pub hash: String,
    /// Components that went into the hash
    pub components: ActionComponents,
}

/// Components that make up an action's identity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionComponents {
    /// Task name
    pub task_name: String,
    /// Command or script
    pub command: Option<String>,
    /// Working directory
    pub working_dir: PathBuf,
    /// Environment variables that affect the action
    pub env_vars: HashMap<String, String>,
    /// Input file hashes (path -> hash)
    pub input_files: HashMap<String, String>,
    /// Task configuration hash
    pub config_hash: String,
}

/// Action cache that integrates with CAS
pub struct ActionCache {
    /// Concurrent cache for action results
    result_cache: Arc<ConcurrentCache>,
    /// Content-addressed storage for outputs
    cas: Arc<ContentAddressedStore>,
    /// In-flight actions to prevent duplicate execution
    in_flight: Arc<DashMap<String, Arc<tokio::sync::Notify>>>,
}

impl ActionCache {
    /// Create a new action cache
    pub fn new(cas: Arc<ContentAddressedStore>, max_cache_size: u64) -> Self {
        Self {
            result_cache: Arc::new(ConcurrentCache::new(max_cache_size)),
            cas,
            in_flight: Arc::new(DashMap::new()),
        }
    }

    /// Compute action digest for a task
    pub async fn compute_digest(
        &self,
        task_name: &str,
        task_config: &TaskConfig,
        working_dir: &Path,
        env_vars: HashMap<String, String>,
    ) -> Result<ActionDigest> {
        let mut components = ActionComponents {
            task_name: task_name.to_string(),
            command: task_config.command.clone().or(task_config.script.clone()),
            working_dir: working_dir.to_path_buf(),
            env_vars,
            input_files: HashMap::new(),
            config_hash: hash_task_config(task_config)?,
        };

        // Hash input files
        if let Some(inputs) = &task_config.inputs {
            for pattern in inputs {
                let files = crate::cache::hash_engine::expand_glob_pattern(pattern, working_dir)?;
                for file in files {
                    let content = tokio::fs::read(&file).await.map_err(|e| {
                        Error::file_system(&file, "read input file for action digest", e)
                    })?;

                    let hash = compute_hash(&content);
                    let relative_path = file
                        .strip_prefix(working_dir)
                        .unwrap_or(&file)
                        .to_string_lossy()
                        .to_string();
                    components.input_files.insert(relative_path, hash);
                }
            }
        }

        // Compute final digest
        let digest_hash = compute_action_hash(&components)?;

        Ok(ActionDigest {
            hash: digest_hash,
            components,
        })
    }

    /// Check if an action result is cached
    pub async fn get_cached_result(&self, digest: &ActionDigest) -> Option<ActionResult> {
        // Check in-flight actions first
        if let Some(notify) = self.in_flight.get(&digest.hash) {
            // Wait for in-flight action to complete
            notify.notified().await;
            // Try again after notification
            return self.result_cache.get(&digest.hash).and_then(|cached| {
                serde_json::from_value(serde_json::to_value(cached).ok()?).ok()
            });
        }

        // Check cache
        self.result_cache
            .get(&digest.hash)
            .and_then(|cached| serde_json::from_value(serde_json::to_value(cached).ok()?).ok())
    }

    /// Execute an action with caching
    pub async fn execute_action<F, Fut>(
        &self,
        digest: &ActionDigest,
        execute_fn: F,
    ) -> Result<ActionResult>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<ActionResult>>,
    {
        // Check cache first
        if let Some(cached) = self.get_cached_result(digest).await {
            return Ok(cached);
        }

        // Mark as in-flight
        let notify = Arc::new(tokio::sync::Notify::new());
        let existing = self.in_flight.insert(digest.hash.clone(), notify.clone());

        if existing.is_some() {
            // Another task is already executing this action
            drop(existing);
            notify.notified().await;

            // Try to get from cache after notification
            if let Some(cached) = self.get_cached_result(digest).await {
                return Ok(cached);
            }

            // If still not in cache, something went wrong with the other execution
            return Err(Error::configuration(
                "Action execution completed but result not found in cache".to_string(),
            ));
        }

        // Execute the action
        let result = match execute_fn().await {
            Ok(mut result) => {
                // Store outputs in CAS
                result = self.store_outputs_in_cas(result).await?;
                result
            }
            Err(e) => {
                // Remove from in-flight and notify waiters
                self.in_flight.remove(&digest.hash);
                notify.notify_waiters();
                return Err(e);
            }
        };

        // Cache the result
        let cached_result = crate::cache::CachedTaskResult {
            cache_key: digest.hash.clone(),
            executed_at: result.executed_at,
            exit_code: result.exit_code,
            stdout: None,
            stderr: None,
            output_files: HashMap::new(), // Output files are stored in CAS by hash
        };

        self.result_cache
            .insert(digest.hash.clone(), cached_result)?;

        // Remove from in-flight and notify waiters
        self.in_flight.remove(&digest.hash);
        notify.notify_waiters();

        Ok(result)
    }

    /// Store action outputs in CAS
    async fn store_outputs_in_cas(&self, mut result: ActionResult) -> Result<ActionResult> {
        // Store stdout if present
        if let Some(stdout_content) = result.stdout_hash.as_ref() {
            let hash = self.cas.store(Cursor::new(stdout_content.as_bytes()))?;
            result.stdout_hash = Some(hash);
        }

        // Store stderr if present
        if let Some(stderr_content) = result.stderr_hash.as_ref() {
            let hash = self.cas.store(Cursor::new(stderr_content.as_bytes()))?;
            result.stderr_hash = Some(hash);
        }

        // Store output files
        let mut new_output_files = HashMap::new();
        for (path, content_hash) in &result.output_files {
            // In a real implementation, we'd read the file and store it
            // For now, we'll assume the hash is already computed
            new_output_files.insert(path.clone(), content_hash.clone());
        }
        result.output_files = new_output_files;

        Ok(result)
    }

    /// Get statistics
    pub fn stats(&self) -> crate::cache::concurrent_cache::CacheStatSnapshot {
        self.result_cache.stats()
    }

    /// Clear the cache
    pub fn clear(&self) {
        self.result_cache.clear();
        self.in_flight.clear();
    }
}

/// Compute hash of task configuration
fn hash_task_config(config: &TaskConfig) -> Result<String> {
    let serialized = serde_json::to_string(config).map_err(|e| Error::Json {
        message: "Failed to serialize task config for hashing".to_string(),
        source: e,
    })?;

    Ok(compute_hash(serialized.as_bytes()))
}

/// Compute hash of action components
fn compute_action_hash(components: &ActionComponents) -> Result<String> {
    let serialized = serde_json::to_string(components).map_err(|e| Error::Json {
        message: "Failed to serialize action components for hashing".to_string(),
        source: e,
    })?;

    Ok(compute_hash(serialized.as_bytes()))
}

/// Compute SHA256 hash
fn compute_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_action_digest_computation() {
        let temp_dir = TempDir::new().unwrap();
        let cas =
            Arc::new(ContentAddressedStore::new(temp_dir.path().to_path_buf(), 4096).unwrap());
        let cache = ActionCache::new(cas, 0);

        let task_config = TaskConfig {
            description: Some("Test task".to_string()),
            command: Some("echo hello".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: None,
            outputs: None,
            cache: Some(true),
            cache_key: None,
            timeout: None,
            security: None,
        };

        let digest = cache
            .compute_digest("test", &task_config, temp_dir.path(), HashMap::new())
            .await
            .unwrap();

        assert!(!digest.hash.is_empty());
        assert_eq!(digest.components.task_name, "test");
        assert_eq!(digest.components.command, Some("echo hello".to_string()));
    }

    #[tokio::test]
    async fn test_action_caching() {
        let temp_dir = TempDir::new().unwrap();
        let cas =
            Arc::new(ContentAddressedStore::new(temp_dir.path().to_path_buf(), 4096).unwrap());
        let cache = ActionCache::new(cas, 0);

        let task_config = TaskConfig {
            description: Some("Test task".to_string()),
            command: Some("echo hello".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: None,
            outputs: None,
            cache: Some(true),
            cache_key: None,
            timeout: None,
            security: None,
        };

        let digest = cache
            .compute_digest("test", &task_config, temp_dir.path(), HashMap::new())
            .await
            .unwrap();

        // Execute action
        let result = cache
            .execute_action(&digest, || async {
                Ok(ActionResult {
                    exit_code: 0,
                    stdout_hash: Some("hello\n".to_string()),
                    stderr_hash: None,
                    output_files: HashMap::new(),
                    executed_at: SystemTime::now(),
                    duration_ms: 10,
                })
            })
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);

        // Should be cached now
        let cached = cache.get_cached_result(&digest).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().exit_code, 0);

        // Stats should show hit
        let stats = cache.stats();
        assert_eq!(stats.writes, 1);
    }

    #[tokio::test]
    async fn test_concurrent_action_execution() {
        let temp_dir = TempDir::new().unwrap();
        let cas =
            Arc::new(ContentAddressedStore::new(temp_dir.path().to_path_buf(), 4096).unwrap());
        let cache = Arc::new(ActionCache::new(cas, 0));

        let task_config = TaskConfig {
            description: Some("Test task".to_string()),
            command: Some("echo hello".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: None,
            outputs: None,
            cache: Some(true),
            cache_key: None,
            timeout: None,
            security: None,
        };

        let digest = cache
            .compute_digest("test", &task_config, temp_dir.path(), HashMap::new())
            .await
            .unwrap();

        // Spawn multiple concurrent executions
        let mut handles = vec![];
        for i in 0..5 {
            let cache = cache.clone();
            let digest = digest.clone();
            let handle = tokio::spawn(async move {
                cache
                    .execute_action(&digest, || async move {
                        // Simulate some work
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        Ok(ActionResult {
                            exit_code: 0,
                            stdout_hash: Some(format!("hello from {}\n", i)),
                            stderr_hash: None,
                            output_files: HashMap::new(),
                            executed_at: SystemTime::now(),
                            duration_ms: 10,
                        })
                    })
                    .await
            });
            handles.push(handle);
        }

        // Wait for all to complete
        let results: Vec<_> = futures::future::join_all(handles).await;

        // All should succeed
        for result in results {
            assert!(result.is_ok());
            assert_eq!(result.unwrap().unwrap().exit_code, 0);
        }

        // Only one execution should have happened
        let stats = cache.stats();
        assert_eq!(stats.writes, 1);
    }
}
