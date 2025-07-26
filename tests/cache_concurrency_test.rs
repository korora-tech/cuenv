//! Integration tests for concurrent cache operations

use cuenv::cache_manager::CacheManager;
use cuenv::cue_parser::TaskConfig;
use cuenv::env_manager::EnvManager;
use cuenv::task_executor::TaskExecutor;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// Create a test environment with a CUE file
fn setup_test_env(tasks_cue: &str) -> (TaskExecutor, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let env_file = temp_dir.path().join("env.cue");
    fs::write(&env_file, tasks_cue).unwrap();

    let mut manager = EnvManager::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        manager.load_env(temp_dir.path()).await.unwrap();
    });

    let executor = TaskExecutor::new(manager, temp_dir.path().to_path_buf()).unwrap();
    (executor, temp_dir)
}

#[test]
fn test_concurrent_cache_writes() {
    // Create a shared cache manager
    let cache_manager = Arc::new(CacheManager::new().unwrap());
    let temp_dir = Arc::new(TempDir::new().unwrap());

    // Create test directories
    let src_dir = temp_dir.path().join("src");
    let build_dir = temp_dir.path().join("build");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&build_dir).unwrap();

    // Create a barrier to synchronize thread starts
    let num_threads = 10;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let cache_manager = Arc::clone(&cache_manager);
            let temp_dir = Arc::clone(&temp_dir);
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                // Wait for all threads to be ready
                barrier.wait();

                // Create a unique task config
                let task_config = TaskConfig {
                    description: Some(format!("Test task {}", i)),
                    command: Some(format!("echo task_{}", i)),
                    script: None,
                    dependencies: None,
                    working_dir: None,
                    shell: None,
                    inputs: Some(vec![format!("src/file_{}.rs", i)]),
                    outputs: Some(vec![format!("build/output_{}.o", i)]),
                    cache: Some(true),
                    cache_key: None,
                    timeout: None,
                    security: None,
                };

                // Create input file
                let input_file = src_dir.join(format!("file_{}.rs", i));
                fs::write(&input_file, format!("// Content for file {}", i)).unwrap();

                // Create output file
                let output_file = build_dir.join(format!("output_{}.o", i));
                fs::write(&output_file, format!("Output {}", i)).unwrap();

                let task_name = format!("task_{}", i);

                // Generate cache key
                let cache_key = cache_manager
                    .generate_cache_key(&task_name, &task_config, temp_dir.path())
                    .unwrap();

                // Save to cache
                cache_manager
                    .save_result(&cache_key, &task_config, temp_dir.path(), 0)
                    .unwrap();

                // Immediately read back
                let result = cache_manager
                    .get_cached_result(&cache_key, &task_config, temp_dir.path())
                    .unwrap();

                assert!(result.is_some());
                assert_eq!(result.unwrap().exit_code, 0);
            })
        })
        .collect();

    // Wait for all threads to complete
    for handle in handles {
        handle.join().unwrap();
    }

    // Check final statistics
    let stats = cache_manager.get_statistics().unwrap();
    assert_eq!(stats.writes, num_threads as u64);
    assert_eq!(stats.hits, num_threads as u64);
    assert_eq!(stats.errors, 0);
}

#[test]
fn test_concurrent_same_key_access() {
    let cache_manager = Arc::new(CacheManager::new().unwrap());
    let temp_dir = Arc::new(TempDir::new().unwrap());

    // Create shared task config
    let task_config = Arc::new(TaskConfig {
        description: Some("Shared task".to_string()),
        command: Some("echo shared".to_string()),
        script: None,
        dependencies: None,
        working_dir: None,
        shell: None,
        inputs: Some(vec!["src/*.rs".to_string()]),
        outputs: Some(vec!["build/shared.o".to_string()]),
        cache: Some(true),
        cache_key: Some("shared_key".to_string()), // Force same cache key
        timeout: None,
        security: None,
    });

    // Create test files
    let src_dir = temp_dir.path().join("src");
    let build_dir = temp_dir.path().join("build");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&build_dir).unwrap();
    fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
    fs::write(build_dir.join("shared.o"), "shared output").unwrap();

    let num_threads = 20;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let cache_manager = Arc::clone(&cache_manager);
            let temp_dir = Arc::clone(&temp_dir);
            let task_config = Arc::clone(&task_config);
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                // Synchronize start
                barrier.wait();

                let task_name = "shared_task";

                // Generate cache key (should be same for all)
                let cache_key = cache_manager
                    .generate_cache_key(task_name, &task_config, temp_dir.path())
                    .unwrap();

                // Alternate between reads and writes
                if i % 2 == 0 {
                    // Write
                    cache_manager
                        .save_result(&cache_key, &task_config, temp_dir.path(), 0)
                        .unwrap();
                } else {
                    // Read
                    let _ = cache_manager
                        .get_cached_result(&cache_key, &task_config, temp_dir.path())
                        .unwrap();
                }
            })
        })
        .collect();

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify no errors occurred
    let stats = cache_manager.get_statistics().unwrap();
    assert_eq!(stats.errors, 0);
    assert!(stats.lock_contentions > 0); // Should have some contentions
}

#[test]
fn test_cache_invalidation_race() {
    let cache_manager = Arc::new(CacheManager::new().unwrap());
    let temp_dir = Arc::new(TempDir::new().unwrap());

    let task_config = TaskConfig {
        description: Some("Race test task".to_string()),
        command: Some("echo test".to_string()),
        script: None,
        dependencies: None,
        working_dir: None,
        shell: None,
        inputs: Some(vec!["input.txt".to_string()]),
        outputs: Some(vec!["output.txt".to_string()]),
        cache: Some(true),
        cache_key: None,
        timeout: None,
        security: None,
    };

    // Create initial files
    let input_file = temp_dir.path().join("input.txt");
    let output_file = temp_dir.path().join("output.txt");
    fs::write(&input_file, "initial content").unwrap();
    fs::write(&output_file, "initial output").unwrap();

    // Generate initial cache key and save
    let initial_key = cache_manager
        .generate_cache_key("test_task", &task_config, temp_dir.path())
        .unwrap();

    cache_manager
        .save_result(&initial_key, &task_config, temp_dir.path(), 0)
        .unwrap();

    // Start threads that will:
    // 1. Modify input file
    // 2. Try to read cache (should miss)
    // 3. Save new cache
    let num_threads = 5;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let cache_manager = Arc::clone(&cache_manager);
            let temp_dir = Arc::clone(&temp_dir);
            let task_config = task_config.clone();
            let barrier = Arc::clone(&barrier);
            let input_file = input_file.clone();
            let output_file = output_file.clone();

            thread::spawn(move || {
                barrier.wait();

                // Modify input file
                fs::write(&input_file, format!("modified by thread {}", i)).unwrap();

                // Generate new cache key (should be different)
                let new_key = cache_manager
                    .generate_cache_key("test_task", &task_config, temp_dir.path())
                    .unwrap();

                // This should be a cache miss
                let result = cache_manager
                    .get_cached_result(&new_key, &task_config, temp_dir.path())
                    .unwrap();
                assert!(result.is_none());

                // Update output and save new cache
                fs::write(&output_file, format!("output from thread {}", i)).unwrap();
                cache_manager
                    .save_result(&new_key, &task_config, temp_dir.path(), 0)
                    .unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    let stats = cache_manager.get_statistics().unwrap();
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.misses, num_threads as u64);
    assert_eq!(stats.writes, num_threads as u64 + 1); // +1 for initial save
}

#[tokio::test]
async fn test_concurrent_task_execution_with_cache() {
    let tasks_cue = r#"package env

env: {
    TEST_VAR: "test"
}

tasks: {
    "build1": {
        description: "Build task 1"
        command: "echo 'Building 1' > build1.out"
        outputs: ["build1.out"]
        cache: true
    }
    "build2": {
        description: "Build task 2"
        command: "echo 'Building 2' > build2.out"
        outputs: ["build2.out"]
        cache: true
    }
    "build3": {
        description: "Build task 3"
        command: "echo 'Building 3' > build3.out"
        outputs: ["build3.out"]
        cache: true
    }
}"#;

    let (executor, temp_dir) = setup_test_env(tasks_cue);

    // First execution - should cache results
    let tasks = vec![
        "build1".to_string(),
        "build2".to_string(),
        "build3".to_string(),
    ];
    let result = executor
        .execute_tasks_with_dependencies(&tasks, &[], false)
        .await
        .unwrap();
    assert_eq!(result, 0);

    // Verify outputs exist
    assert!(temp_dir.path().join("build1.out").exists());
    assert!(temp_dir.path().join("build2.out").exists());
    assert!(temp_dir.path().join("build3.out").exists());

    // Check initial cache statistics
    let stats1 = executor.get_cache_statistics().unwrap();
    assert_eq!(stats1.writes, 3);
    assert_eq!(stats1.misses, 3);

    // Remove output files
    fs::remove_file(temp_dir.path().join("build1.out")).unwrap();
    fs::remove_file(temp_dir.path().join("build2.out")).unwrap();
    fs::remove_file(temp_dir.path().join("build3.out")).unwrap();

    // Second execution - should skip due to cache but fail because outputs are missing
    let result = executor
        .execute_tasks_with_dependencies(&tasks, &[], false)
        .await
        .unwrap();
    assert_eq!(result, 0);

    // Outputs should be recreated
    assert!(temp_dir.path().join("build1.out").exists());
    assert!(temp_dir.path().join("build2.out").exists());
    assert!(temp_dir.path().join("build3.out").exists());

    // Check cache was invalidated and tasks re-executed
    let stats2 = executor.get_cache_statistics().unwrap();
    assert_eq!(stats2.writes, 6); // 3 more writes
    assert_eq!(stats2.misses, 6); // 3 more misses
}

#[test]
fn test_cache_cleanup() {
    let cache_manager = CacheManager::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    // Create some cached entries
    for i in 0..5 {
        let task_config = TaskConfig {
            description: Some(format!("Old task {}", i)),
            command: Some("echo old".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: None,
            outputs: None,
            cache: Some(true),
            cache_key: Some(format!("old_key_{}", i)),
            timeout: None,
            security: None,
        };

        let cache_key = cache_manager
            .generate_cache_key(&format!("old_task_{}", i), &task_config, temp_dir.path())
            .unwrap();

        cache_manager
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();
    }

    // Initial stats
    let stats1 = cache_manager.get_statistics().unwrap();
    assert_eq!(stats1.writes, 5);

    // Sleep briefly to ensure some time passes
    thread::sleep(Duration::from_millis(100));

    // Clean up entries older than 50ms
    let (files_deleted, bytes_saved) = cache_manager.cleanup(Duration::from_millis(50)).unwrap();

    // Should have deleted the old entries
    assert!(files_deleted > 0);
    assert!(bytes_saved > 0);

    // Verify cleanup was recorded
    let stats2 = cache_manager.get_statistics().unwrap();
    assert!(stats2.last_cleanup.is_some());
}

#[test]
fn test_cache_lock_timeout() {
    // This test verifies that lock timeouts work correctly
    let cache_manager = Arc::new(CacheManager::new().unwrap());
    let temp_dir = Arc::new(TempDir::new().unwrap());

    let task_config = TaskConfig {
        description: Some("Lock test".to_string()),
        command: Some("echo test".to_string()),
        script: None,
        dependencies: None,
        working_dir: None,
        shell: None,
        inputs: None,
        outputs: Some(vec!["output.txt".to_string()]),
        cache: Some(true),
        cache_key: Some("lock_test_key".to_string()),
        timeout: None,
        security: None,
    };

    // Create output file
    fs::write(temp_dir.path().join("output.txt"), "test").unwrap();

    let cache_key = cache_manager
        .generate_cache_key("lock_task", &task_config, temp_dir.path())
        .unwrap();

    // First thread acquires lock and holds it
    let cache_manager1 = Arc::clone(&cache_manager);
    let temp_dir1 = Arc::clone(&temp_dir);
    let task_config1 = task_config.clone();
    let cache_key1 = cache_key.clone();

    let barrier = Arc::new(Barrier::new(2));
    let barrier1 = Arc::clone(&barrier);

    let handle1 = thread::spawn(move || {
        // Signal ready
        barrier1.wait();

        // This will acquire the lock
        cache_manager1
            .save_result(&cache_key1, &task_config1, temp_dir1.path(), 0)
            .unwrap();

        // Hold lock by sleeping
        thread::sleep(Duration::from_millis(500));
    });

    let cache_manager2 = Arc::clone(&cache_manager);
    let temp_dir2 = Arc::clone(&temp_dir);
    let barrier2 = Arc::clone(&barrier);

    let handle2 = thread::spawn(move || {
        // Wait for first thread to acquire lock
        barrier2.wait();
        thread::sleep(Duration::from_millis(50));

        // This should wait for the lock
        let start = std::time::Instant::now();
        let result = cache_manager2
            .get_cached_result(&cache_key, &task_config, temp_dir2.path())
            .unwrap();
        let elapsed = start.elapsed();

        // Should have waited for lock
        assert!(elapsed.as_millis() > 400);
        assert!(result.is_some());
    });

    handle1.join().unwrap();
    handle2.join().unwrap();

    // Check that lock contention was recorded
    let stats = cache_manager.get_statistics().unwrap();
    assert!(stats.lock_contentions > 0);
}
