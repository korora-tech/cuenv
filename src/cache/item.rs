use crate::atomic_file::write_atomic_string;
use crate::cache::{get_cache_mode, resolve_cache_path};
use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A cached item that can be loaded from and saved to disk
pub struct CacheItem<T>
where
    T: Default + for<'de> Deserialize<'de> + Serialize,
{
    pub data: T,
    pub path: PathBuf,
}

impl<T> CacheItem<T>
where
    T: Default + for<'de> Deserialize<'de> + Serialize,
{
    /// Load a cache item from disk
    pub fn load<P: AsRef<Path>>(cache_dir: &Path, path: P) -> Result<CacheItem<T>> {
        let full_path = resolve_cache_path(cache_dir, path.as_ref());
        let mut data = T::default();

        if get_cache_mode().is_readable() {
            // Directly attempt to read without checking existence first (avoiding TOCTOU)
            match fs::read_to_string(&full_path) {
                Ok(content) => {
                    log::debug!("Cache hit, reading item from {:?}", full_path);

                    data = serde_json::from_str(&content).map_err(|e| Error::Json {
                        message: "Failed to parse cached item".to_string(),
                        source: e,
                    })?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    log::debug!("Cache miss, item does not exist at {:?}", full_path);
                }
                Err(e) => {
                    return Err(Error::file_system(full_path.clone(), "read cache item", e));
                }
            }
        } else {
            log::trace!("Cache is not readable, skipping checks for {:?}", full_path);
        }

        Ok(CacheItem {
            data,
            path: full_path,
        })
    }

    /// Save the cache item to disk
    pub fn save(&self) -> Result<()> {
        if get_cache_mode().is_writable() {
            log::debug!("Writing cache item to {:?}", self.path);

            let content = serde_json::to_string_pretty(&self.data).map_err(|e| Error::Json {
                message: "Failed to serialize cache item".to_string(),
                source: e,
            })?;

            // Use atomic write to prevent corruption
            write_atomic_string(&self.path, &content)?;
        } else {
            log::trace!("Cache is not writable, skipping save for {:?}", self.path);
        }

        Ok(())
    }

    /// Get the directory containing this cache item
    pub fn get_dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }
}
