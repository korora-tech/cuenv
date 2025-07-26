//! High-performance caching system for cuenv tasks
//!
//! This module provides a robust caching infrastructure with
//! content-addressed storage and concurrent access support.

mod engine;
mod hash_engine;
mod item;
mod mode;

// Advanced caching modules
mod action_cache;
mod concurrent_cache;
mod content_addressed_store;
mod types;

pub use engine::CacheEngine;
pub use hash_engine::{expand_glob_pattern, HashEngine};
pub use item::CacheItem;
pub use mode::{get_cache_mode, CacheMode};

// Export advanced caching components
pub use action_cache::{ActionCache, ActionDigest, ActionResult};
pub use concurrent_cache::{ConcurrentCache, ConcurrentCacheBuilder};
pub use content_addressed_store::{ContentAddressedStore, ObjectMetadata};
pub use types::CachedTaskResult;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve a cache path relative to the cache directory
pub(crate) fn resolve_cache_path(cache_dir: &Path, path: impl AsRef<OsStr>) -> PathBuf {
    let path = PathBuf::from(path.as_ref());

    let mut resolved = if path.is_absolute() {
        path
    } else {
        cache_dir.join(path)
    };

    resolved.set_extension("json");
    resolved
}
