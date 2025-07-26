//! Common types used across the cache implementation

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Represents a cached task execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTaskResult {
    /// Hash of the task configuration and inputs
    pub cache_key: String,
    /// Timestamp when task was executed
    pub executed_at: SystemTime,
    /// Exit code of the task
    pub exit_code: i32,
    /// Standard output (if captured)
    pub stdout: Option<Vec<u8>>,
    /// Standard error (if captured)
    pub stderr: Option<Vec<u8>>,
    /// Output files produced by the task
    pub output_files: HashMap<String, String>,
}
