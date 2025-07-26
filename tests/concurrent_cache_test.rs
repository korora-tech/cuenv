#[cfg(test)]
mod concurrent_cache_tests {
    use cuenv::cache_manager::CacheManager;
    use cuenv::cue_parser::TaskConfig;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Helper to create a basic test task configuration
    fn create_test_task(name: &str, cache_enabled: bool) -> TaskConfig {
        TaskConfig {
            description: Some(format!("Test task: {}", name)),
            command: Some("echo test".to_string()),
            script: None,
            dependencies: None,
            working_dir: None,
            shell: None,
            inputs: Some(vec!["src/*".to_string()]),
            outputs: Some(vec!["build/output.txt".to_string()]),
            security: None,
            cache: Some(cache_enabled),
            cache_key: None,
            timeout: None,
        }
    }

    /// Test concurrent access to the task cache
    #[test]
    fn test_concurrent_cache_access() {
        let temp_dir = TempDir::new().unwrap();
        let num_threads = 8;
        let barrier = Arc::new(Barrier::new(num_threads));
        let cache_hits = Arc::new(AtomicU32::new(0));
        let cache_misses = Arc::new(AtomicU32::new(0));

        // Setup test files
        let src_dir = temp_dir.path().join("src");
        let build_dir = temp_dir.path().join("build");
        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&build_dir).unwrap();
        fs::write(src_dir.join("input.txt"), "test content").unwrap();

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let cache_hits = Arc::clone(&cache_hits);
                let cache_misses = Arc::clone(&cache_misses);
                let working_dir = temp_dir.path().to_path_buf();

                thread::spawn(move || {
                    // Synchronize thread start
                    barrier.wait();

                    let cache = CacheManager::new().unwrap();
                    let task_config = create_test_task("concurrent_test", true);

                    // Generate cache key
                    let cache_key = cache
                        .generate_cache_key("test_task", &task_config, &working_dir)
                        .unwrap();

                    // Try to get cached result
                    match cache.get_cached_result(&cache_key, &task_config, &working_dir) {
                        Ok(Some(_)) => {
                            cache_hits.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(None) => {
                            cache_misses.fetch_add(1, Ordering::SeqCst);

                            // Simulate task execution
                            let output_file = working_dir.join("build/output.txt");
                            fs::write(&output_file, "test output").ok();

                            // Save to cache
                            cache
                                .save_result(&cache_key, &task_config, &working_dir, 0)
                                .ok();
                        }
                        Err(e) => {
                            eprintln!("Cache error: {}", e);
                        }
                    }
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        let total_hits = cache_hits.load(Ordering::SeqCst);
        let total_misses = cache_misses.load(Ordering::SeqCst);

        println!(
            "Cache test results - Hits: {}, Misses: {}",
            total_hits, total_misses
        );

        // Verify results
        assert_eq!(total_hits + total_misses, num_threads as u32);
        assert!(total_misses >= 1, "At least one cache miss expected");
    }

    /// Test cache key generation is deterministic
    #[test]
    fn test_concurrent_cache_key_generation() {
        let temp_dir = TempDir::new().unwrap();
        let num_threads = 10;
        let barrier = Arc::new(Barrier::new(num_threads));
        let keys = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Create test input file
        let src_dir = temp_dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("input.txt"), "stable content").unwrap();

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                let keys = Arc::clone(&keys);
                let working_dir = temp_dir.path().to_path_buf();

                thread::spawn(move || {
                    barrier.wait();

                    let cache = CacheManager::new().unwrap();
                    let task_config = create_test_task("key_test", true);

                    let cache_key = cache
                        .generate_cache_key("stable_task", &task_config, &working_dir)
                        .unwrap();

                    keys.lock().unwrap().push((i, cache_key));
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // All keys should be identical
        let all_keys = keys.lock().unwrap();
        let first_key = &all_keys[0].1;
        for (thread_id, key) in all_keys.iter() {
            assert_eq!(
                key, first_key,
                "Thread {} generated different key: {} vs {}",
                thread_id, key, first_key
            );
        }
    }

    /// Test cache behavior with concurrent file modifications
    #[test]
    fn test_cache_invalidation_on_concurrent_changes() {
        let temp_dir = TempDir::new().unwrap();
        let barrier = Arc::new(Barrier::new(2));

        // Setup
        let src_dir = temp_dir.path().join("src");
        let build_dir = temp_dir.path().join("build");
        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&build_dir).unwrap();
        let input_file = src_dir.join("input.txt");
        fs::write(&input_file, "initial content").unwrap();

        // Thread 1: Generate cache key, save result
        let barrier1 = Arc::clone(&barrier);
        let working_dir1 = temp_dir.path().to_path_buf();
        let input_file1 = input_file.clone();

        let handle1 = thread::spawn(move || {
            let cache = CacheManager::new().unwrap();
            let task_config = create_test_task("invalidation_test", true);

            // Generate initial key
            let key1 = cache
                .generate_cache_key("test", &task_config, &working_dir1)
                .unwrap();

            // Create output and save to cache
            let output_file = working_dir1.join("build/output.txt");
            fs::write(&output_file, "output v1").unwrap();
            cache
                .save_result(&key1, &task_config, &working_dir1, 0)
                .unwrap();

            // Signal thread 2
            barrier1.wait();

            // Wait for file modification
            thread::sleep(Duration::from_millis(100));

            // Generate key after modification
            let key2 = cache
                .generate_cache_key("test", &task_config, &working_dir1)
                .unwrap();

            (key1, key2)
        });

        // Thread 2: Modify input file
        let barrier2 = Arc::clone(&barrier);
        let handle2 = thread::spawn(move || {
            // Wait for thread 1 to save cache
            barrier2.wait();

            // Modify input file
            thread::sleep(Duration::from_millis(50));
            fs::write(&input_file, "modified content").unwrap();
        });

        handle2.join().unwrap();
        let (key_before, key_after) = handle1.join().unwrap();

        // Keys should be different after file modification
        assert_ne!(
            key_before, key_after,
            "Cache key should change when input file is modified"
        );
    }

    /// Test concurrent cache operations don't corrupt data
    #[test]
    fn test_cache_data_integrity() {
        let temp_dir = TempDir::new().unwrap();
        let num_threads = 5;
        let num_tasks = 3;
        let barrier = Arc::new(Barrier::new(num_threads));
        let errors = Arc::new(AtomicU32::new(0));

        // Setup directories
        let src_dir = temp_dir.path().join("src");
        let build_dir = temp_dir.path().join("build");
        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&build_dir).unwrap();

        // Create different input files for different tasks
        for i in 0..num_tasks {
            fs::write(
                src_dir.join(format!("input{}.txt", i)),
                format!("content for task {}", i),
            )
            .unwrap();
        }

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let barrier = Arc::clone(&barrier);
                let errors = Arc::clone(&errors);
                let working_dir = temp_dir.path().to_path_buf();

                thread::spawn(move || {
                    barrier.wait();

                    let cache = CacheManager::new().unwrap();

                    // Each thread works on multiple tasks
                    for task_id in 0..num_tasks {
                        let mut task_config = create_test_task(&format!("task_{}", task_id), true);
                        task_config.inputs = Some(vec![format!("src/input{}.txt", task_id)]);
                        task_config.outputs = Some(vec![format!("build/output{}.txt", task_id)]);

                        let cache_key = cache
                            .generate_cache_key(
                                &format!("task_{}", task_id),
                                &task_config,
                                &working_dir,
                            )
                            .unwrap();

                        // Try to get from cache
                        match cache.get_cached_result(&cache_key, &task_config, &working_dir) {
                            Ok(Some(result)) => {
                                // Verify cached data integrity
                                if result.exit_code != 0 {
                                    errors.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Ok(None) => {
                                // Create output
                                let output_file =
                                    working_dir.join(format!("build/output{}.txt", task_id));
                                fs::write(
                                    &output_file,
                                    format!("Thread {} executed task {}", thread_id, task_id),
                                )
                                .unwrap();

                                // Save to cache
                                if let Err(_) =
                                    cache.save_result(&cache_key, &task_config, &working_dir, 0)
                                {
                                    errors.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::SeqCst);
                            }
                        }

                        // Small delay between tasks
                        thread::sleep(Duration::from_micros(100));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_errors = errors.load(Ordering::SeqCst);
        assert_eq!(
            total_errors, 0,
            "No errors should occur during concurrent cache operations"
        );

        // Verify all output files exist
        for i in 0..num_tasks {
            let output_file = build_dir.join(format!("output{}.txt", i));
            assert!(output_file.exists(), "Output file {} should exist", i);
        }
    }
}
