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

/// Extracts an integer stat from a summary row such as "Errors [op] 0".
fn summary_stat(result: &CommandResult, label: &str) -> u64 {
    result
        .output
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            if words.next() == Some(label) {
                words.last().map(str::to_string)
            } else {
                None
            }
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "no integer {label} stat in latte output:\n{}",
                result.output
            )
        })
}

/// A `run` command exits successfully even when workload operations fail, so
/// checking the exit status is not enough - the error count must be zero.
fn assert_no_errors(result: &CommandResult) {
    let errors = summary_stat(result, "Errors");
    assert_eq!(
        errors, 0,
        "latte reported {errors} errors:\n{}",
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

/// Writes a mini binary dataset: header (count, dimension as u32 LE), then
/// count * dimension little-endian f32 values.
fn binary_dataset_file(count: u32, dim: u32) -> tempfile::NamedTempFile {
    let mut data = Vec::with_capacity(8 + (count * dim * 4) as usize);
    data.extend_from_slice(&count.to_le_bytes());
    data.extend_from_slice(&dim.to_le_bytes());
    for i in 0..count * dim {
        data.extend_from_slice(&(i as f32).to_le_bytes());
    }
    let datafile = tempfile::NamedTempFile::new().expect("temp data file");
    std::fs::write(datafile.path(), &data).expect("write data file");
    datafile
}

#[tokio::test]
#[ignore]
async fn test_latte_alternator_binary_file_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Alternator;
    let workload = workload_path("integration_tests/binary_file_alternator.rn");
    let duration = "10000";

    let datafile = binary_dataset_file(1000, 4);
    let datafile_param = format!("datafile=\"{}\"", datafile.path().display());

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    latte.schema(&db, &workload, &["-P", datafile_param.as_str()]);

    println!("\n[TEST-INFO] Phase 2: Insert records read from the binary file");
    // More than one latte thread, so the per-worker file handle opening runs
    // on worker-cloned contexts.
    let insert_result = latte.run(
        &db,
        &workload,
        duration,
        &["-f=insert", "--threads", "2", "-P", datafile_param.as_str()],
    );
    assert_latte_success(&insert_result);
    assert_no_errors(&insert_result);
    assert_has_throughput_metrics(&insert_result);

    println!("\n[TEST-INFO] Phase 3: Read the inserted items back");
    let get_result = latte.run(
        &db,
        &workload,
        duration,
        &["-f=get", "--threads", "2", "-P", datafile_param.as_str()],
    );
    assert_latte_success(&get_result);
    assert_no_errors(&get_result);
    assert_eq!(
        summary_stat(&get_result, "Rows"),
        summary_stat(&get_result, "Requests"),
        "some reads found no row:\n{}",
        get_result.output
    );
    assert_has_throughput_metrics(&get_result);
}

#[tokio::test]
#[ignore]
async fn test_latte_cql_binary_file_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Cql;
    let workload = workload_path("integration_tests/binary_file.rn");
    let duration = "10000";

    let datafile = binary_dataset_file(1000, 4);
    let datafile_param = format!("datafile=\"{}\"", datafile.path().display());

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    let rf_args: &[&str] = match db._container {
        Some(_) => &["-P", "replication_factor=1"], // Running in a single container
        None => &[],
    };
    let schema_args = [&["-P", datafile_param.as_str()], rf_args].concat();
    latte.schema(&db, &workload, &schema_args);

    println!("\n[TEST-INFO] Phase 2: Insert records read from the binary file");
    // More than one latte thread, so the per-worker file handle opening runs
    // on worker-cloned contexts.
    let insert_result = latte.run(
        &db,
        &workload,
        duration,
        &["-f=insert", "--threads", "2", "-P", datafile_param.as_str()],
    );
    assert_latte_success(&insert_result);
    assert_no_errors(&insert_result);
    assert_has_throughput_metrics(&insert_result);

    println!("\n[TEST-INFO] Phase 3: Read the inserted rows back");
    let get_result = latte.run(
        &db,
        &workload,
        duration,
        &["-f=get", "--threads", "2", "-P", datafile_param.as_str()],
    );
    assert_latte_success(&get_result);
    assert_no_errors(&get_result);
    assert_eq!(
        summary_stat(&get_result, "Rows"),
        summary_stat(&get_result, "Requests"),
        "some reads found no row:\n{}",
        get_result.output
    );
    assert_has_throughput_metrics(&get_result);
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

/// Tests that `return` inside if/else and match arms in async functions
/// compiles and executes correctly. This directly exercises the rune compiler
/// divergence fix for both expr_if and expr_match
/// Ref: https://github.com/rune-rs/rune/issues/1016
#[tokio::test]
#[ignore]
async fn test_rune_return_in_diverging_branches() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Cql;
    let workload = workload_path("integration_tests/return_statement.rn");
    let duration = "5000";

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    latte.schema(&db, &workload, &[]);

    println!("\n[TEST-INFO] Phase 2: Write (exercises return in if/else and match)");
    let write_result = latte.run(&db, &workload, duration, &["-f", "write"]);
    assert_latte_success(&write_result);
    assert_has_throughput_metrics(&write_result);

    println!("\n[TEST-INFO] Phase 3: Read (exercises return in if/else)");
    let read_result = latte.run(&db, &workload, duration, &["-f", "read"]);
    assert_latte_success(&read_result);
    assert_has_throughput_metrics(&read_result);
}

/// Covers the rows number validation of the 'execute_prepared_with_validation'
/// and 'execute_with_validation' context functions against multi-row partitions
/// of different sizes. Its 'get_many' and 'count' functions pass the unsigned
/// 'partition.rows_num' value as a validation argument, which used to be
/// rejected by the validation arguments parser.
#[tokio::test]
#[ignore]
async fn test_latte_cql_row_count_validation_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Cql;
    let workload = workload_path("row_count_validation.rn");
    let duration = "1000";

    // 100 partitions of 4 rows and 100 partitions of 6 rows
    #[rustfmt::skip]
    let mut args: Vec<&str> = vec![
        "-P", "row_count=1000",
        "-P", "rows_per_partition=1",
        "-P", "partition_sizes=\"50:4,50:6\"",
    ];
    if db._container.is_some() {
        args.extend(["-P", "replication_factor=1"]); // Running in a single container
    }

    println!("\n[TEST-INFO] Phase 1: Create the schema ({:?})", latte);
    latte.schema(&db, &workload, &args);

    println!("\n[TEST-INFO] Phase 2: Data population");
    let mut populate_args = args.clone();
    populate_args.push("-f=insert");
    let populate_result = latte.run(&db, &workload, duration, &populate_args);
    assert_latte_success(&populate_result);
    assert_no_errors(&populate_result);
    assert_has_throughput_metrics(&populate_result);

    // Both statement kinds go through the same validation arguments parser.
    for use_prepared_statements in ["true", "false"] {
        for function in ["get", "get_many", "count"] {
            println!(
                "\n[TEST-INFO] Phase 3: Data validation using '{function}' \
                 (use_prepared_statements={use_prepared_statements})"
            );
            let mut validation_args = args.clone();
            let function_arg = format!("-f={function}");
            let prepared_arg = format!("use_prepared_statements={use_prepared_statements}");
            validation_args.push(&function_arg);
            validation_args.extend(["-P", &prepared_arg]);
            let validation_result = latte.run(&db, &workload, duration, &validation_args);
            assert_latte_success(&validation_result);
            assert_no_errors(&validation_result);
        }
    }
}
