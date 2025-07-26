//! Moon-style caching system for cuenv tasks
//! 
//! This module provides a robust, battle-tested caching infrastructure
//! inspired by the moon build tool's cache system.

mod cache_engine;
mod cache_item;
mod cache_mode;
mod hash_engine;

pub use cache_engine::*;
pub use cache_item::*;
pub use cache_mode::*;
pub use hash_engine::*;

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