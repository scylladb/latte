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

    pub async fn cluster_info(&self) -> Result<Option<ClusterInfo>, AlternatorError> {
        Ok(Some(ClusterInfo {
            name: "Alternator".to_string(),
            db_version: "Alternator".to_string(),
        }))
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
