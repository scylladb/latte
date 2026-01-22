use std::process::Command;
use std::time::Duration;
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage};

#[test]
fn test_latte_with_scylladb() {
    // Start ScyllaDB container
    let scylla_image = GenericImage::new("scylladb/scylla", "6.2")
        .with_wait_for(WaitFor::message_on_stdout("init - serving"));
    
    let container = scylla_image.start().expect("Failed to start ScyllaDB container");
    let port = container.get_host_port_ipv4(9042).expect("Failed to get ScyllaDB port");
    
    // Give ScyllaDB a bit more time to be fully ready
    std::thread::sleep(Duration::from_secs(10));
    
    // Build latte if not already built
    let build_status = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("Failed to build latte");
    
    assert!(build_status.success(), "Failed to build latte");
    
    // Run latte with the write workload for 1 minute
    let latte_binary = format!("{}/target/release/latte", env!("CARGO_MANIFEST_DIR"));
    let workload_path = format!("{}/workloads/basic/write.rn", env!("CARGO_MANIFEST_DIR"));
    let hosts = format!("127.0.0.1:{}", port);
    
    let output = Command::new(&latte_binary)
        .args([
            "run",
            &workload_path,
            "--hosts", &hosts,
            "--duration", "60s",
            "--warmup", "0s",
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
    
    // Verify that some output was generated (indicating the test ran)
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout_str.contains("Throughput") || stdout_str.contains("ops/s"),
        "Expected throughput metrics in output"
    );
}
