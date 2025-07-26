use crate::audit::audit_logger;
use crate::env_diff::EnvDiff;
use crate::file_times::FileTimes;
use crate::gzenv;
use crate::sync_env::SyncEnv;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// State information stored in environment variables
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuenvState {
    /// The directory containing the loaded env.cue file
    pub dir: PathBuf,
    /// The path to the loaded env.cue file
    pub file: PathBuf,
    /// The environment being used (e.g., "dev", "staging", "production")
    pub environment: Option<String>,
    /// The capabilities that were loaded
    pub capabilities: Vec<String>,
}

/// Manage cuenv state through environment variables
pub struct StateManager;

impl StateManager {
    /// Get the environment variable name with optional prefix
    pub(crate) fn env_var_name(base: &str) -> String {
        // Use std::env::var here to avoid recursive locking issues
        if let Ok(prefix) = std::env::var("CUENV_PREFIX") {
            format!("{prefix}_{base}")
        } else {
            base.to_string()
        }
    }

    /// Check if an environment is currently loaded
    pub fn is_loaded() -> bool {
        SyncEnv::var(Self::env_var_name("CUENV_DIR"))
            .unwrap_or_default()
            .is_some()
    }

    /// Get the currently loaded directory
    pub fn current_dir() -> Option<PathBuf> {
        SyncEnv::var(Self::env_var_name("CUENV_DIR"))
            .unwrap_or_default()
            .and_then(|dir| {
                // Remove the leading '-' that direnv uses
                dir.strip_prefix('-')
                    .map(PathBuf::from)
                    .or_else(|| Some(PathBuf::from(dir)))
            })
    }

    /// Load state for a directory
    pub async fn load(
        dir: &Path,
        file: &Path,
        environment: Option<&str>,
        capabilities: &[String],
        diff: &EnvDiff,
        watches: &FileTimes,
    ) -> Result<()> {
        // Log environment state change
        if let Some(logger) = audit_logger() {
            let _ = logger
                .log_environment_change("load", dir, environment, capabilities)
                .await;
        }

        // Set CUENV_DIR with leading '-' like direnv
        SyncEnv::set_var(
            Self::env_var_name("CUENV_DIR"),
            format!("-{}", dir.display()),
        )?;

        // Set CUENV_FILE
        SyncEnv::set_var(Self::env_var_name("CUENV_FILE"), file.display().to_string())?;

        // Set CUENV_DIFF
        let diff_encoded = gzenv::encode(diff).context("Failed to encode environment diff")?;
        SyncEnv::set_var(Self::env_var_name("CUENV_DIFF"), diff_encoded)?;

        // Set CUENV_WATCHES
        let watches_encoded = gzenv::encode(watches).context("Failed to encode file watches")?;
        SyncEnv::set_var(Self::env_var_name("CUENV_WATCHES"), watches_encoded)?;

        // Store additional state
        let state = CuenvState {
            dir: dir.to_path_buf(),
            file: file.to_path_buf(),
            environment: environment.map(|s| s.to_string()),
            capabilities: capabilities.to_vec(),
        };

        let state_encoded = gzenv::encode(&state).context("Failed to encode state")?;
        SyncEnv::set_var(Self::env_var_name("CUENV_STATE"), state_encoded)?;

        Ok(())
    }

    /// Unload the current environment
    pub async fn unload() -> Result<()> {
        // Get current state for logging
        let current_state = Self::get_state()?;

        // Log environment state change
        if let Some(logger) = audit_logger() {
            if let Some(state) = &current_state {
                let _ = logger
                    .log_environment_change(
                        "unload",
                        &state.dir,
                        state.environment.as_deref(),
                        &state.capabilities,
                    )
                    .await;
            }
        }

        // Get the diff to revert changes
        if let Some(diff_encoded) = SyncEnv::var(Self::env_var_name("CUENV_DIFF"))? {
            if let Ok(diff) = gzenv::decode::<EnvDiff>(&diff_encoded) {
                // Apply the reverse diff to restore original environment
                let reversed_diff = diff.reverse();
                reversed_diff.apply()?;
            }
        }

        // Remove cuenv state variables
        SyncEnv::remove_var(Self::env_var_name("CUENV_DIR"))?;
        SyncEnv::remove_var(Self::env_var_name("CUENV_FILE"))?;
        SyncEnv::remove_var(Self::env_var_name("CUENV_DIFF"))?;
        SyncEnv::remove_var(Self::env_var_name("CUENV_WATCHES"))?;
        SyncEnv::remove_var(Self::env_var_name("CUENV_STATE"))?;

        Ok(())
    }

    /// Get the current state
    pub fn get_state() -> Result<Option<CuenvState>> {
        match SyncEnv::var(Self::env_var_name("CUENV_STATE"))? {
            Some(encoded) => {
                let state = gzenv::decode(&encoded).context("Failed to decode state")?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    /// Get the environment diff
    pub fn get_diff() -> Result<Option<EnvDiff>> {
        match SyncEnv::var(Self::env_var_name("CUENV_DIFF"))? {
            Some(encoded) => {
                let diff = gzenv::decode(&encoded).context("Failed to decode diff")?;
                Ok(Some(diff))
            }
            None => Ok(None),
        }
    }

    /// Get the file watches
    pub fn get_watches() -> Result<Option<FileTimes>> {
        match SyncEnv::var(Self::env_var_name("CUENV_WATCHES"))? {
            Some(encoded) => {
                let watches = gzenv::decode(&encoded).context("Failed to decode watches")?;
                Ok(Some(watches))
            }
            None => Ok(None),
        }
    }

    /// Check if watched files have changed
    pub fn files_changed() -> bool {
        if let Ok(Some(watches)) = Self::get_watches() {
            watches.has_changed()
        } else {
            false
        }
    }

    /// Check if we should load environment for a directory
    pub fn should_load(dir: &Path) -> bool {
        match Self::current_dir() {
            Some(current) => current != dir,
            None => true,
        }
    }

    /// Check if we should unload when leaving a directory
    pub fn should_unload(current_dir: &Path) -> bool {
        if let Some(loaded_dir) = Self::current_dir() {
            // Unload if we're no longer in the loaded directory or its subdirectories
            !current_dir.starts_with(&loaded_dir)
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_state_management() {
        // Clean environment - remove any cuenv state variables
        let _ = SyncEnv::remove_var("CUENV_PREFIX");
        let _ = SyncEnv::remove_var("CUENV_DIR");
        let _ = SyncEnv::remove_var("CUENV_FILE");
        let _ = SyncEnv::remove_var("CUENV_DIFF");
        let _ = SyncEnv::remove_var("CUENV_WATCHES");
        let _ = SyncEnv::remove_var("CUENV_STATE");

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let file = dir.join("env.cue");

        // Initially not loaded
        assert!(!StateManager::is_loaded());
        assert!(StateManager::current_dir().is_none());

        // Create a diff
        let mut prev = HashMap::new();
        prev.insert("OLD_VAR".to_string(), "old".to_string());
        let mut next = HashMap::new();
        next.insert("NEW_VAR".to_string(), "new".to_string());
        let diff = EnvDiff::new(prev, next);

        // Create watches
        let watches = FileTimes::new();

        // Load state
        StateManager::load(
            dir,
            &file,
            Some("dev"),
            &["cap1".to_string()],
            &diff,
            &watches,
        )
        .await
        .unwrap();

        // Check loaded state

        assert!(StateManager::is_loaded());
        assert_eq!(StateManager::current_dir(), Some(dir.to_path_buf()));

        // Get state
        let state = StateManager::get_state().unwrap().unwrap();
        assert_eq!(state.dir, dir);
        assert_eq!(state.file, file);
        assert_eq!(state.environment, Some("dev".to_string()));
        assert_eq!(state.capabilities, vec!["cap1".to_string()]);

        // Check diff
        let loaded_diff = StateManager::get_diff().unwrap().unwrap();
        assert_eq!(loaded_diff, diff);

        // Unload
        StateManager::unload().await.unwrap();
        assert!(!StateManager::is_loaded());
    }

    #[test]
    fn test_should_load_unload() {
        // Clean environment
        let _ = SyncEnv::remove_var("CUENV_PREFIX");
        let _ = SyncEnv::remove_var("CUENV_DIR");
        let _ = SyncEnv::remove_var("CUENV_FILE");
        let _ = SyncEnv::remove_var("CUENV_DIFF");
        let _ = SyncEnv::remove_var("CUENV_WATCHES");
        let _ = SyncEnv::remove_var("CUENV_STATE");

        let temp_dir = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();
        let root = temp_dir.path();
        let subdir = root.join("subdir");
        let other = temp_dir2.path(); // Use a completely different temp directory

        // Create subdirectory
        fs::create_dir_all(&subdir).unwrap();

        // Should load when nothing is loaded
        assert!(StateManager::should_load(root));

        // Simulate loading root
        SyncEnv::set_var(
            StateManager::env_var_name("CUENV_DIR"),
            format!("-{}", root.display()),
        )
        .unwrap();

        // Should not reload same directory
        assert!(!StateManager::should_load(root));

        // Should load different directory
        assert!(StateManager::should_load(&other));

        // Should not unload when in subdirectory
        assert!(!StateManager::should_unload(&subdir));

        // Should unload when leaving to different directory
        assert!(StateManager::should_unload(&other));

        // Clean up
        SyncEnv::remove_var("CUENV_DIR").unwrap();
    }
}
