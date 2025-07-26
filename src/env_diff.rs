use crate::sync_env::SyncEnv;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Environment variables that should be ignored when computing diffs
const IGNORED_VARS: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "PS1",
    "PS2",
    "PS3",
    "PS4",
    "PROMPT_COMMAND",
    "BASH_REMATCH",
    "RANDOM",
    "LINENO",
    "SECONDS",
    "CUENV_DIR",
    "CUENV_FILE",
    "CUENV_WATCHES",
    "CUENV_DIFF",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvDiff {
    /// Environment variables before the change
    pub prev: HashMap<String, String>,
    /// Environment variables after the change
    pub next: HashMap<String, String>,
}

impl EnvDiff {
    /// Create a new environment diff
    pub fn new(prev: HashMap<String, String>, next: HashMap<String, String>) -> Self {
        Self { prev, next }
    }

    /// Create a diff from the current environment to a new environment
    pub fn from_current(next: HashMap<String, String>) -> Result<Self> {
        let current = SyncEnv::vars()?.into_iter().collect();
        Ok(Self::new(current, next))
    }

    /// Get the variables that were added or changed
    pub fn added_or_changed(&self) -> HashMap<&str, &str> {
        let mut result = HashMap::new();

        for (key, value) in &self.next {
            if IGNORED_VARS.contains(&key.as_str()) {
                continue;
            }

            match self.prev.get(key) {
                None => {
                    // Variable was added
                    result.insert(key.as_str(), value.as_str());
                }
                Some(prev_value) if prev_value != value => {
                    // Variable was changed
                    result.insert(key.as_str(), value.as_str());
                }
                _ => {}
            }
        }

        result
    }

    /// Get the variables that were removed
    pub fn removed(&self) -> HashSet<&str> {
        let mut result = HashSet::new();

        for key in self.prev.keys() {
            if IGNORED_VARS.contains(&key.as_str()) {
                continue;
            }

            if !self.next.contains_key(key) {
                result.insert(key.as_str());
            }
        }

        result
    }

    /// Apply this diff to the current environment
    pub fn apply(&self) -> Result<()> {
        // Remove variables that are in prev but not in next
        for key in self.removed() {
            SyncEnv::remove_var(key)?;
        }

        // Add or update variables
        for (key, value) in self.added_or_changed() {
            SyncEnv::set_var(key, value)?;
        }

        Ok(())
    }

    /// Reverse this diff (swap prev and next)
    pub fn reverse(&self) -> Self {
        Self {
            prev: self.next.clone(),
            next: self.prev.clone(),
        }
    }

    /// Check if this diff is empty (no changes)
    pub fn is_empty(&self) -> bool {
        self.added_or_changed().is_empty() && self.removed().is_empty()
    }

    /// Merge another diff into this one
    /// The resulting diff represents going from self.prev to other.next
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            prev: self.prev.clone(),
            next: other.next.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_added_or_changed() {
        let mut prev = HashMap::new();
        prev.insert("FOO".to_string(), "bar".to_string());
        prev.insert("EXISTING".to_string(), "value".to_string());

        let mut next = HashMap::new();
        next.insert("FOO".to_string(), "baz".to_string()); // changed
        next.insert("NEW".to_string(), "value".to_string()); // added
        next.insert("EXISTING".to_string(), "value".to_string()); // unchanged

        let diff = EnvDiff::new(prev, next);
        let changes = diff.added_or_changed();

        assert_eq!(changes.len(), 2);
        assert_eq!(changes.get("FOO"), Some(&"baz"));
        assert_eq!(changes.get("NEW"), Some(&"value"));
        assert_eq!(changes.get("EXISTING"), None);
    }

    #[test]
    fn test_removed() {
        let mut prev = HashMap::new();
        prev.insert("FOO".to_string(), "bar".to_string());
        prev.insert("TO_REMOVE".to_string(), "value".to_string());

        let mut next = HashMap::new();
        next.insert("FOO".to_string(), "bar".to_string());

        let diff = EnvDiff::new(prev, next);
        let removed = diff.removed();

        assert_eq!(removed.len(), 1);
        assert!(removed.contains("TO_REMOVE"));
    }

    #[test]
    fn test_ignored_vars() {
        let mut prev = HashMap::new();
        prev.insert("PWD".to_string(), "/old/path".to_string());
        prev.insert("FOO".to_string(), "bar".to_string());

        let mut next = HashMap::new();
        next.insert("PWD".to_string(), "/new/path".to_string());
        next.insert("FOO".to_string(), "baz".to_string());

        let diff = EnvDiff::new(prev, next);
        let changes = diff.added_or_changed();

        // PWD should be ignored
        assert_eq!(changes.len(), 1);
        assert_eq!(changes.get("FOO"), Some(&"baz"));
        assert_eq!(changes.get("PWD"), None);
    }

    #[test]
    fn test_reverse() {
        let mut prev = HashMap::new();
        prev.insert("FOO".to_string(), "bar".to_string());

        let mut next = HashMap::new();
        next.insert("FOO".to_string(), "baz".to_string());

        let diff = EnvDiff::new(prev.clone(), next.clone());
        let reversed = diff.reverse();

        assert_eq!(reversed.prev, next);
        assert_eq!(reversed.next, prev);
    }

    #[test]
    fn test_is_empty() {
        let env = HashMap::new();
        let diff = EnvDiff::new(env.clone(), env);
        assert!(diff.is_empty());

        let mut env1 = HashMap::new();
        env1.insert("FOO".to_string(), "bar".to_string());
        let mut env2 = HashMap::new();
        env2.insert("FOO".to_string(), "baz".to_string());

        let diff = EnvDiff::new(env1, env2);
        assert!(!diff.is_empty());
    }
}
