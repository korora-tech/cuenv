use crate::errors::{Error, Result};
use crate::xdg::XdgPaths;
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub struct DirectoryManager;

impl DirectoryManager {
    pub fn new() -> Self {
        Self
    }

    pub fn allow_directory(&self, dir: &Path) -> Result<()> {
        let allowed_file = self.get_allowed_file()?;

        // Ensure the directory exists
        if !dir.exists() {
            return Err(Error::file_system(
                dir.to_path_buf(),
                "access directory",
                std::io::Error::new(std::io::ErrorKind::NotFound, "Directory does not exist"),
            ));
        }

        // Get canonical path
        let canonical_dir = dir
            .canonicalize()
            .map_err(|e| Error::file_system(dir.to_path_buf(), "canonicalize path", e))?;

        // Check if already allowed
        if self.is_directory_allowed(&canonical_dir)? {
            return Ok(()); // Already allowed
        }

        // Calculate hash of env.cue if it exists
        let env_cue = canonical_dir.join("env.cue");
        let hash = if env_cue.exists() {
            self.calculate_file_hash(&env_cue)?
        } else {
            String::new()
        };

        // Append to allowed file with hash
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&allowed_file)
            .map_err(|e| Error::file_system(allowed_file.clone(), "open allowed file", e))?;

        if hash.is_empty() {
            writeln!(file, "{}", canonical_dir.display())
        } else {
            writeln!(file, "{}:{}", canonical_dir.display(), hash)
        }
        .map_err(|e| Error::file_system(allowed_file, "write to allowed file", e))?;

        Ok(())
    }

    pub fn deny_directory(&self, dir: &Path) -> Result<()> {
        let allowed_file = self.get_allowed_file()?;

        if !allowed_file.exists() {
            return Ok(()); // Nothing to deny
        }

        // Get canonical path
        let canonical_dir = dir
            .canonicalize()
            .map_err(|e| Error::file_system(dir.to_path_buf(), "canonicalize path", e))?;

        // Read all allowed directories
        let file = fs::File::open(&allowed_file)
            .map_err(|e| Error::file_system(allowed_file.clone(), "open allowed file", e))?;
        let reader = BufReader::new(file);

        let mut allowed_dirs: Vec<String> = Vec::new();
        for line in reader.lines() {
            let line =
                line.map_err(|e| Error::file_system(allowed_file.clone(), "read allowed file", e))?;
            let line = line.trim();
            if !line.is_empty() && line != canonical_dir.to_string_lossy() {
                allowed_dirs.push(line.to_string());
            }
        }

        // Write back the filtered list
        fs::write(&allowed_file, allowed_dirs.join("\n") + "\n")
            .map_err(|e| Error::file_system(allowed_file, "write allowed file", e))?;

        Ok(())
    }

    pub fn is_directory_allowed(&self, dir: &Path) -> Result<bool> {
        let allowed_file = self.get_allowed_file()?;

        if !allowed_file.exists() {
            return Ok(false);
        }

        // Get canonical path
        let canonical_dir = dir
            .canonicalize()
            .map_err(|e| Error::file_system(dir.to_path_buf(), "canonicalize path", e))?;

        // Read allowed directories
        let file = fs::File::open(&allowed_file)
            .map_err(|e| Error::file_system(allowed_file.clone(), "open allowed file", e))?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line =
                line.map_err(|e| Error::file_system(allowed_file.clone(), "read allowed file", e))?;
            let line = line.trim();

            // Parse line which can be either "path" or "path:hash"
            let (allowed_path, allowed_hash) = if let Some(colon_pos) = line.rfind(':') {
                (
                    line[..colon_pos].to_string(),
                    Some(line[colon_pos + 1..].to_string()),
                )
            } else {
                (line.to_string(), None)
            };

            if allowed_path == canonical_dir.to_string_lossy() {
                // Path matches, now check hash if present
                if let Some(expected_hash) = allowed_hash {
                    let env_cue = canonical_dir.join("env.cue");
                    if env_cue.exists() {
                        let actual_hash = self.calculate_file_hash(&env_cue)?;
                        return Ok(actual_hash == expected_hash);
                    } else {
                        // env.cue doesn't exist but hash was expected
                        return Ok(false);
                    }
                } else {
                    // No hash requirement, directory is allowed
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    fn get_allowed_file(&self) -> Result<PathBuf> {
        let allowed_file = XdgPaths::allowed_file();
        let data_dir = allowed_file
            .parent()
            .ok_or_else(|| Error::configuration("allowed file path has no parent directory"))?;

        // Create data directory if it doesn't exist
        if !data_dir.exists() {
            fs::create_dir_all(data_dir).map_err(|e| {
                Error::file_system(data_dir.to_path_buf(), "create data directory", e)
            })?;
        }

        Ok(allowed_file)
    }

    fn calculate_file_hash(&self, file_path: &Path) -> Result<String> {
        let mut file = fs::File::open(file_path)
            .map_err(|e| Error::file_system(file_path.to_path_buf(), "open file for hashing", e))?;

        let mut hasher = Sha256::new();
        let mut buffer = [0; 8192];

        loop {
            let n = file.read(&mut buffer).map_err(|e| {
                Error::file_system(file_path.to_path_buf(), "read file for hashing", e)
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        Ok(format!("{:x}", hasher.finalize()))
    }
}

impl Default for DirectoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectoryManager {
    pub fn get_current_directory() -> Result<PathBuf> {
        match env::current_dir() {
            Ok(dir) => Ok(dir),
            Err(e) => Err(Error::file_system(
                PathBuf::from("."),
                "get current directory",
                e,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_current_directory() -> Result<()> {
        let result = DirectoryManager::get_current_directory();
        assert!(result.is_ok());
        assert!(result?.exists());

        Ok(())
    }
}
