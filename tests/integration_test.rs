use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const CQL_PORT: u16 = 9042;
const ALTERNATOR_PORT: u16 = 8000;

struct ScyllaDb {
    _container: Option<ContainerAsync<GenericImage>>,
    host: String,
    cql_port: u16,
    alternator_port: u16,
}

type StartResult = Result<ScyllaDb, String>;

async fn start_scylla() -> StartResult {
    if let Ok(host) = std::env::var("SCYLLA_TEST_HOST") {
        let cql_port = std::env::var("SCYLLA_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(CQL_PORT);
        let alternator_port = std::env::var("SCYLLA_ALTERNATOR_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(ALTERNATOR_PORT);

        eprintln!(
            "Using pre-existing ScyllaDB at {} (CQL: {}, Alternator: {})",
            host, cql_port, alternator_port
        );
        return Ok(ScyllaDb {
            _container: None,
            host,
            cql_port,
            alternator_port,
        });
    }

    let image_registry =
        std::env::var("SCYLLADB_IMAGE_REGISTRY").unwrap_or_else(|_| "scylladb/scylla".into());
    let image_tag = std::env::var("SCYLLADB_IMAGE_TAG").unwrap_or_else(|_| "latest".into());
    eprintln!("Starting {}:{}", image_registry, image_tag);

    let container = GenericImage::new(&image_registry, &image_tag)
        .with_exposed_port(CQL_PORT.tcp())
        .with_exposed_port(ALTERNATOR_PORT.tcp())
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
            "--broadcast-rpc-address".to_string(), // Avoids Latte trying to connect to the container's internal IP
            "127.0.0.1".to_string(),
            "--alternator-port".to_string(),
            ALTERNATOR_PORT.to_string(),
            "--alternator-write-isolation".to_string(),
            "always".to_string(),
        ])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .map_err(|e| format!("failed to start ScyllaDB container: {e}"))?;

    let cql_port = container
        .get_host_port_ipv4(CQL_PORT.tcp())
        .await
        .map_err(|e| format!("failed to get mapped CQL port: {e}"))?;

    let alternator_port = container
        .get_host_port_ipv4(ALTERNATOR_PORT.tcp())
        .await
        .map_err(|e| format!("failed to get mapped Alternator port: {e}"))?;

    let host = String::from("127.0.0.1");

    wait_for_port_readiness(&host, cql_port).await;
    wait_for_port_readiness(&host, alternator_port).await;

    Ok(ScyllaDb {
        _container: Some(container),
        host,
        cql_port,
        alternator_port,
    })
}

async fn wait_for_port_readiness(host: &str, port: u16) {
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

#[derive(Copy, Clone, Debug)]
enum LatteVariant {
    Cql,
    Alternator,
}

impl LatteVariant {
    fn binary_name(&self) -> &'static str {
        match self {
            LatteVariant::Cql => "latte",
            LatteVariant::Alternator => "latte-alternator",
        }
    }

    fn binary_path(&self) -> String {
        format!(
            "{}/target/release/{}",
            env!("CARGO_MANIFEST_DIR"),
            self.binary_name()
        )
    }

    fn ensure_built(&self) {
        static BUILT_CQL: OnceLock<bool> = OnceLock::new();
        static BUILT_ALT: OnceLock<bool> = OnceLock::new();

        let (lock, extra_args): (&OnceLock<bool>, &[&str]) = match self {
            LatteVariant::Cql => (&BUILT_CQL, &[]),
            LatteVariant::Alternator => (
                &BUILT_ALT,
                &["--no-default-features", "--features", "alternator"],
            ),
        };

        lock.get_or_init(|| {
            let status = Command::new("cargo")
                .args(["build", "--release", "--bin", self.binary_name()])
                .args(extra_args)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("Failed to invoke cargo build");

            assert!(
                status.success(),
                "cargo build failed for {}",
                self.binary_name()
            );
            true
        });
    }

    fn endpoint_arg(&self, db: &ScyllaDb) -> String {
        match self {
            LatteVariant::Cql => format!("{}:{}", db.host, db.cql_port),
            LatteVariant::Alternator => format!("http://{}:{}", db.host, db.alternator_port),
        }
    }

    fn schema(&self, db: &ScyllaDb, workload: &str, extra_args: &[&str]) -> CommandResult {
        self.ensure_built();

        let mut cmd = Command::new(self.binary_path());
        cmd.args(["schema", workload, &self.endpoint_arg(db)])
            .args(extra_args)
            .current_dir(env!("CARGO_MANIFEST_DIR"));

        println!("Running '{:?}'", cmd);
        let result = run_command(cmd);

        assert!(
            result.status.success(),
            "'{} schema' failed:\n{}",
            self.binary_name(),
            result.output
        );
        result
    }

    fn run(
        &self,
        db: &ScyllaDb,
        workload: &str,
        duration: &str,
        extra_args: &[&str],
    ) -> CommandResult {
        self.ensure_built();

        let mut cmd = Command::new(self.binary_path());
        cmd.args([
            "run",
            workload,
            &self.endpoint_arg(db),
            "--duration",
            duration,
            "--warmup",
            "0s",
            "-q",
        ])
        .args(extra_args)
        .current_dir(env!("CARGO_MANIFEST_DIR"));

        println!("Running '{:?}'", cmd);
        let result = run_command(cmd);

        eprintln!("'{} run' output:\n{}", self.binary_name(), result.output);
        result
    }
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

#[tokio::test]
#[ignore]
async fn test_latte_cql_data_validation_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Cql;
    let workload = workload_path("data_validation.rn");
    let duration = "50000";

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    let extra_args: &[&str] = match db._container {
        Some(_) => &["-P", "replication_factor=1"], // Running in a single container
        None => &[],
    };
    latte.schema(&db, &workload, extra_args);

    println!("\n[TEST-INFO] Phase 2: Data population");
    let populate_result = latte.run(&db, &workload, duration, &["-f=insert"]);
    assert_latte_success(&populate_result);
    assert_has_throughput_metrics(&populate_result);

    println!("\n[TEST-INFO] Phase 3: Data validation");
    let data_validation_result = latte.run(&db, &workload, duration, &["-f=get_by_ck"]);
    assert_latte_success(&data_validation_result);
    assert_has_throughput_metrics(&data_validation_result);
}

#[tokio::test]
#[ignore]
async fn test_latte_alternator_type_validation_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Alternator;
    let workload = workload_path("alternator/type_validation.rn");
    let duration = "50000";

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    latte.schema(&db, &workload, &[]);

    println!("\n[TEST-INFO] Phase 2: Run type validation workload");
    let result = latte.run(&db, &workload, duration, &[]);
    assert_latte_success(&result);
    assert_has_throughput_metrics(&result);
}
