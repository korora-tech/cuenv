use crate::cue_parser::ParseResult;
use crate::retry::{retry_blocking, RetryConfig};
use crate::xdg::XdgPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedParseResult {
    pub result: ParseResult,
    pub mtime: SystemTime,
}

pub struct CueCache;

impl CueCache {
    /// Get cached parse result if it's still valid
    pub fn get(cue_file: &Path) -> Option<ParseResult> {
        let cache_file = XdgPaths::cache_file(&cue_file.to_path_buf());

        // Check if cache file exists
        if !cache_file.exists() {
            return None;
        }

        // Get modification time of source file
        let source_mtime = match fs::metadata(cue_file) {
            Ok(metadata) => match metadata.modified() {
                Ok(mtime) => mtime,
                Err(_) => return None,
            },
            Err(_) => return None,
        };

        // Read cache file with retry for transient failures
        let cache_content =
            match retry_blocking(RetryConfig::fast(), || fs::read_to_string(&cache_file)) {
                Ok(content) => content,
                Err(_) => return None,
            };

        // Deserialize cache
        let cached: CachedParseResult = match serde_json::from_str(&cache_content) {
            Ok(cached) => cached,
            Err(_) => return None,
        };

        // Check if cache is still valid
        if cached.mtime >= source_mtime {
            Some(cached.result)
        } else {
            None
        }
    }

    /// Save parse result to cache
    pub fn save(cue_file: &Path, result: &ParseResult) -> Result<(), std::io::Error> {
        let cache_file = XdgPaths::cache_file(&cue_file.to_path_buf());
        let cache_dir = cache_file.parent().unwrap();

        // Create cache directory if it doesn't exist with retry
        if !cache_dir.exists() {
            retry_blocking(RetryConfig::fast(), || fs::create_dir_all(cache_dir))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        // Get modification time of source file
        let source_mtime = fs::metadata(cue_file)?.modified()?;

        // Create cache entry
        let cached = CachedParseResult {
            result: result.clone(),
            mtime: source_mtime,
        };

        // Serialize and save with retry
        let cache_content = serde_json::to_string(&cached)?;
        retry_blocking(RetryConfig::fast(), || {
            fs::write(&cache_file, &cache_content)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(())
    }

    /// Clear all cached files
    pub fn clear() -> Result<(), std::io::Error> {
        let cache_dir = XdgPaths::cache_dir();
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue_parser::ParseResult;
    use tempfile::TempDir;

    #[test]
    fn test_cache_save_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let cue_file = temp_dir.path().join("test.cue");
        fs::write(&cue_file, "package env").unwrap();

        // Create a parse result
        let mut result = ParseResult::default();
        result
            .variables
            .insert("FOO".to_string(), "bar".to_string());

        // Save to cache
        CueCache::save(&cue_file, &result).unwrap();

        // Get from cache
        let cached = CueCache::get(&cue_file).unwrap();
        assert_eq!(cached.variables.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn test_cache_invalidation() {
        let temp_dir = TempDir::new().unwrap();
        let cue_file = temp_dir.path().join("test.cue");
        fs::write(&cue_file, "package env").unwrap();

        // Create and save a parse result
        let result = ParseResult::default();
        CueCache::save(&cue_file, &result).unwrap();

        // Modify the file (with a small delay to ensure different mtime)
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&cue_file, "package env\n// modified").unwrap();

        // Cache should be invalidated
        assert!(CueCache::get(&cue_file).is_none());
    }
}
