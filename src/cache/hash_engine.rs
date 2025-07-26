use crate::atomic_file::write_atomic_string;
use crate::errors::{Error, Result};
use globset::{Glob, GlobSetBuilder};
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
        let serialized = serde_json::to_string(&content).map_err(|e| Error::Json {
            message: "Failed to serialize content for hashing".to_string(),
            source: e,
        })?;

        self.hasher.update(serialized.as_bytes());
        self.manifest
            .inputs
            .push(format!("content:{}", serialized.len()));

        Ok(())
    }

    /// Hash a file's content
    pub fn hash_file(&mut self, file_path: &Path) -> Result<()> {
        let content = fs::read(file_path)
            .map_err(|e| Error::file_system(file_path.to_path_buf(), "read file for hashing", e))?;

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
        let files = expand_glob_pattern(pattern, base_dir)?;

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
        serde_json::to_string_pretty(&self.manifest).map_err(|e| Error::Json {
            message: "Failed to serialize hash manifest".to_string(),
            source: e,
        })
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

        fs::create_dir_all(&hashes_dir)
            .map_err(|e| Error::file_system(hashes_dir.clone(), "create hashes directory", e))?;

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
        write_atomic_string(&path, &data)?;

        Ok(())
    }
}

/// Expand a glob pattern to find matching files
pub fn expand_glob_pattern(pattern: &str, base_dir: &Path) -> Result<Vec<PathBuf>> {
    // Check if it's a direct file path (no glob chars)
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        let full_path = base_dir.join(pattern);
        // Use metadata to avoid TOCTOU between exists() and is_file()/is_dir()
        match full_path.metadata() {
            Ok(metadata) => {
                if metadata.is_file() {
                    return Ok(vec![full_path]);
                } else if metadata.is_dir() {
                    // If it's a directory, include all files recursively
                    let mut files = Vec::new();
                    collect_files_recursive(&full_path, &mut files)?;
                    return Ok(files);
                }
            }
            Err(_) => {
                // Path doesn't exist or can't be accessed
                return Ok(Vec::new());
            }
        }
    }

    // Build glob pattern
    let glob = Glob::new(pattern)
        .map_err(|e| Error::configuration(format!("Invalid glob pattern '{}': {}", pattern, e)))?;

    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    let globset = builder
        .build()
        .map_err(|e| Error::configuration(format!("Failed to build globset: {}", e)))?;

    // Walk directory and find matches
    let mut files = Vec::new();
    walk_directory_for_glob(base_dir, base_dir, &globset, &mut files)?;

    Ok(files)
}

/// Recursively walk directory and collect files matching the globset
fn walk_directory_for_glob(
    dir: &Path,
    base_dir: &Path,
    globset: &globset::GlobSet,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    // Validate that dir is within base_dir to prevent traversal
    let canonical_dir = dir
        .canonicalize()
        .map_err(|e| Error::file_system(dir.to_path_buf(), "canonicalize directory", e))?;
    let canonical_base = base_dir.canonicalize().map_err(|e| {
        Error::file_system(base_dir.to_path_buf(), "canonicalize base directory", e)
    })?;

    if !canonical_dir.starts_with(&canonical_base) {
        return Err(Error::configuration(format!(
            "Directory traversal detected: {:?} is outside of base directory {:?}",
            dir, base_dir
        )));
    }

    let entries = fs::read_dir(dir)
        .map_err(|e| Error::file_system(dir.to_path_buf(), "read directory", e))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| Error::file_system(dir.to_path_buf(), "read directory entry", e))?;
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
                .map_err(|e| Error::file_system(path.clone(), "get metadata", e))?
                .file_type()
                .is_symlink()
            {
                walk_directory_for_glob(&path, base_dir, globset, files)?;
            }
        }
    }

    Ok(())
}

/// Recursively collect all files in a directory
fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|e| Error::file_system(dir.to_path_buf(), "read directory", e))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| Error::file_system(dir.to_path_buf(), "read directory entry", e))?;
        let path = entry.path();

        let metadata = entry
            .metadata()
            .map_err(|e| Error::file_system(path.clone(), "get metadata", e))?;

        if metadata.is_file() {
            files.push(path);
        } else if metadata.is_dir() && !metadata.file_type().is_symlink() {
            // Skip symlinks to prevent traversal
            collect_files_recursive(&path, files)?;
        }
    }
    Ok(())
}
