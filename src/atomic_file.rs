//! Atomic file operations to prevent corrupted cache files

use crate::errors::{Error, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

/// Write data to a file atomically by writing to a temporary file and renaming
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::configuration("Invalid file path: no parent directory".to_string())
    })?;

    // Ensure parent directory exists
    fs::create_dir_all(parent)
        .map_err(|e| Error::file_system(parent.to_path_buf(), "create parent directory", e))?;

    // Create temporary file in the same directory to ensure atomic rename
    let temp_name = format!(".{}.tmp", Uuid::new_v4());
    let temp_path = parent.join(&temp_name);

    // Write to temporary file
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|e| Error::file_system(&temp_path, "create temporary file", e))?;

        file.write_all(content)
            .map_err(|e| Error::file_system(&temp_path, "write to temporary file", e))?;

        file.sync_all()
            .map_err(|e| Error::file_system(&temp_path, "sync temporary file", e))?;

        Ok(())
    })();

    // If writing failed, clean up temp file
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
        return result;
    }

    // Atomic rename
    fs::rename(&temp_path, path).map_err(|e| {
        // Clean up on failure
        let _ = fs::remove_file(&temp_path);
        Error::file_system(path.to_path_buf(), "atomic rename", e)
    })?;

    Ok(())
}

/// Write string content to a file atomically
pub fn write_atomic_string(path: &Path, content: &str) -> Result<()> {
    write_atomic(path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_atomic_write() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // Write content atomically
        write_atomic_string(&file_path, "Hello, World!").unwrap();

        // Verify content
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("subdir").join("test.txt");

        // Write content atomically (should create subdir)
        write_atomic_string(&file_path, "Test").unwrap();

        // Verify content
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "Test");
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // Write initial content
        fs::write(&file_path, "Old content").unwrap();

        // Overwrite atomically
        write_atomic_string(&file_path, "New content").unwrap();

        // Verify new content
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "New content");
    }
}
