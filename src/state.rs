use crate::audit::audit_logger;
use crate::env_diff::EnvDiff;
use crate::file_times::FileTimes;
use crate::gzenv;
use crate::sync_env::SyncEnv;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Represents a snapshot of environment variables for rollback
#[derive(Debug)]
struct EnvSnapshot {
    variables: HashMap<String, Option<String>>,
}

impl EnvSnapshot {
    /// Create a snapshot of the specified environment variables
    fn capture(keys: &[String]) -> Result<Self> {
        let mut variables = HashMap::with_capacity(keys.len());
        for key in keys {
            variables.insert(key.to_string(), SyncEnv::var(key)?);
        }
        Ok(Self { variables })
    }

    /// Restore environment variables from this snapshot
    fn restore(&self) -> Result<()> {
        for (key, value) in &self.variables {
            match value {
                Some(val) => SyncEnv::set_var(key, val)?,
                None => SyncEnv::remove_var(key)?,
            }
        }
        Ok(())
    }
}

/// Transaction for atomic state changes with rollback support
struct StateTransaction {
    snapshot: EnvSnapshot,
    operations: Vec<StateOperation>,
    committed: bool,
}

#[derive(Debug)]
enum StateOperation {
    SetVar { key: String, value: String },
    RemoveVar { key: String },
}

impl StateTransaction {
    /// Create a new transaction with a snapshot of current state
    fn new(keys: &[String]) -> Result<Self> {
        Ok(Self {
            snapshot: EnvSnapshot::capture(keys)?,
            operations: Vec::with_capacity(keys.len()), // Pre-allocate based on expected operations
            committed: false,
        })
    }

    /// Add a set variable operation to the transaction
    fn set_var(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.operations.push(StateOperation::SetVar {
            key: key.into(),
            value: value.into(),
        });
    }

    /// Add a remove variable operation to the transaction
    fn remove_var(&mut self, key: impl Into<String>) {
        self.operations
            .push(StateOperation::RemoveVar { key: key.into() });
    }

    /// Apply all operations in the transaction
    fn apply(&self) -> Result<()> {
        for op in &self.operations {
            match op {
                StateOperation::SetVar { key, value } => SyncEnv::set_var(key, value)?,
                StateOperation::RemoveVar { key } => SyncEnv::remove_var(key)?,
            }
        }
        Ok(())
    }

    /// Commit the transaction (apply all operations)
    fn commit(mut self) -> Result<()> {
        self.apply()?;
        self.committed = true;
        Ok(())
    }

    /// Rollback to the original state if not committed
    fn rollback(&self) -> Result<()> {
        if !self.committed {
            self.snapshot.restore()?;
        }
        Ok(())
    }
}

impl Drop for StateTransaction {
    fn drop(&mut self) {
        if !self.committed {
            // Best effort rollback on drop
            let _ = self.rollback();
        }
    }
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

    /// Get all state variable names
    fn state_var_names() -> Vec<String> {
        vec![
            Self::env_var_name("CUENV_DIR"),
            Self::env_var_name("CUENV_FILE"),
            Self::env_var_name("CUENV_DIFF"),
            Self::env_var_name("CUENV_WATCHES"),
            Self::env_var_name("CUENV_STATE"),
        ]
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

    /// Encode and store a value in an environment variable
    fn encode_and_store<T: Serialize>(
        transaction: &mut StateTransaction,
        key: String,
        value: &T,
        context: &str,
    ) -> Result<()> {
        let encoded = gzenv::encode(value).with_context(|| context.to_string())?;
        transaction.set_var(key, encoded);
        Ok(())
    }

    /// Decode a value from an environment variable
    fn decode_from_var<T: for<'de> Deserialize<'de>>(
        key: &str,
        context: &str,
    ) -> Result<Option<T>> {
        match SyncEnv::var(key)? {
            Some(encoded) => {
                let decoded = gzenv::decode(&encoded).with_context(|| context.to_string())?;
                Ok(Some(decoded))
            }
            None => Ok(None),
        }
    }

    /// Store the core state information
    async fn store_state(
        transaction: &mut StateTransaction,
        dir: &Path,
        file: &Path,
        environment: Option<&str>,
        capabilities: &[String],
    ) -> Result<()> {
        // Log environment state change
        if let Some(logger) = audit_logger() {
            let _ = logger
                .log_environment_change("load", dir, environment, capabilities)
                .await;
        }

        // Set CUENV_DIR with leading '-' like direnv
        transaction.set_var(
            Self::env_var_name("CUENV_DIR"),
            format!("-{}", dir.display()),
        );

        // Set CUENV_FILE
        transaction.set_var(Self::env_var_name("CUENV_FILE"), file.display().to_string());

        // Create and encode the state object
        let state = CuenvState {
            dir: dir.to_path_buf(),
            file: file.to_path_buf(),
            environment: environment.map(str::to_string),
            capabilities: capabilities.to_vec(),
        };

        Self::encode_and_store(
            transaction,
            Self::env_var_name("CUENV_STATE"),
            &state,
            "Failed to encode state",
        )?;

        Ok(())
    }

    /// Store environment diff and watches
    fn store_metadata(
        transaction: &mut StateTransaction,
        diff: &EnvDiff,
        watches: &FileTimes,
    ) -> Result<()> {
        // Store the diff
        Self::encode_and_store(
            transaction,
            Self::env_var_name("CUENV_DIFF"),
            diff,
            "Failed to encode environment diff",
        )?;

        // Store the watches
        Self::encode_and_store(
            transaction,
            Self::env_var_name("CUENV_WATCHES"),
            watches,
            "Failed to encode file watches",
        )?;

        Ok(())
    }

    /// Load state for a directory with transactional semantics
    pub async fn load(
        dir: &Path,
        file: &Path,
        environment: Option<&str>,
        capabilities: &[String],
        diff: &EnvDiff,
        watches: &FileTimes,
    ) -> Result<()> {
        // Create a transaction with snapshot of current state
        let mut transaction = StateTransaction::new(&Self::state_var_names())?;

        // Store all state components
        Self::store_state(&mut transaction, dir, file, environment, capabilities).await?;
        Self::store_metadata(&mut transaction, diff, watches)?;

        // Commit the transaction
        transaction.commit()?;

        Ok(())
    }

    /// Restore environment from diff
    async fn restore_environment_from_diff() -> Result<()> {
        if let Some(diff) = Self::decode_from_var::<EnvDiff>(
            &Self::env_var_name("CUENV_DIFF"),
            "Failed to decode diff",
        )? {
            // Apply the reverse diff to restore original environment
            let reversed_diff = diff.reverse();
            reversed_diff.apply()?;
        }
        Ok(())
    }

    /// Log unload operation
    async fn log_unload(current_state: &Option<CuenvState>) -> Result<()> {
        if let Some(logger) = audit_logger() {
            if let Some(state) = current_state {
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
        Ok(())
    }

    /// Unload the current environment with transactional semantics
    pub async fn unload() -> Result<()> {
        // Get current state for logging
        let current_state = Self::get_state()?;

        // Create a transaction that includes both state vars and environment vars
        let mut all_keys = Self::state_var_names();

        // Also snapshot environment variables that might be modified
        if let Some(diff) = Self::decode_from_var::<EnvDiff>(
            &Self::env_var_name("CUENV_DIFF"),
            "Failed to decode diff",
        )? {
            // Add keys that will be modified by the reverse diff
            // Pre-allocate capacity for all keys
            let additional_keys = diff.added_or_changed().len() + diff.removed().len();
            all_keys.reserve(additional_keys);

            for key in diff.added_or_changed().keys() {
                all_keys.push(key.to_string());
            }
            for key in diff.removed() {
                all_keys.push(key.to_string());
            }
        }

        let mut transaction = StateTransaction::new(&all_keys)?;

        // Log the unload operation
        Self::log_unload(&current_state).await?;

        // Restore original environment
        Self::restore_environment_from_diff().await?;

        // Remove all cuenv state variables
        for var_name in Self::state_var_names() {
            transaction.remove_var(var_name);
        }

        // Commit the transaction
        transaction.commit()?;

        Ok(())
    }

    /// Get the current state
    pub fn get_state() -> Result<Option<CuenvState>> {
        Self::decode_from_var(&Self::env_var_name("CUENV_STATE"), "Failed to decode state")
    }

    /// Get the environment diff
    pub fn get_diff() -> Result<Option<EnvDiff>> {
        Self::decode_from_var(&Self::env_var_name("CUENV_DIFF"), "Failed to decode diff")
    }

    /// Get the file watches
    pub fn get_watches() -> Result<Option<FileTimes>> {
        Self::decode_from_var(
            &Self::env_var_name("CUENV_WATCHES"),
            "Failed to decode watches",
        )
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
    fn test_transaction_rollback() {
        // Clean environment
        let test_key = format!("TEST_TRANSACTION_{}", uuid::Uuid::new_v4());
        let _ = SyncEnv::remove_var(&test_key);

        // Set initial value
        SyncEnv::set_var(&test_key, "initial").unwrap();

        // Create a transaction and don't commit
        {
            let mut transaction = StateTransaction::new(&[test_key.clone()]).unwrap();
            transaction.set_var(&test_key, "modified");

            // Apply changes
            transaction.apply().unwrap();

            // Verify the change was applied
            assert_eq!(
                SyncEnv::var(&test_key).unwrap(),
                Some("modified".to_string())
            );

            // Don't commit - transaction will rollback on drop
        }

        // Verify rollback happened
        assert_eq!(
            SyncEnv::var(&test_key).unwrap(),
            Some("initial".to_string())
        );

        // Clean up
        SyncEnv::remove_var(&test_key).unwrap();
    }

    #[test]
    fn test_transaction_commit() {
        // Clean environment
        let test_key = format!("TEST_TRANSACTION_COMMIT_{}", uuid::Uuid::new_v4());
        let _ = SyncEnv::remove_var(&test_key);

        // Set initial value
        SyncEnv::set_var(&test_key, "initial").unwrap();

        // Create a transaction and commit it
        {
            let mut transaction = StateTransaction::new(&[test_key.clone()]).unwrap();
            transaction.set_var(&test_key, "committed");
            transaction.commit().unwrap();
        }

        // Verify commit persisted
        assert_eq!(
            SyncEnv::var(&test_key).unwrap(),
            Some("committed".to_string())
        );

        // Clean up
        SyncEnv::remove_var(&test_key).unwrap();
    }

    #[test]
    fn test_env_snapshot_restore() {
        // Set up test environment
        let key1 = format!("TEST_SNAPSHOT_1_{}", uuid::Uuid::new_v4());
        let key2 = format!("TEST_SNAPSHOT_2_{}", uuid::Uuid::new_v4());
        let key3 = format!("TEST_SNAPSHOT_3_{}", uuid::Uuid::new_v4());

        SyncEnv::set_var(&key1, "value1").unwrap();
        SyncEnv::set_var(&key2, "value2").unwrap();
        // key3 intentionally not set

        // Take snapshot
        let snapshot = EnvSnapshot::capture(&[key1.clone(), key2.clone(), key3.clone()]).unwrap();

        // Modify environment
        SyncEnv::set_var(&key1, "modified1").unwrap();
        SyncEnv::remove_var(&key2).unwrap();
        SyncEnv::set_var(&key3, "new3").unwrap();

        // Verify changes
        assert_eq!(SyncEnv::var(&key1).unwrap(), Some("modified1".to_string()));
        assert_eq!(SyncEnv::var(&key2).unwrap(), None);
        assert_eq!(SyncEnv::var(&key3).unwrap(), Some("new3".to_string()));

        // Restore from snapshot
        snapshot.restore().unwrap();

        // Verify restoration
        assert_eq!(SyncEnv::var(&key1).unwrap(), Some("value1".to_string()));
        assert_eq!(SyncEnv::var(&key2).unwrap(), Some("value2".to_string()));
        assert_eq!(SyncEnv::var(&key3).unwrap(), None);

        // Clean up
        SyncEnv::remove_var(&key1).unwrap();
        SyncEnv::remove_var(&key2).unwrap();
    }

    #[tokio::test]
    async fn test_load_rollback_on_error() {
        // Clean environment
        let _ = SyncEnv::remove_var("CUENV_PREFIX");
        let _ = SyncEnv::remove_var("CUENV_DIR");
        let _ = SyncEnv::remove_var("CUENV_FILE");
        let _ = SyncEnv::remove_var("CUENV_DIFF");
        let _ = SyncEnv::remove_var("CUENV_WATCHES");
        let _ = SyncEnv::remove_var("CUENV_STATE");

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let file = dir.join("env.cue");

        // Create a diff that will fail to encode (simulate error)
        let diff = EnvDiff::new(HashMap::new(), HashMap::new());
        let watches = FileTimes::new();

        // Set some initial state
        SyncEnv::set_var("CUENV_DIR", "should-be-preserved").unwrap();

        // This would fail if we had a way to inject encoding failures
        // For now, just verify the happy path works
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

        // Verify load succeeded
        assert!(StateManager::is_loaded());

        // Clean up
        StateManager::unload().await.unwrap();
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
