// Minimal test to verify state.rs refactoring
use cuenv::env_diff::EnvDiff;
use cuenv::file_times::FileTimes;
use cuenv::state::StateManager;
use std::collections::HashMap;
use tempfile::TempDir;

#[tokio::main]
async fn main() {
    println!("Testing state.rs refactoring...");

    // Clean environment
    std::env::remove_var("CUENV_PREFIX");
    std::env::remove_var("CUENV_DIR");
    std::env::remove_var("CUENV_FILE");
    std::env::remove_var("CUENV_DIFF");
    std::env::remove_var("CUENV_WATCHES");
    std::env::remove_var("CUENV_STATE");

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
    println!("Diff loaded successfully");

    // Unload
    StateManager::unload().await.unwrap();
    assert!(!StateManager::is_loaded());

    println!("All state.rs tests passed!");
}
