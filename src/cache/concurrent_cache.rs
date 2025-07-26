//! Lock-free concurrent cache implementation
//!
//! This module provides a high-performance, lock-free cache implementation
//! using DashMap for concurrent access without explicit locking.

use crate::cache::CachedTaskResult;
use crate::errors::{Error, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Statistics for cache operations using atomic counters
#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub writes: AtomicU64,
    pub errors: AtomicU64,
    pub bytes_saved: AtomicU64,
}

impl CacheStats {
    /// Get a snapshot of current statistics
    pub fn snapshot(&self) -> CacheStatSnapshot {
        CacheStatSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            bytes_saved: self.bytes_saved.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of cache statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStatSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub errors: u64,
    pub bytes_saved: u64,
}

/// Entry in the concurrent cache
#[derive(Debug)]
struct CacheEntry {
    /// The cached task result
    result: CachedTaskResult,
    /// When this entry was last accessed
    last_accessed: AtomicU64,
    /// Size in bytes (for eviction policy)
    size_bytes: usize,
}

/// Lock-free concurrent cache using DashMap
pub struct ConcurrentCache {
    /// The actual cache storage
    cache: Arc<DashMap<String, CacheEntry>>,
    /// Statistics
    stats: Arc<CacheStats>,
    /// Maximum cache size in bytes (0 = unlimited)
    max_size_bytes: AtomicU64,
    /// Current cache size in bytes
    current_size_bytes: AtomicU64,
}

impl ConcurrentCache {
    /// Create a new concurrent cache
    pub fn new(max_size_bytes: u64) -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            stats: Arc::new(CacheStats::default()),
            max_size_bytes: AtomicU64::new(max_size_bytes),
            current_size_bytes: AtomicU64::new(0),
        }
    }

    /// Get a cached result
    pub fn get(&self, key: &str) -> Option<CachedTaskResult> {
        match self.cache.get(key) {
            Some(entry) => {
                // Update last accessed time
                entry.last_accessed.store(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    Ordering::Relaxed,
                );
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry.result.clone())
            }
            None => {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert a cached result
    pub fn insert(&self, key: String, result: CachedTaskResult) -> Result<()> {
        let serialized = serde_json::to_vec(&result).map_err(|e| Error::Json {
            message: "Failed to serialize cache entry".to_string(),
            source: e,
        })?;

        let size_bytes = serialized.len();

        // Check if we need to evict entries
        let max_size = self.max_size_bytes.load(Ordering::Relaxed);
        if max_size > 0 {
            self.maybe_evict_entries(size_bytes)?;
        }

        let entry = CacheEntry {
            result,
            last_accessed: AtomicU64::new(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
            size_bytes,
        };

        // Insert the entry
        self.cache.insert(key, entry);

        // Update statistics
        self.current_size_bytes
            .fetch_add(size_bytes as u64, Ordering::Relaxed);
        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_saved
            .fetch_add(size_bytes as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Remove a cached entry
    pub fn remove(&self, key: &str) -> Option<CachedTaskResult> {
        self.cache.remove(key).map(|(_, entry)| {
            self.current_size_bytes
                .fetch_sub(entry.size_bytes as u64, Ordering::Relaxed);
            entry.result
        })
    }

    /// Clear all entries
    pub fn clear(&self) {
        self.cache.clear();
        self.current_size_bytes.store(0, Ordering::Relaxed);
    }

    /// Get current statistics
    pub fn stats(&self) -> CacheStatSnapshot {
        self.stats.snapshot()
    }

    /// Evict entries if necessary using LRU policy
    fn maybe_evict_entries(&self, needed_bytes: usize) -> Result<()> {
        let max_size = self.max_size_bytes.load(Ordering::Relaxed);
        let current_size = self.current_size_bytes.load(Ordering::Relaxed);

        if max_size == 0 || current_size + needed_bytes as u64 <= max_size {
            return Ok(());
        }

        // Collect entries with their last accessed times
        let mut entries: Vec<(String, u64, usize)> = self
            .cache
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.value().last_accessed.load(Ordering::Relaxed),
                    entry.value().size_bytes,
                )
            })
            .collect();

        // Sort by last accessed time (oldest first)
        entries.sort_by_key(|e| e.1);

        // Evict entries until we have enough space
        let mut freed_bytes = 0u64;
        let needed_to_free = (current_size + needed_bytes as u64).saturating_sub(max_size);

        for (key, _, size) in entries {
            if freed_bytes >= needed_to_free {
                break;
            }

            if let Some(_) = self.cache.remove(&key) {
                freed_bytes += size as u64;
                self.current_size_bytes
                    .fetch_sub(size as u64, Ordering::Relaxed);
            }
        }

        Ok(())
    }

    /// Clean up entries older than the specified duration
    pub fn cleanup_stale(&self, max_age: Duration) -> (usize, u64) {
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(max_age.as_secs());

        let mut removed_count = 0;
        let mut removed_bytes = 0u64;

        // DashMap doesn't support retain with mutable access, so collect keys first
        let stale_keys: Vec<String> = self
            .cache
            .iter()
            .filter(|entry| entry.value().last_accessed.load(Ordering::Relaxed) < cutoff)
            .map(|entry| entry.key().clone())
            .collect();

        for key in stale_keys {
            if let Some((_, entry)) = self.cache.remove(&key) {
                removed_count += 1;
                removed_bytes += entry.size_bytes as u64;
                self.current_size_bytes
                    .fetch_sub(entry.size_bytes as u64, Ordering::Relaxed);
            }
        }

        (removed_count, removed_bytes)
    }
}

/// Builder for ConcurrentCache
pub struct ConcurrentCacheBuilder {
    max_size_bytes: u64,
}

impl ConcurrentCacheBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            max_size_bytes: 0, // Unlimited by default
        }
    }

    /// Set maximum cache size in bytes
    pub fn max_size_bytes(mut self, size: u64) -> Self {
        self.max_size_bytes = size;
        self
    }

    /// Build the cache
    pub fn build(self) -> ConcurrentCache {
        ConcurrentCache::new(self.max_size_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::thread;

    #[test]
    fn test_concurrent_cache_basic() {
        let cache = ConcurrentCache::new(0);

        let result = CachedTaskResult {
            cache_key: "test_key".to_string(),
            executed_at: SystemTime::now(),
            exit_code: 0,
            output_hashes: HashMap::new(),
        };

        // Insert
        cache
            .insert("test_key".to_string(), result.clone())
            .unwrap();

        // Get
        let retrieved = cache.get("test_key").unwrap();
        assert_eq!(retrieved.cache_key, "test_key");

        // Stats
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.writes, 1);
    }

    #[test]
    fn test_concurrent_access() {
        let cache = Arc::new(ConcurrentCache::new(0));
        let num_threads = 10;
        let operations_per_thread = 100;

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..operations_per_thread {
                        let key = format!("key_{}_{}", thread_id, i % 10);
                        let result = CachedTaskResult {
                            cache_key: key.clone(),
                            executed_at: SystemTime::now(),
                            exit_code: 0,
                            output_hashes: HashMap::new(),
                        };

                        // Write
                        cache.insert(key.clone(), result).unwrap();

                        // Read
                        cache.get(&key);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let stats = cache.stats();
        assert_eq!(stats.writes, (num_threads * operations_per_thread) as u64);
        assert!(stats.hits > 0);
    }

    #[test]
    fn test_eviction() {
        // Set small cache size to trigger eviction
        let cache = ConcurrentCache::new(1000); // 1KB limit

        // Insert entries until eviction occurs
        for i in 0..20 {
            let result = CachedTaskResult {
                cache_key: format!("key_{}", i),
                executed_at: SystemTime::now(),
                exit_code: 0,
                output_hashes: HashMap::from([
                    ("file1.txt".to_string(), "hash1".to_string()),
                    ("file2.txt".to_string(), "hash2".to_string()),
                ]),
            };
            cache.insert(format!("key_{}", i), result).unwrap();
        }

        // Cache should have evicted some entries
        let current_size = cache.current_size_bytes.load(Ordering::Relaxed);
        assert!(current_size <= 1000);
    }

    #[test]
    fn test_cleanup_stale() {
        let cache = ConcurrentCache::new(0);

        // Insert some entries
        for i in 0..5 {
            let result = CachedTaskResult {
                cache_key: format!("key_{}", i),
                executed_at: SystemTime::now() - Duration::from_secs(3600 * (i + 1) as u64),
                exit_code: 0,
                output_hashes: HashMap::new(),
            };
            cache.insert(format!("key_{}", i), result).unwrap();
        }

        // Clean up entries older than 2 hours
        let (removed_count, _) = cache.cleanup_stale(Duration::from_secs(7200));

        // Should have removed entries 2, 3, and 4 (3, 4, and 5 hours old)
        assert_eq!(removed_count, 3);
    }
}
