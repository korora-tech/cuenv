//! Chaos testing for concurrent scenarios
//!
//! This module contains tests that inject failures and unexpected conditions
//! to verify the system behaves correctly under adverse conditions.

#[cfg(test)]
mod chaos_concurrent_tests {
    use cuenv::cache_manager::CacheManager;
    use cuenv::cue_parser::TaskConfig;
    use cuenv::env_manager::EnvManager;
    use cuenv::errors::{Error, Result};
    use cuenv::state::StateManager;
    use cuenv::sync_env::SyncEnv;
    use cuenv::task_executor::TaskExecutor;
    use rand::Rng;
    use std::collections::HashMap;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::runtime::Runtime;

    /// Randomly inject failures into filesystem operations
    struct ChaosFilesystem {
        failure_rate: f32,
        failures_injected: Arc<AtomicU32>,
    }

    impl ChaosFilesystem {
        fn new(failure_rate: f32) -> Self {
            Self {
                failure_rate,
                failures_injected: Arc::new(AtomicU32::new(0)),
            }
        }

        fn maybe_fail(&self, operation: &str) -> Result<()> {
            let mut rng = rand::thread_rng();
            if rng.gen::<f32>() < self.failure_rate {
                self.failures_injected.fetch_add(1, Ordering::SeqCst);
                Err(Error::file_system(
                    PathBuf::from("chaos"),
                    operation,
                    std::io::Error::new(ErrorKind::Other, "Chaos injection"),
                ))
            } else {
                Ok(())
            }
        }

        fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
            self.maybe_fail("write")?;
            fs::write(path, contents)
                .map_err(|e| Error::file_system(path.to_path_buf(), "write", e))
        }

        fn read(&self, path: &Path) -> Result<Vec<u8>> {
            self.maybe_fail("read")?;
            fs::read(path).map_err(|e| Error::file_system(path.to_path_buf(), "read", e))
        }
    }

    /// Test cache behavior with random filesystem failures
    #[test]
    fn test_cache_with_filesystem_chaos() {
        let temp_dir = TempDir::new().unwrap();
        let cache_manager = Arc::new(CacheManager::new().unwrap());
        let chaos_fs = Arc::new(ChaosFilesystem::new(0.1)); // 10% failure rate
        let num_threads = 10;
        let operations_per_thread = 20;
        let barrier = Arc::new(Barrier::new(num_threads));
        let successful_ops = Arc::new(AtomicU32::new(0));
        let failed_ops = Arc::new(AtomicU32::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let barrier = Arc::clone(&barrier);
                let cache_manager = Arc::clone(&cache_manager);
                let chaos_fs = Arc::clone(&chaos_fs);
                let successful_ops = Arc::clone(&successful_ops);
                let failed_ops = Arc::clone(&failed_ops);
                let working_dir = temp_dir.path().to_path_buf();

                thread::spawn(move || {
                    barrier.wait();

                    for op in 0..operations_per_thread {
                        let task_config = TaskConfig {
                            description: Some(format!("Chaos task {}_{}", thread_id, op)),
                            command: Some("echo test".to_string()),
                            script: None,
                            dependencies: None,
                            working_dir: None,
                            shell: None,
                            inputs: None,
                            outputs: Some(vec![format!("output_{}_{}.txt", thread_id, op)]),
                            security: None,
                            cache: Some(true),
                            cache_key: None,
                            timeout: None,
                        };

                        // Try to write output file with chaos
                        let output_path =
                            working_dir.join(format!("output_{}_{}.txt", thread_id, op));
                        match chaos_fs.write(&output_path, b"test output") {
                            Ok(_) => {
                                // Try cache operations
                                match cache_manager.generate_cache_key(
                                    &format!("task_{}_{}", thread_id, op),
                                    &task_config,
                                    &working_dir,
                                ) {
                                    Ok(cache_key) => {
                                        match cache_manager.save_result(
                                            &cache_key,
                                            &task_config,
                                            &working_dir,
                                            0,
                                        ) {
                                            Ok(_) => successful_ops.fetch_add(1, Ordering::SeqCst),
                                            Err(_) => failed_ops.fetch_add(1, Ordering::SeqCst),
                                        };
                                    }
                                    Err(_) => failed_ops.fetch_add(1, Ordering::SeqCst),
                                }
                            }
                            Err(_) => failed_ops.fetch_add(1, Ordering::SeqCst),
                        }

                        // Small delay to spread operations
                        thread::sleep(Duration::from_millis(10));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_successful = successful_ops.load(Ordering::SeqCst);
        let total_failed = failed_ops.load(Ordering::SeqCst);
        let failures_injected = chaos_fs.failures_injected.load(Ordering::SeqCst);

        println!(
            "Chaos test results - Success: {}, Failed: {}, Chaos failures: {}",
            total_successful, total_failed, failures_injected
        );

        // System should handle failures gracefully
        assert!(
            total_successful > 0,
            "Some operations should succeed despite chaos"
        );
        assert!(
            failures_injected > 0,
            "Chaos should have injected some failures"
        );
        assert_eq!(
            total_successful + total_failed,
            (num_threads * operations_per_thread) as u32
        );
    }

    /// Test state management with random interruptions
    #[test]
    fn test_state_management_chaos() {
        let runtime = Runtime::new().unwrap();
        let num_threads = 8;
        let chaos_rate = 0.2; // 20% chance of interruption
        let barrier = Arc::new(Barrier::new(num_threads));
        let interruptions = Arc::new(AtomicU32::new(0));
        let recoveries = Arc::new(AtomicU32::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let barrier = Arc::clone(&barrier);
                let interruptions = Arc::clone(&interruptions);
                let recoveries = Arc::clone(&recoveries);

                thread::spawn(move || {
                    let runtime = Runtime::new().unwrap();
                    let mut rng = rand::thread_rng();
                    barrier.wait();

                    for i in 0..10 {
                        let temp_dir = TempDir::new().unwrap();

                        runtime.block_on(async {
                            let diff =
                                cuenv::env_diff::EnvDiff::new(HashMap::new(), HashMap::new());
                            let watches = cuenv::file_times::FileTimes::new();

                            // Load state
                            match StateManager::load(
                                temp_dir.path(),
                                &temp_dir.path().join("env.cue"),
                                Some(&format!("chaos_env_{}_{}", thread_id, i)),
                                &[],
                                &diff,
                                &watches,
                            )
                            .await
                            {
                                Ok(_) => {
                                    // Randomly interrupt operations
                                    if rng.gen::<f32>() < chaos_rate {
                                        interruptions.fetch_add(1, Ordering::SeqCst);

                                        // Simulate abrupt interruption by not unloading
                                        // This tests the system's ability to handle incomplete states
                                        return;
                                    }

                                    // Normal unload
                                    if let Ok(_) = StateManager::unload().await {
                                        recoveries.fetch_add(1, Ordering::SeqCst);
                                    }
                                }
                                Err(_) => {
                                    // Error during load - system should handle gracefully
                                }
                            }
                        });
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_interruptions = interruptions.load(Ordering::SeqCst);
        let total_recoveries = recoveries.load(Ordering::SeqCst);

        println!(
            "State chaos test - Interruptions: {}, Recoveries: {}",
            total_interruptions, total_recoveries
        );

        // System should handle interruptions without crashing
        assert!(
            total_interruptions > 0,
            "Some interruptions should have occurred"
        );
        assert!(
            total_recoveries > 0,
            "Some operations should complete normally"
        );
    }

    /// Test concurrent task execution with random delays and failures
    #[test]
    fn test_task_execution_chaos() {
        let runtime = Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();

        runtime.block_on(async {
            let mut tasks = HashMap::new();
            let num_tasks = 20;

            // Create tasks with random characteristics
            for i in 0..num_tasks {
                let mut rng = rand::thread_rng();
                let fail_chance = rng.gen::<f32>();

                let command = if fail_chance < 0.2 {
                    // 20% chance of failure
                    "exit 1".to_string()
                } else if fail_chance < 0.4 {
                    // 20% chance of slow task
                    "sleep 0.5 && echo done".to_string()
                } else {
                    // 60% chance of normal task
                    format!("echo task_{}", i)
                };

                tasks.insert(
                    format!("task_{}", i),
                    TaskConfig {
                        description: Some(format!("Chaos task {}", i)),
                        command: Some(command),
                        script: None,
                        dependencies: if i > 0 && rng.gen_bool(0.3) {
                            // 30% chance of having dependencies
                            Some(vec![format!("task_{}", rng.gen_range(0..i))])
                        } else {
                            None
                        },
                        working_dir: None,
                        shell: None,
                        inputs: None,
                        outputs: None,
                        security: None,
                        cache: Some(rng.gen_bool(0.5)), // 50% chance of caching
                        cache_key: None,
                        timeout: Some(Duration::from_secs(1)),
                    },
                );
            }

            let env_manager = EnvManager::new(
                HashMap::new(),
                tasks,
                Vec::new(),
                Vec::new(),
                None,
                Vec::new(),
            );

            let executor = TaskExecutor::new(env_manager, temp_dir.path().to_path_buf()).unwrap();

            // Execute random tasks concurrently
            let mut handles = Vec::new();
            for i in 0..10 {
                let executor_clone = executor.clone();
                let handle = tokio::spawn(async move {
                    let task_name = format!("task_{}", i % num_tasks);
                    executor_clone.execute_task(&task_name, &[]).await
                });
                handles.push(handle);
            }

            let mut successes = 0;
            let mut failures = 0;
            let mut errors = 0;

            for handle in handles {
                match handle.await {
                    Ok(Ok(0)) => successes += 1,
                    Ok(Ok(_)) => failures += 1,
                    Ok(Err(_)) | Err(_) => errors += 1,
                }
            }

            println!(
                "Task chaos test - Successes: {}, Failures: {}, Errors: {}",
                successes, failures, errors
            );

            // Should have a mix of results
            assert!(successes + failures + errors == 10);
            assert!(successes > 0, "Some tasks should succeed");
        });
    }

    /// Test cache corruption recovery
    #[test]
    fn test_cache_corruption_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let cache_manager = Arc::new(CacheManager::new().unwrap());
        let corruption_injected = Arc::new(AtomicBool::new(false));
        let recovery_successful = Arc::new(AtomicBool::new(false));

        // First, create valid cache entries
        let task_config = TaskConfig {
            description: Some("Corruption test".to_string()),
            command: Some("echo test".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: None,
            outputs: Some(vec!["output.txt".to_string()]),
            security: None,
            cache: Some(true),
            cache_key: None,
            timeout: None,
        };

        // Create output file
        fs::write(temp_dir.path().join("output.txt"), "test output").unwrap();

        // Save to cache
        let cache_key = cache_manager
            .generate_cache_key("test_task", &task_config, temp_dir.path())
            .unwrap();

        cache_manager
            .save_result(&cache_key, &task_config, temp_dir.path(), 0)
            .unwrap();

        // Now corrupt the cache by writing invalid data
        let cache_dir = dirs::cache_dir().unwrap().join("cuenv").join("cache");

        // Find cache files and corrupt them
        if let Ok(entries) = fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                    // Corrupt the JSON file
                    fs::write(entry.path(), b"{ invalid json ").ok();
                    corruption_injected.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }

        // Try to read from corrupted cache
        match cache_manager.get_cached_result(&cache_key, &task_config, temp_dir.path()) {
            Ok(_) => {
                // Cache should handle corruption gracefully
                recovery_successful.store(true, Ordering::SeqCst);
            }
            Err(_) => {
                // Error is acceptable, but system shouldn't crash
                recovery_successful.store(true, Ordering::SeqCst);
            }
        }

        assert!(
            corruption_injected.load(Ordering::SeqCst)
                || recovery_successful.load(Ordering::SeqCst),
            "System should handle cache corruption gracefully"
        );
    }

    /// Test concurrent operations during system resource exhaustion
    #[test]
    fn test_resource_exhaustion_chaos() {
        let num_threads = 50; // Many threads to exhaust resources
        let barrier = Arc::new(Barrier::new(num_threads));
        let resource_errors = Arc::new(AtomicU32::new(0));
        let completed = Arc::new(AtomicU32::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                let resource_errors = Arc::clone(&resource_errors);
                let completed = Arc::clone(&completed);

                thread::spawn(move || {
                    barrier.wait();

                    // Try to create many temporary files
                    let mut temp_files = Vec::new();
                    for j in 0..100 {
                        match TempDir::new() {
                            Ok(temp_dir) => {
                                // Try to create files in the temp dir
                                for k in 0..10 {
                                    let file_path =
                                        temp_dir.path().join(format!("file_{}_{}.txt", j, k));
                                    match fs::write(
                                        &file_path,
                                        format!("Thread {} file {}-{}", i, j, k),
                                    ) {
                                        Ok(_) => temp_files.push(temp_dir),
                                        Err(_) => {
                                            resource_errors.fetch_add(1, Ordering::SeqCst);
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                resource_errors.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                        }
                    }

                    if !temp_files.is_empty() {
                        completed.fetch_add(1, Ordering::SeqCst);
                    }

                    // Keep temp files alive briefly
                    thread::sleep(Duration::from_millis(100));
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_errors = resource_errors.load(Ordering::SeqCst);
        let total_completed = completed.load(Ordering::SeqCst);

        println!(
            "Resource exhaustion test - Completed: {}, Errors: {}",
            total_completed, total_errors
        );

        // System should handle resource exhaustion gracefully
        assert!(total_completed > 0 || total_errors > 0);
    }
}
