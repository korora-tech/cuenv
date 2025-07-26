use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Content hasher for generating cache keys
#[derive(Debug)]
pub struct ContentHasher {
    /// Label for debugging purposes
    pub label: String,
    hasher: Sha256,
    /// Metadata about what was hashed
    pub manifest: HashManifest,
}

/// Manifest containing metadata about what was hashed
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HashManifest {
    pub label: String,
    pub inputs: Vec<String>,
    pub files: HashMap<String, String>,
}

impl ContentHasher {
    /// Create a new content hasher with a label
    pub fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            hasher: Sha256::new(),
            manifest: HashManifest {
                label: label.to_string(),
                inputs: Vec::new(),
                files: HashMap::new(),
            },
        }
    }

    /// Hash arbitrary content
    pub fn hash_content<T: Serialize>(&mut self, content: T) -> Result<()> {
        let serialized = serde_json::to_string(&content).map_err(|e| {
            Error::Json {
                message: "Failed to serialize content for hashing".to_string(),
                source: e,
            }
        })?;
        
        self.hasher.update(serialized.as_bytes());
        self.manifest.inputs.push(format!("content:{}", serialized.len()));
        
        Ok(())
    }

    /// Hash a file's content
    pub fn hash_file(&mut self, file_path: &Path) -> Result<()> {
        let content = fs::read(file_path).map_err(|e| {
            Error::file_system(file_path.to_path_buf(), "read file for hashing", e)
        })?;

        let file_hash = {
            let mut file_hasher = Sha256::new();
            file_hasher.update(&content);
            format!("{:x}", file_hasher.finalize())
        };

        self.hasher.update(&content);
        
        let path_str = file_path.to_string_lossy().to_string();
        self.manifest.files.insert(path_str.clone(), file_hash);
        self.manifest.inputs.push(format!("file:{}", path_str));

        Ok(())
    }

    /// Hash files matching a glob pattern
    pub fn hash_glob(&mut self, pattern: &str, base_dir: &Path) -> Result<()> {
        let files = self.expand_glob(pattern, base_dir)?;
        
        // Sort files for consistent hashing
        let mut sorted_files = files;
        sorted_files.sort();

        for file in sorted_files {
            self.hash_file(&file)?;
        }

        Ok(())
    }

    /// Generate the final hash
    pub fn generate_hash(&mut self) -> Result<String> {
        let result = self.hasher.finalize_reset();
        Ok(format!("{:x}", result))
    }

    /// Serialize the manifest for storage
    pub fn serialize(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.manifest).map_err(|e| {
            Error::Json {
                message: "Failed to serialize hash manifest".to_string(),
                source: e,
            }
        })
    }

    /// Expand a glob pattern to a list of files
    fn expand_glob(&self, pattern: &str, base_dir: &Path) -> Result<Vec<PathBuf>> {
        let full_pattern = base_dir.join(pattern);

        if full_pattern.is_file() {
            Ok(vec![full_pattern])
        } else if full_pattern.is_dir() {
            // If it's a directory, include all files recursively
            let mut files = Vec::new();
            self.collect_files_recursive(&full_pattern, &mut files)?;
            Ok(files)
        } else {
            // Simple wildcard matching
            let parent = full_pattern.parent().unwrap_or(base_dir);
            if !parent.exists() {
                return Ok(Vec::new());
            }

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
}

/// Hash engine for managing cache hash manifests
#[derive(Debug)]
pub struct HashEngine {
    /// Directory for storing hash manifests
    pub hashes_dir: PathBuf,
}

impl HashEngine {
    /// Create a new hash engine
    pub fn new(cache_dir: &Path) -> Result<HashEngine> {
        let hashes_dir = cache_dir.join("hashes");

        log::debug!("Creating hash engine with hashes_dir: {:?}", hashes_dir);

        fs::create_dir_all(&hashes_dir).map_err(|e| {
            Error::file_system(hashes_dir.clone(), "create hashes directory", e)
        })?;

        Ok(HashEngine { hashes_dir })
    }

    /// Create a new content hasher
    pub fn create_hasher(&self, label: &str) -> ContentHasher {
        ContentHasher::new(label)
    }

    /// Get the path for a hash manifest
    pub fn get_manifest_path(&self, hash: &str) -> PathBuf {
        self.hashes_dir.join(format!("{}.json", hash))
    }

    /// Save a hash manifest to disk
    pub fn save_manifest(&self, hasher: &ContentHasher, hash: &str) -> Result<()> {
        let path = self.get_manifest_path(hash);
        
        log::debug!("Saving hash manifest for '{}' to {:?}", hasher.label, path);

        let data = hasher.serialize()?;
        fs::write(&path, data).map_err(|e| {
            Error::file_system(path, "write hash manifest", e)
        })?;

        Ok(())
    }
}