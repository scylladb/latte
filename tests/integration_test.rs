use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const CQL_PORT: u16 = 9042;

struct ScyllaDb {
    _container: Option<ContainerAsync<GenericImage>>,
    host: String,
    port: u16,
}

type StartResult = Result<ScyllaDb, String>;

static SCYLLA: OnceLock<StartResult> = OnceLock::new();
static LATTE_BUILT: OnceLock<bool> = OnceLock::new();

fn scylla() -> &'static ScyllaDb {
    SCYLLA
        .get_or_init(|| {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(start_scylla())
        })
        .as_ref()
        .expect("ScyllaDB container is not available")
}

async fn start_scylla() -> StartResult {
    if let Ok(host) = std::env::var("SCYLLA_TEST_HOST") {
        let port = std::env::var("SCYLLA_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(CQL_PORT);
        eprintln!("Using pre-existing ScyllaDB at {}:{}", host, port);
        return Ok(ScyllaDb {
            _container: None,
            host,
            port,
        });
    }

    let image_registry =
        std::env::var("SCYLLADB_IMAGE_REGISTRY").unwrap_or_else(|_| "scylladb/scylla".into());
    let image_tag = std::env::var("SCYLLADB_IMAGE_TAG").unwrap_or_else(|_| "latest".into());
    eprintln!("Starting {}:{}", image_registry, image_tag);

    let container = GenericImage::new(&image_registry, &image_tag)
        .with_exposed_port(CQL_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stderr("serving"))
        .with_cmd(vec![
            "--smp".to_string(),
            "1".to_string(),
            "--memory".to_string(),
            "512M".to_string(),
            "--overprovisioned".to_string(),
            "1".to_string(),
            "--skip-wait-for-gossip-to-settle".to_string(),
            "0".to_string(),
        ])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .map_err(|e| format!("failed to start ScyllaDB container: {e}"))?;

    let port = container
        .get_host_port_ipv4(CQL_PORT.tcp())
        .await
        .map_err(|e| format!("failed to get mapped port: {e}"))?;

    let host = String::from("127.0.0.1");

    wait_for_cql(&host, port).await;

    Ok(ScyllaDb {
        _container: Some(container),
        host,
        port,
    })
}

async fn wait_for_cql(host: &str, port: u16) {
    let addr = format!("{}:{}", host, port);
    for attempt in 0..60 {
        if TcpStream::connect(&addr).is_ok() {
            eprintln!("ScyllaDB ready on {} (attempt {})", addr, attempt + 1);
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        if attempt > 0 && attempt % 10 == 0 {
            eprintln!("Still waiting for ScyllaDB on {}...", addr);
        }
    }
    panic!("ScyllaDB did not become ready on {} within 120s", addr);
}

fn ensure_latte_built() {
    LATTE_BUILT.get_or_init(|| {
        let status = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("Failed to invoke cargo build");
        assert!(status.success(), "cargo build --release failed");
        true
    });
}

fn latte_binary() -> String {
    format!("{}/target/release/latte", env!("CARGO_MANIFEST_DIR"))
}

fn hosts_arg(db: &ScyllaDb) -> String {
    format!("{}:{}", db.host, db.port)
}

fn workload_path(name: &str) -> String {
    format!("{}/workloads/{}", env!("CARGO_MANIFEST_DIR"), name)
}

struct CommandResult {
    status: ExitStatus,
    output: String,
}

fn run_command(mut cmd: Command) -> CommandResult {
    unsafe {
        cmd.pre_exec(|| {
            // Redirect stderr (fd 2) to stdout (fd 1) so both streams share one pipe
            extern "C" {
                fn dup2(oldfd: i32, newfd: i32) -> i32;
            }
            if dup2(1, 2) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let output = cmd
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run command");

    CommandResult {
        status: output.status,
        output: String::from_utf8_lossy(&output.stdout).into_owned(),
    }
}

fn latte_schema(db: &ScyllaDb, workload: &str) -> CommandResult {
    println!(
        "{}",
        format_args!(
            "Running the 'latte schema' command for the '{}' rune script",
            workload
        )
    );
    let mut cmd = Command::new(latte_binary());
    cmd.args(["schema", &workload_path(workload), &hosts_arg(db)])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    let result = run_command(cmd);
    assert!(
        result.status.success(),
        "latte schema failed:\n{}",
        result.output
    );
    result
}

fn latte_run(db: &ScyllaDb, workload: &str, duration: &str, extra_args: &[&str]) -> CommandResult {
    let workload = workload_path(workload);
    let hosts = hosts_arg(db);
    let mut args: Vec<&str> = vec![
        "run",
        &workload,
        &hosts,
        "--duration",
        &duration,
        "--warmup",
        "0s",
        "-q",
    ];
    args.extend_from_slice(extra_args);

    println!(
        "{}",
        format_args!(
            "Running the 'latte run' command with the following params: {:?}",
            &args
        )
    );

    let mut cmd = Command::new(latte_binary());
    cmd.args(&args).current_dir(env!("CARGO_MANIFEST_DIR"));
    let result = run_command(cmd);

    eprintln!("latte output:\n{}", result.output);

    result
}

fn assert_latte_success(result: &CommandResult) {
    assert!(
        result.status.success(),
        "latte failed (exit {:?}):\n{}",
        result.status.code(),
        result.output
    );
}

fn assert_has_throughput_metrics(result: &CommandResult) {
    assert!(
        result.output.contains("thrpt")
            || result.output.contains("op/s")
            || result.output.contains("req/s"),
        "Expected throughput metrics in latte output:\n{}",
        result.output
    );
}

#[test]
#[ignore]
fn test_latte_data_validation_workload() {
    let db = scylla();
    ensure_latte_built();

    let rune_path = "data_validation.rn";
    let duration = "50000";

    println!("\n[TEST-INFO] Phase 1: Create the schema");
    latte_schema(db, rune_path);

    println!("\n[TEST-INFO] Phase 2: Data population");
    let populate_result = latte_run(db, rune_path, duration, &["-f=insert"]);
    assert_latte_success(&populate_result);
    assert_has_throughput_metrics(&populate_result);

    println!("\n[TEST-INFO] Phase 3: Data validation");
    let data_validation_result = latte_run(db, rune_path, duration, &["-f=get_by_ck"]);
    assert_latte_success(&data_validation_result);
    assert_has_throughput_metrics(&data_validation_result);
}
