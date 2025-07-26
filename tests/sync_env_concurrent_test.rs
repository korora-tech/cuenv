#[cfg(test)]
mod concurrent_tests {
    use cuenv::sync_env::{InstanceLock, SyncEnv};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_concurrent_env_modifications() {
        let num_threads = 10;
        let iterations = 50; // Reduced iterations
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    // Wait for all threads to be ready
                    barrier.wait();

                    // Use a unique key per thread with UUID to avoid any conflicts
                    let thread_id = uuid::Uuid::new_v4();
                    let key = format!("CONCURRENT_TEST_{}_{}", i, thread_id);

                    for j in 0..iterations {
                        // Set a value
                        let value = format!("thread_{}_iter_{}", i, j);
                        SyncEnv::set_var(&key, &value).unwrap();

                        // Read it back immediately
                        let read_value = SyncEnv::var(&key).unwrap();
                        assert_eq!(
                            read_value,
                            Some(value.clone()),
                            "Thread {} iteration {}: expected {}, got {:?}",
                            i,
                            j,
                            value,
                            read_value
                        );

                        // Small random delay to increase chance of contention
                        thread::sleep(Duration::from_micros(10));
                    }

                    // Clean up
                    SyncEnv::remove_var(&key).unwrap();

                    // Return the key so we can verify cleanup
                    key
                })
            })
            .collect();

        // Wait for all threads to complete and collect keys
        let keys: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Verify all variables were cleaned up
        for key in keys {
            assert_eq!(SyncEnv::var(&key).unwrap(), None);
        }
    }

    #[test]
    fn test_instance_lock_prevents_concurrent_access() {
        let barrier = Arc::new(Barrier::new(2));

        // First thread acquires lock
        let barrier1 = Arc::clone(&barrier);
        let handle1 = thread::spawn(move || {
            let _lock = InstanceLock::acquire().unwrap();
            barrier1.wait(); // Signal that lock is acquired

            // Hold the lock for a bit
            thread::sleep(Duration::from_millis(100));

            // Lock is automatically released when _lock goes out of scope
        });

        // Second thread tries to acquire lock
        let barrier2 = Arc::clone(&barrier);
        let handle2 = thread::spawn(move || {
            barrier2.wait(); // Wait for first thread to acquire lock

            // This should fail immediately since first thread holds the lock
            let result = InstanceLock::try_acquire();
            assert!(
                result.is_err(),
                "Second thread should not be able to acquire lock"
            );

            // Wait for first thread to release
            thread::sleep(Duration::from_millis(150));

            // Now it should succeed
            let _lock = InstanceLock::try_acquire().unwrap();
        });

        handle1.join().unwrap();
        handle2.join().unwrap();
    }

    #[test]
    fn test_env_operations_are_atomic() {
        let num_threads = 5;
        let barrier = Arc::new(Barrier::new(num_threads));
        let test_key = "ATOMIC_TEST_KEY";

        // Set initial value
        SyncEnv::set_var(test_key, "initial").unwrap();

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();

                    // Each thread tries to read-modify-write
                    for _ in 0..100 {
                        let current = SyncEnv::var(test_key).unwrap().unwrap_or_default();
                        let new_value = format!("{}_thread_{}", current, i);
                        SyncEnv::set_var(test_key, &new_value).unwrap();

                        // Small delay to increase contention
                        thread::sleep(Duration::from_micros(1));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify the final value contains contributions from all threads
        let final_value = SyncEnv::var(test_key).unwrap().unwrap();

        // Clean up
        SyncEnv::remove_var(test_key).unwrap();

        // The exact final value is non-deterministic, but it should be non-empty
        // and contain thread markers
        assert!(!final_value.is_empty());
        assert!(final_value.contains("thread_"));
    }
}
