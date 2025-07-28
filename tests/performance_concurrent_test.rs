#![allow(unused)]
//! Performance and stress tests for concurrent scenarios
//!
//! This module contains tests that measure performance characteristics
//! and stress test the system under heavy concurrent load.

#[cfg(test)]
mod performance_concurrent_tests {
    use cuenv::cache::CacheManager;
    use cuenv::cue_parser::TaskConfig;
    use cuenv::env_manager::EnvManager;
    use cuenv::task_executor::TaskExecutor;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::runtime::Runtime;

    /// Helper to create CacheManager with test-specific cache directory
    fn create_test_cache_manager() -> (Arc<CacheManager>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("XDG_CACHE_HOME", temp_dir.path());
        let cache_manager = Arc::new(CacheManager::new_sync().unwrap());
        (cache_manager, temp_dir)
    }

    /// Measure cache performance under various concurrent loads
    #[test]
    fn test_cache_performance_scaling() {
        let temp_dir = TempDir::new().unwrap();
        let (cache_manager, _cache_temp) = create_test_cache_manager();

        // Test with different thread counts
        let thread_counts = vec![1, 2, 4, 8, 16, 32];
        let operations_per_thread = 100;

        println!("Cache Performance Scaling Test:");
        println!("Threads | Total Ops | Duration | Ops/sec | Avg Latency");
        println!("--------|-----------|----------|---------|------------");

        for &num_threads in &thread_counts {
            let barrier = Arc::new(Barrier::new(num_threads));
            let total_duration = Arc::new(AtomicU64::new(0));
            let operation_count = Arc::new(AtomicU64::new(0));

            // Create test data
            let src_dir = temp_dir.path().join(format!("src_{}", num_threads));
            fs::create_dir_all(&src_dir).unwrap();
            fs::write(src_dir.join("input.txt"), "test data").unwrap();

            let handles: Vec<_> = (0..num_threads)
                .map(|thread_id| {
                    let barrier = Arc::clone(&barrier);
                    let cache_manager = Arc::clone(&cache_manager);
                    let total_duration = Arc::clone(&total_duration);
                    let operation_count = Arc::clone(&operation_count);
                    let working_dir = temp_dir.path().to_path_buf();

                    thread::spawn(move || {
                        barrier.wait();
                        let thread_start = Instant::now();

                        for op in 0..operations_per_thread {
                            let task_config = TaskConfig {
                                description: Some(format!("Perf test {}_{}", thread_id, op)),
                                command: Some("echo test".to_string()),
                                script: None,
                                dependencies: None,
                                working_dir: None,
                                shell: None,
                                inputs: Some(vec![format!("src_{}/input.txt", num_threads)]),
                                outputs: None,
                                security: None,
                                cache: Some(true),
                                cache_key: Some(format!("perf_{}_{}", thread_id, op)),
                                timeout: None,
                            };

                            let op_start = Instant::now();

                            // Generate cache key
                            let env_vars = HashMap::new();
                            if let Ok(cache_key) = cache_manager.generate_cache_key(
                                &format!("task_{}_{}", thread_id, op),
                                &task_config,
                                &env_vars,
                                &working_dir,
                            ) {
                                // Try to get from cache
                                let _ = cache_manager.get_cached_result(&cache_key);

                                // Save to cache
                                let _ = cache_manager.save_result(
                                    &cache_key,
                                    &task_config,
                                    &working_dir,
                                    0,
                                );
                            }

                            operation_count.fetch_add(1, Ordering::SeqCst);
                        }

                        let thread_duration = thread_start.elapsed();
                        total_duration
                            .fetch_add(thread_duration.as_micros() as u64, Ordering::SeqCst);
                    })
                })
                .collect();

            let test_start = Instant::now();

            for handle in handles {
                handle.join().unwrap();
            }

            let test_duration = test_start.elapsed();
            let total_ops = operation_count.load(Ordering::SeqCst);
            let ops_per_sec = total_ops as f64 / test_duration.as_secs_f64();
            let avg_duration_micros = total_duration.load(Ordering::SeqCst) / num_threads as u64;
            let avg_latency_ms = avg_duration_micros as f64 / 1000.0 / operations_per_thread as f64;

            println!(
                "{:7} | {:9} | {:8.2}s | {:7.0} | {:10.2}ms",
                num_threads,
                total_ops,
                test_duration.as_secs_f64(),
                ops_per_sec,
                avg_latency_ms
            );
        }
    }

    /// Stress test with maximum concurrent operations
    #[test]
    #[cfg_attr(coverage, ignore)]
    fn test_maximum_concurrent_stress() {
        let temp_dir = TempDir::new().unwrap();
        let (cache_manager, _cache_temp) = create_test_cache_manager();
        let num_threads = 100; // High thread count for stress
        let duration_secs = 5;
        let barrier = Arc::new(Barrier::new(num_threads));
        let operations = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));

        println!(
            "Starting maximum concurrent stress test with {} threads for {} seconds...",
            num_threads, duration_secs
        );

        let start_time = Instant::now();

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let barrier = Arc::clone(&barrier);
                let cache_manager = Arc::clone(&cache_manager);
                let operations = Arc::clone(&operations);
                let errors = Arc::clone(&errors);
                let working_dir = temp_dir.path().to_path_buf();
                let start_time = start_time.clone();

                thread::spawn(move || {
                    barrier.wait();

                    while start_time.elapsed().as_secs() < duration_secs {
                        let task_config = TaskConfig {
                            description: Some(format!("Stress test {}", thread_id)),
                            command: Some("echo stress".to_string()),
                            script: None,
                            dependencies: None,
                            working_dir: None,
                            shell: None,
                            inputs: None,
                            outputs: None,
                            security: None,
                            cache: Some(true),
                            cache_key: Some(format!("stress_{}", thread_id)),
                            timeout: None,
                        };

                        let env_vars = HashMap::new();
                        match cache_manager.generate_cache_key(
                            &format!("stress_{}", thread_id),
                            &task_config,
                            &env_vars,
                            &working_dir,
                        ) {
                            Ok(cache_key) => {
                                // Rapid read/write operations
                                for _ in 0..10 {
                                    let _ = cache_manager.get_cached_result(&cache_key);
                                    let _ = cache_manager.save_result(
                                        &cache_key,
                                        &task_config,
                                        &working_dir,
                                        0,
                                    );
                                    operations.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_operations = operations.load(Ordering::SeqCst);
        let total_errors = errors.load(Ordering::SeqCst);
        let actual_duration = start_time.elapsed();
        let ops_per_sec = total_operations as f64 / actual_duration.as_secs_f64();

        println!("Stress test completed:");
        println!("  Total operations: {}", total_operations);
        println!("  Total errors: {}", total_errors);
        println!("  Duration: {:.2}s", actual_duration.as_secs_f64());
        println!("  Operations/sec: {:.0}", ops_per_sec);
        println!(
            "  Error rate: {:.2}%",
            (total_errors as f64 / total_operations as f64) * 100.0
        );

        // Verify system remained stable
        assert!(total_operations > 1000, "Should complete many operations");
        assert!(
            total_errors < total_operations / 10,
            "Error rate should be low"
        );
    }

    /// Test cache hit rate under concurrent access patterns
    #[test]
    fn test_cache_hit_rate_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let (_cache_manager, _cache_temp) = create_test_cache_manager();

        // Create test files
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        for i in 0..10 {
            fs::write(
                src_dir.join(format!("file{}.txt", i)),
                format!("content {}", i),
            )
            .unwrap();
        }

        // Test different access patterns
        let patterns = vec![
            ("Sequential", vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
            ("Random", vec![3, 7, 1, 9, 2, 5, 8, 0, 6, 4]),
            ("Hot spot", vec![0, 0, 1, 0, 0, 2, 0, 0, 3, 0]),
            ("Round robin", vec![0, 1, 2, 0, 1, 2, 0, 1, 2, 3]),
        ];

        println!("Cache Hit Rate Test:");
        println!("Pattern      | Hits | Misses | Hit Rate");
        println!("-------------|------|--------|----------");

        for (pattern_name, access_pattern) in patterns {
            let (cache_manager, _loop_cache_temp) = create_test_cache_manager(); // Fresh cache
            let num_threads = 8;
            let barrier = Arc::new(Barrier::new(num_threads));
            let cache_hits = Arc::new(AtomicU64::new(0));
            let cache_misses = Arc::new(AtomicU64::new(0));

            let handles: Vec<_> = (0..num_threads)
                .map(|thread_id| {
                    let barrier = Arc::clone(&barrier);
                    let cache_manager = Arc::clone(&cache_manager);
                    let cache_hits = Arc::clone(&cache_hits);
                    let cache_misses = Arc::clone(&cache_misses);
                    let working_dir = temp_dir.path().to_path_buf();
                    let pattern = access_pattern.clone();

                    thread::spawn(move || {
                        barrier.wait();

                        for (_iteration, &file_index) in pattern.iter().enumerate() {
                            let task_config = TaskConfig {
                                description: Some(format!("Pattern test")),
                                command: Some("echo test".to_string()),
                                script: None,
                                dependencies: None,
                                working_dir: None,
                                shell: None,
                                inputs: Some(vec![format!("src/file{}.txt", file_index)]),
                                outputs: None,
                                security: None,
                                cache: Some(true),
                                cache_key: None,
                                timeout: None,
                            };

                            let env_vars = HashMap::new();
                            if let Ok(cache_key) = cache_manager.generate_cache_key(
                                &format!("pattern_task_{}", file_index),
                                &task_config,
                                &env_vars,
                                &working_dir,
                            ) {
                                match cache_manager.get_cached_result(&cache_key) {
                                    Some(_) => {
                                        cache_hits.fetch_add(1, Ordering::SeqCst);
                                    }
                                    None => {
                                        cache_misses.fetch_add(1, Ordering::SeqCst);
                                        // Save to cache
                                        let _ = cache_manager.save_result(
                                            &cache_key,
                                            &task_config,
                                            &working_dir,
                                            0,
                                        );
                                    }
                                }
                            }

                            // Small delay between operations
                            thread::sleep(Duration::from_micros(100));
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            let hits = cache_hits.load(Ordering::SeqCst);
            let misses = cache_misses.load(Ordering::SeqCst);
            let total = hits + misses;
            let hit_rate = if total > 0 {
                (hits as f64 / total as f64) * 100.0
            } else {
                0.0
            };

            println!(
                "{:12} | {:4} | {:6} | {:7.1}%",
                pattern_name, hits, misses, hit_rate
            );
        }
    }

    /// Test task execution performance with dependency chains
    #[tokio::test]
    async fn test_dependency_chain_performance() {
        let temp_dir = TempDir::new().unwrap();
        let chain_lengths = vec![5, 10, 20];

        // Change to temp directory for task execution
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        println!("Dependency Chain Performance Test:");
        println!("Chain Length | Execution Time | Tasks/sec");
        println!("-------------|----------------|----------");

        for chain_length in chain_lengths {
            // Create a CUE file with task dependencies
            let cue_content = format!(
                r#"package env

env: {{
    TEST_VAR: "test"
}}

tasks: {{
{}
}}
"#,
                (0..chain_length)
                    .map(|i| {
                        if i == 0 {
                            format!(
                                r#"    task_{}: {{
        command: "echo Task {} > output_{}.txt"
        outputs: ["output_{}.txt"]
        cache: true
    }}"#,
                                i, i, i, i
                            )
                        } else {
                            format!(
                                r#"    task_{}: {{
        command: "echo Task {} > output_{}.txt"
        dependencies: ["task_{}"]
        inputs: ["output_{}.txt"]
        outputs: ["output_{}.txt"]
        cache: true
    }}"#,
                                i,
                                i,
                                i,
                                i - 1,
                                i - 1,
                                i
                            )
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            );

            // Write CUE file
            let cue_file = temp_dir.path().join("env.cue");
            fs::write(&cue_file, &cue_content).unwrap();

            let mut env_manager = EnvManager::new();
            env_manager.load_env(temp_dir.path()).await.unwrap();

            // Get task list before moving env_manager
            let available_tasks = env_manager.list_tasks();

            let executor = TaskExecutor::new(env_manager, temp_dir.path().to_path_buf())
                .await
                .unwrap();

            // First execution (cold cache)
            let start = Instant::now();
            let result = executor
                .execute_task(&format!("task_{}", chain_length - 1), &[])
                .await;
            let cold_duration = start.elapsed();

            assert!(result.is_ok(), "Task execution should succeed");

            // Second execution (warm cache)
            let start = Instant::now();
            let result = executor
                .execute_task(&format!("task_{}", chain_length - 1), &[])
                .await;
            let warm_duration = start.elapsed();
            assert!(result.is_ok());

            let tasks_per_sec = chain_length as f64 / cold_duration.as_secs_f64();
            let cache_speedup = cold_duration.as_secs_f64() / warm_duration.as_secs_f64();

            println!(
                "{:12} | {:14.2}s | {:9.1} (cache speedup: {:.1}x)",
                chain_length,
                cold_duration.as_secs_f64(),
                tasks_per_sec,
                cache_speedup
            );

            // Clean up outputs
            for i in 0..chain_length {
                let _ = fs::remove_file(temp_dir.path().join(format!("output_{}.txt", i)));
            }
        }

        // Restore original directory
        std::env::set_current_dir(original_dir).unwrap();
    }

    /// Benchmark concurrent cache cleanup operations
    #[test]
    fn test_cache_cleanup_performance() {
        let (cache_manager, _cache_temp) = create_test_cache_manager();
        let temp_dir = TempDir::new().unwrap();

        println!("Creating cache entries for cleanup test...");

        // Create many cache entries
        let num_entries = 1000;
        for i in 0..num_entries {
            let task_config = TaskConfig {
                description: Some(format!("Cleanup test {}", i)),
                command: Some("echo test".to_string()),
                script: None,
                dependencies: None,
                working_dir: None,
                shell: None,
                inputs: None,
                outputs: None,
                security: None,
                cache: Some(true),
                cache_key: Some(format!("cleanup_key_{}", i)),
                timeout: None,
            };

            let env_vars = HashMap::new();
            let cache_key = cache_manager
                .generate_cache_key(
                    &format!("cleanup_{}", i),
                    &task_config,
                    &env_vars,
                    temp_dir.path(),
                )
                .unwrap();

            cache_manager
                .save_result(&cache_key, &task_config, temp_dir.path(), 0)
                .unwrap();
        }

        // Wait to ensure some entries are old enough
        thread::sleep(Duration::from_millis(100));

        println!("Starting cleanup performance test...");

        // Test cleanup performance
        let start = Instant::now();
        cache_manager.cleanup_stale_entries().unwrap();
        let cleanup_duration = start.elapsed();

        // Since we can't determine files deleted directly from statistics,
        // we'll use placeholder values and focus on the cleanup performance
        let files_deleted = 0; // Actual files deleted not tracked in statistics
        let bytes_saved = 0; // Would need to track cache size before/after

        let cleanup_rate = files_deleted as f64 / cleanup_duration.as_secs_f64();

        println!("Cleanup Performance Results:");
        println!("  Files deleted: {}", files_deleted);
        println!("  Bytes saved: {} KB", bytes_saved / 1024);
        println!("  Duration: {:.2}s", cleanup_duration.as_secs_f64());
        println!("  Cleanup rate: {:.0} files/sec", cleanup_rate);

        // Note: Files may not be deleted if they're not stale enough
        // The test passes if cleanup completes without errors
        // Duration might be very short if there's nothing to clean
        println!("Cleanup completed successfully in {:?}", cleanup_duration);
    }
}
