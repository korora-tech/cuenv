//! Integration tests for cleanup and error recovery

use cuenv::cleanup::{ProcessGuard, TempDirGuard, TempFileGuard};
use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn test_temp_file_cleanup_on_error() {
    let temp_dir = tempdir().unwrap();
    let file_path = temp_dir.path().join("test.txt");

    let result = std::panic::catch_unwind(|| {
        let guard = TempFileGuard::new(file_path.clone());
        File::create(guard.path())
            .unwrap()
            .write_all(b"test")
            .unwrap();
        assert!(guard.path().exists());
        panic!("Simulated error");
    });

    assert!(result.is_err());
    // File should be cleaned up even after panic
    assert!(!file_path.exists());
}

#[test]
fn test_temp_dir_cleanup_on_error() {
    let temp_dir = tempdir().unwrap();
    let dir_path = temp_dir.path().join("test_dir");

    let result = std::panic::catch_unwind(|| {
        let guard = TempDirGuard::new(dir_path.clone()).unwrap();

        // Create some files in the directory
        fs::write(guard.path().join("file1.txt"), "content1").unwrap();
        fs::write(guard.path().join("file2.txt"), "content2").unwrap();

        assert!(guard.path().exists());
        assert!(guard.path().join("file1.txt").exists());

        panic!("Simulated error");
    });

    assert!(result.is_err());
    // Directory and all contents should be cleaned up
    assert!(!dir_path.exists());
}

#[test]
fn test_process_cleanup_on_drop() {
    // Create a long-running process
    let child = Command::new("sleep").arg("30").spawn().unwrap();

    let pid = child.id();

    {
        let _guard = ProcessGuard::new(child, Duration::from_secs(60));

        // Verify process is running
        #[cfg(unix)]
        {
            let check = Command::new("kill")
                .args(&["-0", &pid.to_string()])
                .status()
                .unwrap();
            assert!(check.success());
        }
    }

    // After guard is dropped, process should be terminated
    std::thread::sleep(Duration::from_millis(200));

    #[cfg(unix)]
    {
        let check = Command::new("kill")
            .args(&["-0", &pid.to_string()])
            .status()
            .unwrap();
        assert!(!check.success()); // Process should be gone
    }
}

#[test]
fn test_process_timeout_handling() {
    // Create a process that sleeps for 5 seconds
    let child = Command::new("sleep").arg("5").spawn().unwrap();

    // Set a short timeout
    let mut guard = ProcessGuard::new(child, Duration::from_millis(100));

    // Should timeout
    let result = guard.wait_with_timeout();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("timed out"));
}

#[test]
fn test_nested_cleanup_guards() {
    let temp_dir = tempdir().unwrap();
    let outer_file = temp_dir.path().join("outer.txt");
    let inner_file = temp_dir.path().join("inner.txt");
    let dir_path = temp_dir.path().join("nested_dir");

    let result = std::panic::catch_unwind(|| {
        let _outer_guard = TempFileGuard::new(outer_file.clone());
        File::create(&outer_file).unwrap();

        {
            let _inner_guard = TempFileGuard::new(inner_file.clone());
            File::create(&inner_file).unwrap();

            let _dir_guard = TempDirGuard::new(dir_path.clone()).unwrap();
            fs::write(dir_path.join("nested.txt"), "nested content").unwrap();

            assert!(outer_file.exists());
            assert!(inner_file.exists());
            assert!(dir_path.exists());

            panic!("Nested panic");
        }
    });

    assert!(result.is_err());

    // All resources should be cleaned up
    assert!(!outer_file.exists());
    assert!(!inner_file.exists());
    assert!(!dir_path.exists());
}

#[test]
fn test_multiple_process_cleanup() {
    let mut guards = Vec::new();
    let mut pids = Vec::new();

    // Create multiple processes
    for i in 0..3 {
        let child = Command::new("sleep")
            .arg(format!("{}", 30 + i))
            .spawn()
            .unwrap();

        pids.push(child.id());
        guards.push(ProcessGuard::new(child, Duration::from_secs(60)));
    }

    // Verify all processes are running
    #[cfg(unix)]
    {
        for pid in &pids {
            let check = Command::new("kill")
                .args(&["-0", &pid.to_string()])
                .status()
                .unwrap();
            assert!(check.success());
        }
    }

    // Drop all guards
    drop(guards);

    // Wait a bit for cleanup
    std::thread::sleep(Duration::from_millis(200));

    // Verify all processes are terminated
    #[cfg(unix)]
    {
        for pid in &pids {
            let check = Command::new("kill")
                .args(&["-0", &pid.to_string()])
                .status()
                .unwrap();
            assert!(!check.success());
        }
    }
}

#[test]
fn test_cleanup_with_file_permissions() {
    let temp_dir = tempdir().unwrap();
    let file_path = temp_dir.path().join("readonly.txt");

    {
        let guard = TempFileGuard::new(file_path.clone());
        let mut file = File::create(guard.path()).unwrap();
        file.write_all(b"readonly content").unwrap();
        file.sync_all().unwrap();
        drop(file);

        // Make file read-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(guard.path()).unwrap().permissions();
            perms.set_mode(0o444);
            fs::set_permissions(guard.path(), perms).unwrap();
        }

        // Guard should still attempt cleanup
    }

    // File should be cleaned up (on Unix, this may fail if parent dir is not writable)
    // But the guard should not panic
    if file_path.exists() {
        // Clean up manually for test cleanup
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&file_path, perms).unwrap();
            fs::remove_file(&file_path).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn test_process_group_cleanup() {
    use std::os::unix::process::CommandExt;

    // Create a parent process that spawns children
    let parent = Command::new("sh")
        .arg("-c")
        .arg("sleep 30 & sleep 31 & sleep 32 & wait")
        .process_group(0) // Create new process group
        .spawn()
        .unwrap();

    let pgid = parent.id();

    // Give it time to spawn children
    std::thread::sleep(Duration::from_millis(100));

    {
        let _guard = ProcessGuard::new(parent, Duration::from_secs(60));

        // Verify process group exists
        let check = Command::new("kill")
            .args(&["-0", &format!("-{}", pgid)])
            .status()
            .unwrap();
        assert!(check.success());
    }

    // After guard is dropped, entire process group should be terminated
    std::thread::sleep(Duration::from_millis(200));

    let check = Command::new("kill")
        .args(&["-0", &format!("-{}", pgid)])
        .status()
        .unwrap();
    assert!(!check.success()); // Process group should be gone
}
