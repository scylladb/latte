use std::process::Command;
use std::time::Duration;
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage};

/// Additional time to wait for ScyllaDB to be fully ready after container start
const SCYLLA_READY_WAIT_SECS: u64 = 10;

/// Integration test with ScyllaDB
/// This test is ignored by default and should be run explicitly with:
/// `cargo test --test integration_test -- --ignored`
#[test]
#[ignore]
fn test_latte_with_scylladb() {
    // Get ScyllaDB version from environment variable, default to "2025.1.0" (stable version)
    let scylla_version = std::env::var("SCYLLA_VERSION").unwrap_or_else(|_| "2025.1.0".to_string());

    println!(
        "Starting ScyllaDB container with version: {}",
        scylla_version
    );

    // Start ScyllaDB container using GenericImage with proper wait condition
    // Based on the official testcontainers-modules ScyllaDB implementation
    let scylla_image = GenericImage::new("scylladb/scylla", &scylla_version)
        .with_wait_for(WaitFor::message_on_stderr("init - serving"));

    let scylla_container = scylla_image
        .start()
        .expect("Failed to start ScyllaDB container");

    println!("ScyllaDB container started successfully");

    let port = scylla_container
        .get_host_port_ipv4(9042)
        .expect("Failed to get ScyllaDB port");

    println!("ScyllaDB listening on port: {}", port);

    // Give ScyllaDB time to be fully ready - ScyllaDB can take a while to initialize
    println!(
        "Waiting {} seconds for ScyllaDB to be fully ready...",
        SCYLLA_READY_WAIT_SECS
    );
    std::thread::sleep(Duration::from_secs(SCYLLA_READY_WAIT_SECS));

    // Run latte with the write workload for 1 minute
    // Note: The binary should be pre-built by the test runner (e.g., in CI or via `cargo build --release`)
    let latte_binary = format!("{}/target/release/latte", env!("CARGO_MANIFEST_DIR"));
    let workload_path = format!("{}/workloads/basic/write.rn", env!("CARGO_MANIFEST_DIR"));
    let hosts = format!("127.0.0.1:{}", port);

    let output = Command::new(&latte_binary)
        .args([
            "run",
            &workload_path,
            "--hosts",
            &hosts,
            "--duration",
            "60s",
            "--warmup",
            "0s",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to run latte");

    // Print output for debugging
    println!("Latte stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("Latte stderr:\n{}", String::from_utf8_lossy(&output.stderr));

    // Check that latte completed successfully
    assert!(
        output.status.success(),
        "Latte failed with status: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify that output was generated (a successful run should produce some output)
    // We check for either presence of output on stdout or stderr, as the exact format may vary
    let has_output = !output.stdout.is_empty() || !output.stderr.is_empty();
    assert!(
        has_output,
        "Expected latte to produce some output, but both stdout and stderr are empty"
    );
}
