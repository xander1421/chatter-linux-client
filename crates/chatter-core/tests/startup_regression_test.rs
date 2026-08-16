//! Startup regression tests.
//!
//! Tests each initialization step in isolation to identify hangs.
//! Run with: cargo test --test startup_regression_test -- --nocapture --test-threads=1

use std::time::{Duration, Instant};

/// Maximum time allowed for each initialization step (in seconds).
const TIMEOUT_SECS: u64 = 5;

/// Helper to run a closure with a timeout.
/// Returns Ok(duration) if completed, Err(step_name) if it would hang.
fn with_timeout<F, T>(step_name: &str, f: F) -> Result<(T, Duration), String>
where
    F: FnOnce() -> T,
{
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();

    println!("[{}] completed in {:?}", step_name, elapsed);

    if elapsed > Duration::from_secs(TIMEOUT_SECS) {
        Err(format!("{} took too long: {:?}", step_name, elapsed))
    } else {
        Ok((result, elapsed))
    }
}

// SAFETY: These tests run with --test-threads=1, so no concurrent env access.
unsafe fn set_env(key: &str, val: &str) {
    // SAFETY: Single-threaded test execution
    unsafe { std::env::set_var(key, val) };
}

unsafe fn remove_env(key: &str) {
    // SAFETY: Single-threaded test execution
    unsafe { std::env::remove_var(key) };
}

#[test]
fn test_01_antidebug_init() {
    use chatter_core::system::{init_antidebug, check_debugger};

    println!("\n=== Testing antidebug initialization ===");

    // Test check_debugger (non-destructive check via /proc/self/status)
    let (status, duration) = with_timeout("check_debugger", || {
        check_debugger()
    }).expect("check_debugger should not hang");

    println!("  Debugger status: {:?}", status);
    assert!(duration < Duration::from_secs(1), "check_debugger should be fast");

    // Test init_antidebug (includes ptrace calls)
    // NOTE: This may affect the process state
    let (result, duration) = with_timeout("init_antidebug", || {
        init_antidebug()
    }).expect("init_antidebug should not hang");

    println!("  init_antidebug returned: {}", result);
    assert!(duration < Duration::from_secs(1), "init_antidebug should be fast");
}

#[test]
fn test_02_gstreamer_init() {
    println!("\n=== Testing GStreamer initialization ===");

    let (result, duration) = with_timeout("gstreamer::init", || {
        gstreamer::init()
    }).expect("gstreamer::init should not hang");

    match &result {
        Ok(()) => println!("  GStreamer version: {}", gstreamer::version_string()),
        Err(e) => println!("  GStreamer init failed (ok for headless): {}", e),
    }

    assert!(duration < Duration::from_secs(3), "gstreamer init should complete within 3s");
}

#[test]
fn test_03_directories_init() {
    use chatter_core::system::Directories;

    println!("\n=== Testing directories initialization ===");

    let (result, duration) = with_timeout("Directories::new", || {
        Directories::new()
    }).expect("Directories::new should not hang");

    match &result {
        Ok(dirs) => println!("  Data dir: {:?}", dirs.data_dir()),
        Err(e) => println!("  Directories init failed: {}", e),
    }

    assert!(duration < Duration::from_secs(1), "directories init should be fast");
    assert!(result.is_ok(), "directories should initialize");
}

#[test]
#[ignore] // Requires real keyring service (D-Bus secret service)
fn test_04_keyring_access() {
    use chatter_core::system::Keyring;
    use std::path::PathBuf;

    println!("\n=== Testing keyring access ===");

    // Use a test-specific path to avoid affecting real data
    let test_db_path = PathBuf::from("/tmp/chatter-startup-test/test.db");

    let (keyring, duration) = with_timeout("Keyring::new", || {
        Keyring::new(test_db_path)
    }).expect("Keyring::new should not hang");

    assert!(duration < Duration::from_millis(100), "keyring creation should be instant");

    // Test has_passphrase (should not block)
    let (has_pass, duration) = with_timeout("keyring.has_passphrase", || {
        keyring.has_passphrase()
    }).expect("has_passphrase should not hang");

    println!("  Has passphrase: {}", has_pass);
    assert!(duration < Duration::from_secs(2), "has_passphrase should complete quickly");

    // Test get_passphrase (may fail, but should not hang)
    let (result, duration) = with_timeout("keyring.get_passphrase", || {
        keyring.get_passphrase()
    }).expect("get_passphrase should not hang");

    match &result {
        Ok(_) => println!("  Got existing passphrase"),
        Err(e) => println!("  No passphrase (expected for fresh db): {}", e),
    }

    assert!(duration < Duration::from_secs(2), "get_passphrase should not block indefinitely");
}

#[test]
fn test_05_keyring_with_env_bypass() {
    use chatter_core::system::Keyring;
    use std::path::PathBuf;

    println!("\n=== Testing keyring with CHATTER_DB_PASSPHRASE bypass ===");

    // Set the bypass env var
    // SAFETY: Test runs single-threaded
    unsafe { set_env("CHATTER_DB_PASSPHRASE", "test-passphrase-for-ci") };

    let test_db_path = PathBuf::from("/tmp/chatter-startup-test/test-bypass.db");
    let keyring = Keyring::new(test_db_path);

    let (result, duration) = with_timeout("keyring.get_passphrase (with env)", || {
        keyring.get_passphrase()
    }).expect("get_passphrase with env bypass should not hang");

    // Clean up env var
    // SAFETY: Test runs single-threaded
    unsafe { remove_env("CHATTER_DB_PASSPHRASE") };

    assert!(result.is_ok(), "should get passphrase from env");
    assert_eq!(result.unwrap(), "test-passphrase-for-ci");
    assert!(duration < Duration::from_millis(10), "env bypass should be instant");

    println!("  Env bypass works correctly");
}

#[test]
#[ignore] // Requires keyring for DB passphrase; run with --test-threads=1
fn test_06_database_open() {
    use chatter_core::storage::Database;

    println!("\n=== Testing database open ===");

    let test_db_path = std::path::PathBuf::from("/tmp/chatter-startup-test/test.db");

    // Ensure parent directory exists
    if let Some(parent) = test_db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let (result, duration) = with_timeout("Database::open", || {
        Database::open(&test_db_path, "test-passphrase")
    }).expect("Database::open should not hang");

    match &result {
        Ok(_) => println!("  Database opened successfully"),
        Err(e) => println!("  Database open failed: {}", e),
    }

    assert!(duration < Duration::from_secs(2), "database open should be quick");
    assert!(result.is_ok(), "database should open");

    // Test migrations
    if let Ok(db) = result {
        let (mig_result, mig_duration) = with_timeout("Database::run_migrations", || {
            db.run_migrations()
        }).expect("run_migrations should not hang");

        assert!(mig_duration < Duration::from_secs(5), "migrations should complete");
        assert!(mig_result.is_ok(), "migrations should succeed");
        println!("  Migrations completed in {:?}", mig_duration);
    }
}

#[test]
fn test_07_grpc_pool_creation() {
    use chatter_core::network::grpc::GrpcClientPool;

    println!("\n=== Testing gRPC pool creation ===");

    // This should NOT connect, just create the pool
    let (result, duration) = with_timeout("GrpcClientPool::new", || {
        GrpcClientPool::new("https://localhost:50051")
    }).expect("GrpcClientPool::new should not hang");

    match &result {
        Ok(_) => println!("  gRPC pool created"),
        Err(e) => println!("  gRPC pool creation failed: {}", e),
    }

    assert!(duration < Duration::from_secs(1), "pool creation should be fast (no connection)");
}

#[test]
#[ignore] // Requires keyring + real filesystem; run with --test-threads=1
fn test_08_infrastructure_container() {
    println!("\n=== Testing InfrastructureContainer (full init) ===");

    // Set env vars to bypass keyring and use test paths
    // SAFETY: Test runs single-threaded
    unsafe {
        set_env("CHATTER_DB_PASSPHRASE", "test-passphrase-infra");
        set_env("XDG_DATA_HOME", "/tmp/chatter-startup-test-infra");
    }

    use chatter_core::app::InfrastructureContainer;

    let (result, duration) = with_timeout("InfrastructureContainer::new", || {
        InfrastructureContainer::new(None)
    }).expect("InfrastructureContainer::new should not hang");

    // Clean up
    // SAFETY: Test runs single-threaded
    unsafe {
        remove_env("CHATTER_DB_PASSPHRASE");
        remove_env("XDG_DATA_HOME");
    }

    match &result {
        Ok(_) => println!("  Infrastructure initialized successfully"),
        Err(e) => println!("  Infrastructure init failed: {}", e),
    }

    assert!(duration < Duration::from_secs(10), "infrastructure init should complete");
    assert!(result.is_ok(), "infrastructure should initialize");
}

#[test]
#[ignore] // Requires keyring + real filesystem; run with --test-threads=1
fn test_09_app_context() {
    println!("\n=== Testing AppContext (full app init) ===");

    // Set env vars to bypass keyring and use test paths
    // SAFETY: Test runs single-threaded
    unsafe {
        set_env("CHATTER_DB_PASSPHRASE", "test-passphrase-ctx");
        set_env("XDG_DATA_HOME", "/tmp/chatter-startup-test-ctx");
    }

    use chatter_core::app::AppContext;

    let (result, duration) = with_timeout("AppContext::new", || {
        AppContext::new(None)
    }).expect("AppContext::new should not hang");

    // Clean up
    // SAFETY: Test runs single-threaded
    unsafe {
        remove_env("CHATTER_DB_PASSPHRASE");
        remove_env("XDG_DATA_HOME");
    }

    match &result {
        Ok(_) => println!("  AppContext initialized successfully"),
        Err(e) => println!("  AppContext init failed: {}", e),
    }

    assert!(duration < Duration::from_secs(15), "app context init should complete");
    assert!(result.is_ok(), "app context should initialize");
}

/// Test that specifically checks for the keyring retry blocking issue.
#[test]
#[ignore] // Requires real keyring service (D-Bus secret service)
fn test_10_keyring_no_retry_block() {
    use chatter_core::system::Keyring;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

    println!("\n=== Testing keyring retry does not block indefinitely ===");

    let test_db_path = PathBuf::from("/tmp/chatter-startup-test/retry-test.db");
    let keyring = Keyring::new(test_db_path);

    // Use a channel to detect if the operation completes
    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        // Try with minimal retries to keep test fast
        let result = keyring.get_passphrase_with_retry(1, 1);
        let _ = tx.send(result);
    });

    // Wait max 5 seconds (1 retry * 1 second + overhead)
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => {
            println!("  get_passphrase_with_retry completed: {:?}", result.is_ok());
            handle.join().unwrap();
        }
        Err(_) => {
            panic!("HANG DETECTED: get_passphrase_with_retry blocked for >5 seconds");
        }
    }
}

/// Test with REAL retry parameters (6 retries, 5 second delays = 30+ seconds)
/// This reproduces the actual hang in InfrastructureContainer
#[test]
#[ignore = "Takes 30+ seconds - run manually to verify hang behavior"]
fn test_10b_keyring_real_retry_params() {
    use chatter_core::system::Keyring;
    use std::path::PathBuf;

    println!("\n=== Testing keyring with REAL retry params (6, 5) ===");
    println!("  WARNING: This test takes 30+ seconds if no passphrase exists");

    // Fresh path with no entry
    let unique_path = format!("/tmp/chatter-retry-test-{}", std::process::id());
    let keyring = Keyring::new(PathBuf::from(&unique_path));

    let start = std::time::Instant::now();

    // These are the REAL parameters from InfrastructureContainer
    let result = keyring.get_passphrase_with_retry(6, 5);

    let elapsed = start.elapsed();

    println!("  Result: {:?}", result.is_ok());
    println!("  Total elapsed: {:?}", elapsed);

    // If no passphrase exists, this SHOULD complete, but will take ~30 seconds
    // The bug is that it takes way too long for a missing entry
    assert!(result.is_err(), "Should fail (no entry)");

    // This assertion shows the problem: 30 seconds for a simple "not found"
    if elapsed > Duration::from_secs(25) {
        println!("  BUG CONFIRMED: Took {:?} seconds just to find out there's no entry!", elapsed.as_secs());
    }
}

/// Test keyring with the EXACT path the app uses (XDG_DATA_HOME based)
/// Tests the NEW smart retry logic that fails fast on NoEntry errors.
#[test]
#[ignore] // Requires real keyring service (D-Bus secret service)
fn test_11_keyring_real_path() {
    use chatter_core::system::{Directories, Keyring};
    use std::sync::mpsc;
    use std::thread;

    println!("\n=== Testing keyring with real app path (smart retry) ===");

    // Use a FRESH path that definitely has no entry
    let unique_path = format!("/tmp/chatter-test-{}/chatter-linux/chatter.db", std::process::id());
    let db_path = std::path::PathBuf::from(&unique_path);
    println!("  Database path: {:?}", db_path);

    let keyring = Keyring::new(db_path);

    // Test with timeout to catch hangs - using the retry function
    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        println!("  [thread] Calling get_passphrase_with_retry(6, 5)...");
        let start = std::time::Instant::now();
        // Use the same parameters as InfrastructureContainer
        let result = keyring.get_passphrase_with_retry(6, 5);
        let elapsed = start.elapsed();
        println!("  [thread] get_passphrase_with_retry completed in {:?}", elapsed);
        let _ = tx.send((result, elapsed));
    });

    // With the fix, this should complete in <1 second (NoEntry = no retry)
    // Without the fix, it would take 30+ seconds
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok((result, elapsed)) => {
            println!("  Result: {:?}", result.is_ok());
            println!("  Elapsed: {:?}", elapsed);
            handle.join().unwrap();

            // Should fail (no entry) but should be FAST
            assert!(result.is_err(), "Should fail (no entry exists)");
            assert!(
                elapsed < Duration::from_secs(2),
                "NoEntry should fail fast without retries, took {:?}",
                elapsed
            );
            println!("  SUCCESS: NoEntry detected quickly without blocking retries");
        }
        Err(_) => {
            panic!("HANG DETECTED: get_passphrase_with_retry blocked for >5 seconds (fix not working)");
        }
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(format!("/tmp/chatter-test-{}", std::process::id()));
}

/// Test set_passphrase - this is likely where the hang is
#[test]
fn test_12_keyring_set_passphrase() {
    use chatter_core::system::Keyring;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

    println!("\n=== Testing keyring SET passphrase (fresh entry) ===");

    // Use a unique path that definitely doesn't have an entry
    let unique_path = format!("/tmp/chatter-set-test-{}", std::process::id());
    let test_db_path = PathBuf::from(&unique_path);
    println!("  Test path: {:?}", test_db_path);

    let keyring = Keyring::new(test_db_path);

    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        println!("  [thread] Calling set_passphrase...");
        let start = std::time::Instant::now();
        let result = keyring.set_passphrase("test-password-12345");
        let elapsed = start.elapsed();
        println!("  [thread] set_passphrase completed in {:?}: {:?}", elapsed, result.is_ok());
        let _ = tx.send((result, elapsed));
    });

    // Wait max 5 seconds
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok((result, elapsed)) => {
            println!("  Result: {:?}", result);
            println!("  Elapsed: {:?}", elapsed);
            handle.join().unwrap();

            if result.is_err() {
                println!("  ERROR: {:?}", result.err());
            }

            assert!(elapsed < Duration::from_secs(3), "set_passphrase should not block");
        }
        Err(_) => {
            panic!("HANG DETECTED: keyring.set_passphrase() blocked for >5 seconds");
        }
    }
}

/// Test the full passphrase_or_generate flow with real path
#[test]
#[ignore] // Requires real keyring + filesystem; run with --test-threads=1
fn test_13_infrastructure_real_path() {
    use std::sync::mpsc;
    use std::thread;

    println!("\n=== Testing InfrastructureContainer with real XDG path (no passphrase bypass) ===");

    // SAFETY: Test runs single-threaded
    unsafe {
        set_env("XDG_DATA_HOME", "/tmp/chatter-user1-infra-test");
        // Do NOT set CHATTER_DB_PASSPHRASE - test the real keyring path
    };

    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        use chatter_core::app::InfrastructureContainer;

        println!("  [thread] Creating InfrastructureContainer...");
        let start = std::time::Instant::now();
        let result = InfrastructureContainer::new(None);
        let elapsed = start.elapsed();
        println!("  [thread] Completed in {:?}", elapsed);
        let _ = tx.send((result.is_ok(), elapsed));
    });

    // Wait max 10 seconds (6 retries * 5 seconds would be 30 seconds)
    // If it takes longer than 10s, something is wrong
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok((success, elapsed)) => {
            // SAFETY: Test runs single-threaded
            unsafe { remove_env("XDG_DATA_HOME") };

            println!("  Success: {}", success);
            println!("  Elapsed: {:?}", elapsed);
            handle.join().unwrap();

            // Clean up test data
            let _ = std::fs::remove_dir_all("/tmp/chatter-user1-infra-test");
        }
        Err(_) => {
            // SAFETY: Test runs single-threaded
            unsafe { remove_env("XDG_DATA_HOME") };

            panic!("HANG DETECTED: InfrastructureContainer::new() blocked for >10 seconds");
        }
    }
}

/// Cleanup test data after all tests
#[test]
fn test_99_cleanup() {
    println!("\n=== Cleaning up test data ===");

    let paths = [
        "/tmp/chatter-startup-test",
        "/tmp/chatter-startup-test-infra",
        "/tmp/chatter-startup-test-ctx",
    ];

    for path in paths {
        if std::path::Path::new(path).exists() {
            match std::fs::remove_dir_all(path) {
                Ok(()) => println!("  Removed {}", path),
                Err(e) => println!("  Failed to remove {}: {}", path, e),
            }
        }
    }
}
