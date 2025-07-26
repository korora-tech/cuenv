use std::fmt;

/// Cache mode determines how the cache behaves
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum CacheMode {
    /// Caching is disabled
    Off,
    /// Cache can only be read from
    Read,
    /// Cache can be read from and written to (default)
    #[default]
    ReadWrite,
    /// Cache can only be written to
    Write,
}

impl From<String> for CacheMode {
    fn from(value: String) -> Self {
        match value.to_lowercase().as_str() {
            "off" => CacheMode::Off,
            "read" => CacheMode::Read,
            "read-write" => CacheMode::ReadWrite,
            "write" => CacheMode::Write,
            _ => {
                log::warn!(
                    "Unknown CUENV_CACHE environment variable value \"{}\", falling back to read-write mode",
                    value
                );
                CacheMode::ReadWrite
            }
        }
    }
}

impl fmt::Display for CacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode_str = match self {
            CacheMode::Off => "off",
            CacheMode::Read => "read",
            CacheMode::ReadWrite => "read-write",
            CacheMode::Write => "write",
        };
        write!(f, "{}", mode_str)
    }
}

impl CacheMode {
    /// Check if cache can be read from
    pub fn is_readable(&self) -> bool {
        matches!(self, CacheMode::Read | CacheMode::ReadWrite)
    }

    /// Check if cache is read-only
    pub fn is_read_only(&self) -> bool {
        matches!(self, CacheMode::Read)
    }

    /// Check if cache can be written to
    pub fn is_writable(&self) -> bool {
        matches!(self, CacheMode::Write | CacheMode::ReadWrite)
    }

    /// Check if cache is write-only
    pub fn is_write_only(&self) -> bool {
        matches!(self, CacheMode::Write)
    }
}

/// Get the current cache mode from environment variable
pub fn get_cache_mode() -> CacheMode {
    if let Ok(var) = std::env::var("CUENV_CACHE") {
        return CacheMode::from(var);
    }
    CacheMode::ReadWrite
}