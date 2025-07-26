//! Content-Addressed Storage (CAS) implementation
//!
//! This module provides a content-addressed storage system where files
//! are stored and retrieved by their content hash, ensuring deduplication
//! and integrity.

use crate::atomic_file::{write_atomic, write_atomic_string};
use crate::errors::{Error, Result};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// Metadata for a stored object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// SHA256 hash of the content
    pub hash: String,
    /// Size in bytes
    pub size: u64,
    /// When this object was first stored
    pub stored_at: SystemTime,
    /// Reference count (how many cache entries reference this)
    pub ref_count: u64,
    /// Whether this object is inlined in the metadata
    pub inlined: bool,
}

/// Content-Addressed Storage engine
pub struct ContentAddressedStore {
    /// Base directory for CAS
    base_dir: PathBuf,
    /// Directory for storing objects
    objects_dir: PathBuf,
    /// In-memory index of objects (hash -> metadata)
    index: Arc<DashMap<String, ObjectMetadata>>,
    /// Small object size threshold for inlining (bytes)
    inline_threshold: usize,
    /// Total bytes stored
    total_bytes: AtomicU64,
    /// Lock for index persistence
    index_lock: Arc<RwLock<()>>,
}

impl ContentAddressedStore {
    /// Create a new CAS instance
    pub fn new(base_dir: PathBuf, inline_threshold: usize) -> Result<Self> {
        let objects_dir = base_dir.join("objects");
        fs::create_dir_all(&objects_dir)
            .map_err(|e| Error::file_system(&objects_dir, "create CAS objects directory", e))?;

        let store = Self {
            base_dir: base_dir.clone(),
            objects_dir,
            index: Arc::new(DashMap::new()),
            inline_threshold,
            total_bytes: AtomicU64::new(0),
            index_lock: Arc::new(RwLock::new(())),
        };

        // Load existing index
        store.load_index()?;

        Ok(store)
    }

    /// Store content and return its hash
    pub fn store<R: Read>(&self, mut reader: R) -> Result<String> {
        // Read and hash content
        let mut hasher = Sha256::new();
        let mut content = Vec::new();
        let mut buffer = [0u8; 8192];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                    content.extend_from_slice(&buffer[..n]);
                }
                Err(e) => {
                    return Err(Error::FileSystem {
                        path: self.base_dir.clone(),
                        operation: "read content for CAS".to_string(),
                        source: e,
                    });
                }
            }
        }

        let hash = format!("{:x}", hasher.finalize());
        let size = content.len() as u64;

        // Check if already exists
        if let Some(mut entry) = self.index.get_mut(&hash) {
            entry.ref_count += 1;
            self.persist_index()?;
            return Ok(hash);
        }

        // Determine storage strategy
        let (inlined, _object_path) = if content.len() <= self.inline_threshold {
            // Inline small objects
            (true, None)
        } else {
            // Store large objects as files
            let object_path = self.get_object_path(&hash);
            if let Some(parent) = object_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::file_system(parent.to_path_buf(), "create CAS object directory", e)
                })?;
            }
            write_atomic(&object_path, &content)?;
            (false, Some(object_path))
        };

        // Create metadata
        let metadata = ObjectMetadata {
            hash: hash.clone(),
            size,
            stored_at: SystemTime::now(),
            ref_count: 1,
            inlined,
        };

        // Store inline content if applicable
        if inlined {
            let inline_path = self.get_inline_path(&hash);
            write_atomic(&inline_path, &content)?;
        }

        // Update index
        self.index.insert(hash.clone(), metadata);
        self.total_bytes.fetch_add(size, Ordering::Relaxed);
        self.persist_index()?;

        Ok(hash)
    }

    /// Retrieve content by hash
    pub fn retrieve(&self, hash: &str) -> Result<Vec<u8>> {
        let metadata = self
            .index
            .get(hash)
            .ok_or_else(|| Error::configuration(format!("Object not found in CAS: {}", hash)))?;

        if metadata.inlined {
            // Read from inline storage
            let inline_path = self.get_inline_path(hash);
            fs::read(&inline_path)
                .map_err(|e| Error::file_system(&inline_path, "read inlined CAS object", e))
        } else {
            // Read from object storage
            let object_path = self.get_object_path(hash);
            fs::read(&object_path)
                .map_err(|e| Error::file_system(&object_path, "read CAS object", e))
        }
    }

    /// Check if an object exists
    pub fn contains(&self, hash: &str) -> bool {
        self.index.contains_key(hash)
    }

    /// Get metadata for an object
    pub fn get_metadata(&self, hash: &str) -> Option<ObjectMetadata> {
        self.index.get(hash).map(|entry| entry.clone())
    }

    /// Decrease reference count and potentially remove object
    pub fn release(&self, hash: &str) -> Result<()> {
        let should_remove = {
            let mut entry = self.index.get_mut(hash).ok_or_else(|| {
                Error::configuration(format!("Object not found in CAS: {}", hash))
            })?;

            entry.ref_count = entry.ref_count.saturating_sub(1);
            entry.ref_count == 0
        };

        if should_remove {
            self.remove_object(hash)?;
        } else {
            self.persist_index()?;
        }

        Ok(())
    }

    /// Get total bytes stored
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Clean up unreferenced objects
    pub fn garbage_collect(&self) -> Result<(usize, u64)> {
        let mut removed_count = 0;
        let mut removed_bytes = 0u64;

        // Collect objects with zero references
        let zero_ref_objects: Vec<String> = self
            .index
            .iter()
            .filter(|entry| entry.value().ref_count == 0)
            .map(|entry| entry.key().clone())
            .collect();

        for hash in zero_ref_objects {
            if let Ok(()) = self.remove_object(&hash) {
                removed_count += 1;
                if let Some(metadata) = self.index.get(&hash) {
                    removed_bytes += metadata.size;
                }
            }
        }

        Ok((removed_count, removed_bytes))
    }

    /// Get path for an object file
    fn get_object_path(&self, hash: &str) -> PathBuf {
        // Use subdirectories to avoid too many files in one directory
        let (prefix, suffix) = hash.split_at(2);
        self.objects_dir.join(prefix).join(suffix)
    }

    /// Get path for an inline object
    fn get_inline_path(&self, hash: &str) -> PathBuf {
        self.base_dir.join("inline").join(hash)
    }

    /// Remove an object from storage
    fn remove_object(&self, hash: &str) -> Result<()> {
        if let Some((_, metadata)) = self.index.remove(hash) {
            self.total_bytes.fetch_sub(metadata.size, Ordering::Relaxed);

            if metadata.inlined {
                let inline_path = self.get_inline_path(hash);
                if inline_path.exists() {
                    fs::remove_file(&inline_path).map_err(|e| {
                        Error::file_system(&inline_path, "remove inline CAS object", e)
                    })?;
                }
            } else {
                let object_path = self.get_object_path(hash);
                if object_path.exists() {
                    fs::remove_file(&object_path)
                        .map_err(|e| Error::file_system(&object_path, "remove CAS object", e))?;
                }
            }
        }

        self.persist_index()?;
        Ok(())
    }

    /// Load index from disk
    fn load_index(&self) -> Result<()> {
        let index_path = self.base_dir.join("index.json");
        if !index_path.exists() {
            return Ok(());
        }

        let content = fs::read_to_string(&index_path)
            .map_err(|e| Error::file_system(&index_path, "read CAS index", e))?;

        let index_data: Vec<ObjectMetadata> =
            serde_json::from_str(&content).map_err(|e| Error::Json {
                message: "Failed to parse CAS index".to_string(),
                source: e,
            })?;

        let mut total_bytes = 0u64;
        for metadata in index_data {
            total_bytes += metadata.size;
            self.index.insert(metadata.hash.clone(), metadata);
        }

        self.total_bytes.store(total_bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Persist index to disk
    fn persist_index(&self) -> Result<()> {
        let _guard = self.index_lock.write();

        let index_data: Vec<ObjectMetadata> = self
            .index
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        let content = serde_json::to_string_pretty(&index_data).map_err(|e| Error::Json {
            message: "Failed to serialize CAS index".to_string(),
            source: e,
        })?;

        let index_path = self.base_dir.join("index.json");
        write_atomic_string(&index_path, &content)?;

        Ok(())
    }
}

/// Builder for ContentAddressedStore
pub struct CASBuilder {
    base_dir: Option<PathBuf>,
    inline_threshold: usize,
}

impl CASBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            base_dir: None,
            inline_threshold: 4096, // 4KB default
        }
    }

    /// Set base directory
    pub fn base_dir(mut self, dir: PathBuf) -> Self {
        self.base_dir = Some(dir);
        self
    }

    /// Set inline threshold
    pub fn inline_threshold(mut self, threshold: usize) -> Self {
        self.inline_threshold = threshold;
        self
    }

    /// Build the CAS
    pub fn build(self) -> Result<ContentAddressedStore> {
        let base_dir = self
            .base_dir
            .ok_or_else(|| Error::configuration("CAS base directory not specified".to_string()))?;

        ContentAddressedStore::new(base_dir, self.inline_threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    #[test]
    fn test_cas_store_retrieve() {
        let temp_dir = TempDir::new().unwrap();
        let cas = ContentAddressedStore::new(temp_dir.path().to_path_buf(), 100).unwrap();

        // Store content
        let content = b"Hello, Content-Addressed Storage!";
        let hash = cas.store(Cursor::new(content)).unwrap();

        // Retrieve content
        let retrieved = cas.retrieve(&hash).unwrap();
        assert_eq!(retrieved, content);

        // Check metadata
        let metadata = cas.get_metadata(&hash).unwrap();
        assert_eq!(metadata.size, content.len() as u64);
        assert_eq!(metadata.ref_count, 1);
    }

    #[test]
    fn test_cas_deduplication() {
        let temp_dir = TempDir::new().unwrap();
        let cas = ContentAddressedStore::new(temp_dir.path().to_path_buf(), 100).unwrap();

        // Store same content twice
        let content = b"Duplicate content";
        let hash1 = cas.store(Cursor::new(content)).unwrap();
        let hash2 = cas.store(Cursor::new(content)).unwrap();

        // Should get same hash
        assert_eq!(hash1, hash2);

        // Reference count should be 2
        let metadata = cas.get_metadata(&hash1).unwrap();
        assert_eq!(metadata.ref_count, 2);

        // Total bytes should only count once
        assert_eq!(cas.total_bytes(), content.len() as u64);
    }

    #[test]
    fn test_cas_inline_vs_file() {
        let temp_dir = TempDir::new().unwrap();
        let cas = ContentAddressedStore::new(temp_dir.path().to_path_buf(), 10).unwrap();

        // Small content (should be inlined)
        let small_content = b"Small";
        let small_hash = cas.store(Cursor::new(small_content)).unwrap();
        let small_metadata = cas.get_metadata(&small_hash).unwrap();
        assert!(small_metadata.inlined);

        // Large content (should be file)
        let large_content = b"This is a larger piece of content that exceeds the inline threshold";
        let large_hash = cas.store(Cursor::new(large_content)).unwrap();
        let large_metadata = cas.get_metadata(&large_hash).unwrap();
        assert!(!large_metadata.inlined);

        // Both should retrieve correctly
        assert_eq!(cas.retrieve(&small_hash).unwrap(), small_content);
        assert_eq!(cas.retrieve(&large_hash).unwrap(), large_content);
    }

    #[test]
    fn test_cas_garbage_collection() {
        let temp_dir = TempDir::new().unwrap();
        let cas = ContentAddressedStore::new(temp_dir.path().to_path_buf(), 100).unwrap();

        // Store and release content
        let content = b"Garbage collected content";
        let hash = cas.store(Cursor::new(content)).unwrap();

        // Release reference
        cas.release(&hash).unwrap();

        // Should be removed
        assert!(!cas.contains(&hash));

        // Garbage collection should report it
        let (removed_count, _removed_bytes) = cas.garbage_collect().unwrap();
        assert_eq!(removed_count, 0); // Already removed by release
        assert_eq!(cas.total_bytes(), 0);
    }
}
