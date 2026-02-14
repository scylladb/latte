use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use crate::config::{RetryInterval, ValidationStrategy};
use crate::error::LatteError;
use crate::scripting::cluster_info::ClusterInfo;
use crate::scripting::row_distribution::RowDistributionPreset;
use crate::stats::session::SessionStats;
use aws_sdk_dynamodb::Client;
use rune::runtime::{Object, Shared};
use rune::{Any, Value};
use std::collections::HashMap;
use std::time::Instant;
use try_lock::TryLock;

#[derive(Any)]
pub struct Context {
    client: Option<Client>,
    page_size: u64,
    pub stats: TryLock<SessionStats>,
    pub start_time: TryLock<Instant>,
    pub retry_number: u64,
    pub retry_interval: RetryInterval,
    pub validation_strategy: ValidationStrategy,
    pub partition_row_presets: HashMap<String, RowDistributionPreset>,
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
            stats: TryLock::new(SessionStats::new()),
            start_time: TryLock::new(Instant::now()),
            retry_number,
            retry_interval,
            validation_strategy,
            partition_row_presets: HashMap::new(),
            load_cycle_count: 0,
            data: Value::Object(Shared::new(Object::new()).unwrap()),
        }
    }

    pub fn clone(&self) -> Result<Self, LatteError> {
        let serialized = rmp_serde::to_vec(&self.data)?;
        let deserialized: Value = rmp_serde::from_slice(&serialized)?;
        Ok(Context {
            client: self.client.clone(),
            page_size: self.page_size,
            stats: TryLock::new(SessionStats::default()),
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: self.partition_row_presets.clone(),
            load_cycle_count: self.load_cycle_count,
            data: deserialized,
        })
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
