use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use crate::config::{RetryInterval, ValidationStrategy};
use crate::error::LatteError;
use crate::scripting::cluster_info::ClusterInfo;
use crate::scripting::row_distribution::RowDistributionPreset;
use crate::stats::session::SessionStats;
use aws_sdk_dynamodb::Client;
use rune::runtime::Object;
use rune::{Any, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use try_lock::TryLock;

#[derive(Any)]
pub struct Context {
    client: Option<Client>,
    page_size: u64,
    pub stats: Arc<TryLock<SessionStats>>,
    pub report_metadata: Arc<TryLock<HashMap<String, String>>>,
    pub start_time: TryLock<Instant>,
    pub retry_number: u64,
    pub retry_interval: RetryInterval,
    pub validation_strategy: ValidationStrategy,
    pub partition_row_presets: Arc<TryLock<HashMap<String, RowDistributionPreset>>>,
    #[rune(get, set, add_assign, copy)]
    pub load_cycle_count: u64,
    #[rune(get)]
    pub data: Value,
}

unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    pub fn new(
        client: Option<Client>,
        retry_number: u64,
        retry_interval: RetryInterval,
        validation_strategy: ValidationStrategy,
        page_size: u64,
    ) -> Context {
        Context {
            client,
            page_size,
            stats: Arc::new(TryLock::new(SessionStats::new())),
            report_metadata: Arc::new(TryLock::new(HashMap::new())),
            start_time: TryLock::new(Instant::now()),
            retry_number,
            retry_interval,
            validation_strategy,
            partition_row_presets: Arc::new(TryLock::new(HashMap::new())),
            load_cycle_count: 0,
            data: Value::new(Object::new()).unwrap(),
        }
    }

    pub fn clone(&self) -> Result<Self, LatteError> {
        let serialized = rmp_serde::to_vec(&self.data)?;
        let deserialized: Value = rmp_serde::from_slice(&serialized)?;
        Ok(Context {
            client: self.client.clone(),
            page_size: self.page_size,
            stats: Arc::new(TryLock::new(SessionStats::default())),
            report_metadata: Arc::new(TryLock::new(
                self.report_metadata.try_lock().unwrap().clone(),
            )),
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: Arc::new(TryLock::new(
                self.partition_row_presets.try_lock().unwrap().clone(),
            )),
            load_cycle_count: self.load_cycle_count,
            data: deserialized,
        })
    }

    /// Creates a shallow clone that shares the Arc-backed fields (stats, presets)
    /// with the original. Used to create a rune-owned `Value` for function call arguments.
    pub fn shallow_clone(&self) -> Self {
        Context {
            client: self.client.clone(),
            page_size: self.page_size,
            stats: Arc::clone(&self.stats),
            report_metadata: Arc::clone(&self.report_metadata),
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: Arc::clone(&self.partition_row_presets),
            load_cycle_count: self.load_cycle_count,
            data: self.data.clone(),
        }
    }

    /// Returns cluster metadata.
    pub async fn cluster_info(&self) -> Result<Option<ClusterInfo>, AlternatorError> {
        let client = self.get_client()?;

        // Try ScyllaDB-specific system table via the alternator virtual interface
        let scylla_result = client
            .scan()
            .table_name(".scylla.alternator.system.versions")
            .projection_expression("version, build_id")
            .send()
            .await;

        if let Ok(output) = scylla_result {
            if let Some(items) = output.items {
                if let Some(item) = items.first() {
                    let version = item
                        .get("version")
                        .and_then(|v| v.as_s().ok().map(|s| s.as_ref()))
                        .unwrap_or("unknown");

                    let build_id = item
                        .get("build_id")
                        .and_then(|v| v.as_s().ok().map(|s| s.as_ref()))
                        .unwrap_or("unknown");

                    return Ok(Some(ClusterInfo {
                        name: "".to_string(),
                        db_version: format!("ScyllaDB {version} with build-id {build_id}"),
                    }));
                }
            }
        }

        // If the ScyllaDB-specific table is not available, try to determine if it's AWS.
        let describe_endpoints = client.describe_endpoints().send().await;

        if let Ok(output) = describe_endpoints {
            let aws_endpoint = output
                .endpoints()
                .iter()
                .find(|endpoint| endpoint.address().contains("amazonaws.com"));

            if let Some(endpoint) = aws_endpoint {
                return Ok(Some(ClusterInfo {
                    name: endpoint.address().to_string(),
                    db_version: "AWS DynamoDB".to_string(),
                }));
            }
        }

        // We couldn't determine the cluster info.
        Ok(None)
    }

    pub fn set_report_field(&self, key: &str, value: &str) {
        self.report_metadata
            .try_lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
    }

    pub fn report_metadata_snapshot(&self) -> HashMap<String, String> {
        self.report_metadata.try_lock().unwrap().clone()
    }

    pub fn take_session_stats(&self) -> SessionStats {
        let mut stats = self.stats.try_lock().unwrap();
        let result = stats.clone();
        stats.reset();
        result
    }

    pub fn reset(&self) {
        self.stats.try_lock().unwrap().reset();
        *self.start_time.try_lock().unwrap() = Instant::now();
    }

    pub fn get_client(&self) -> Result<&Client, AlternatorError> {
        self.client
            .as_ref()
            .ok_or(AlternatorError::new(AlternatorErrorKind::Error(
                "DynamoDB client is not initialized".to_string(),
            )))
    }

    pub fn get_page_size(&self) -> u64 {
        self.page_size
    }
}
