use super::cass_error::{CassError, CassErrorKind};
use super::deserialize::RuneRow;
use super::serialize::RuneQueryParams;
use crate::config::{RetryInterval, ValidationStrategy};
use crate::error::LatteError;
use crate::scripting::cluster_info::ClusterInfo;
use crate::scripting::retry_error::handle_retry_error;
use crate::scripting::row_distribution::RowDistributionPreset;
use crate::stats::session::SessionStats;

use once_cell::sync::Lazy;
use regex::Regex;
use rune::runtime::{Object, Vec as RuneVec};
use rune::{Any, Value};
use scylla::client::session::Session;
use scylla::response::PagingState;
use scylla::statement::batch::{Batch, BatchType};
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::unprepared::Statement;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use try_lock::TryLock;

static IS_SELECT_QUERY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*select\b").unwrap());
static IS_SELECT_COUNT_QUERY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*select\s+count\s*\(\s*[^)]*\s*\)").unwrap());

/// This is the main object that a workload script uses to interface with the outside world.
/// It also tracks query execution metrics such as number of requests, rows, response times etc.
#[derive(Any)]
pub struct Context {
    pub start_time: TryLock<Instant>,
    // NOTE: 'session' is defined as optional for being able to test methods
    // which don't 'depend on'/'use' the 'session' object.
    session: Option<Arc<Session>>,
    page_size: u64,
    statements: Arc<TryLock<HashMap<String, Arc<PreparedStatement>>>>,
    pub stats: Arc<TryLock<SessionStats>>,
    pub report_metadata: Arc<TryLock<HashMap<String, String>>>,
    pub metric_orientations: Arc<TryLock<HashMap<String, i8>>>,
    pub retry_number: u64,
    pub retry_interval: RetryInterval,
    pub validation_strategy: ValidationStrategy,
    pub partition_row_presets: Arc<TryLock<HashMap<String, RowDistributionPreset>>>,
    #[rune(get, set, add_assign, copy)]
    pub load_cycle_count: u64,
    #[rune(get)]
    pub preferred_datacenter: String,
    #[rune(get)]
    pub preferred_rack: String,
    #[rune(get)]
    pub data: Value,
}

// Needed, because Rune `Value` is !Send, as it may contain some internal pointers.
// Therefore, it is not safe to pass a `Value` to another thread by cloning it, because
// both objects could accidentally share some unprotected, `!Sync` data.
// To make it safe, the same `Context` is never used by more than one thread at once, and
// we make sure in `clone` to make a deep copy of the `data` field by serializing
// and deserializing it, so no pointers could get through.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    pub fn new(
        session: Option<Session>,
        page_size: u64,
        preferred_datacenter: String,
        preferred_rack: String,
        retry_number: u64,
        retry_interval: RetryInterval,
        validation_strategy: ValidationStrategy,
    ) -> Context {
        let data = Value::new(Object::new()).unwrap();
        Context {
            start_time: TryLock::new(Instant::now()),
            session: session.map(Arc::new),
            page_size,
            statements: Arc::new(TryLock::new(HashMap::new())),
            stats: Arc::new(TryLock::new(SessionStats::new())),
            report_metadata: Arc::new(TryLock::new(HashMap::new())),
            metric_orientations: Arc::new(TryLock::new(HashMap::new())),
            retry_number,
            retry_interval,
            validation_strategy,
            partition_row_presets: Arc::new(TryLock::new(HashMap::new())),
            load_cycle_count: 0,
            preferred_datacenter,
            preferred_rack,
            data,
        }
    }

    /// Clones the context for use by another thread.
    /// The new clone gets fresh statistics.
    /// The user data gets passed through serialization and deserialization to avoid
    /// accidental data sharing.
    pub fn clone(&self) -> Result<Self, LatteError> {
        let serialized = rmp_serde::to_vec(&self.data)?;
        let deserialized: Value = rmp_serde::from_slice(&serialized)?;
        Ok(Context {
            session: self.session.clone(),
            page_size: self.page_size,
            statements: Arc::new(TryLock::new(self.statements.try_lock().unwrap().clone())),
            stats: Arc::new(TryLock::new(SessionStats::default())),
            report_metadata: Arc::new(TryLock::new(
                self.report_metadata.try_lock().unwrap().clone(),
            )),
            metric_orientations: Arc::new(TryLock::new(
                self.metric_orientations.try_lock().unwrap().clone(),
            )),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: Arc::new(TryLock::new(
                self.partition_row_presets.try_lock().unwrap().clone(),
            )),
            load_cycle_count: self.load_cycle_count,
            preferred_datacenter: self.preferred_datacenter.clone(),
            preferred_rack: self.preferred_rack.clone(),
            data: deserialized,
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
        })
    }

    /// Creates a shallow clone that shares the Arc-backed fields (stats, statements, presets)
    /// with the original. Used to create a rune-owned `Value` for function call arguments
    /// without losing stats tracking.
    pub fn shallow_clone(&self) -> Self {
        Context {
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
            session: self.session.clone(),
            page_size: self.page_size,
            statements: Arc::clone(&self.statements),
            stats: Arc::clone(&self.stats),
            report_metadata: Arc::clone(&self.report_metadata),
            metric_orientations: Arc::clone(&self.metric_orientations),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: Arc::clone(&self.partition_row_presets),
            load_cycle_count: self.load_cycle_count,
            preferred_datacenter: self.preferred_datacenter.clone(),
            preferred_rack: self.preferred_rack.clone(),
            data: self.data.clone(),
        }
    }

    /// Returns cluster metadata such as cluster name and DB version.
    pub async fn cluster_info(&self) -> Result<Option<ClusterInfo>, CassError> {
        let session = match &self.session {
            Some(session) => session,
            None => {
                return Err(CassError(CassErrorKind::Error(
                    "'session' is not defined".to_string(),
                )))
            }
        };
        let scylla_cql = "SELECT version, build_id FROM system.versions";
        let rs = session
            .query_unpaged(scylla_cql, ())
            .await
            .map_err(|e| CassError::query_execution_error(scylla_cql, None, e));
        match rs {
            Ok(rs) => {
                let rows_result = rs.into_rows_result()?;
                while let Ok(mut row) = rows_result.rows::<(&str, &str)>() {
                    if let Some(Ok((scylla_version, build_id))) = row.next() {
                        return Ok(Some(ClusterInfo {
                            name: "".to_string(),
                            db_version: format!(
                                "ScyllaDB {scylla_version} with build-id {build_id}",
                            ),
                        }));
                    }
                }
                Ok(None)
            }
            Err(_e) => {
                // NOTE: following exists in both cases
                // and if we run against ScyllaDB then it has static '3.0.8' version.
                let cass_cql = "SELECT cluster_name, release_version FROM system.local";
                let rs = session
                    .query_unpaged(cass_cql, ())
                    .await
                    .map_err(|e| CassError::query_execution_error(cass_cql, None, e));
                match rs {
                    Ok(rs) => {
                        let rows_result = rs.into_rows_result()?;
                        while let Ok(mut row) = rows_result.rows::<(&str, &str)>() {
                            if let Some(Ok((name, cass_version))) = row.next() {
                                return Ok(Some(ClusterInfo {
                                    name: name.to_string(),
                                    db_version: format!("Cassandra {cass_version}"),
                                }));
                            }
                        }
                        Ok(None)
                    }
                    Err(e) => {
                        eprintln!("WARNING: {e}");
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Returns list of datacenters used by nodes
    pub async fn get_datacenters(&self) -> Result<Vec<String>, CassError> {
        match &self.session {
            Some(session) => {
                let cluster_data = session.get_cluster_state();
                let mut datacenters_hashset = HashSet::new();
                for node in cluster_data.get_nodes_info() {
                    if let Some(dc) = &node.datacenter {
                        datacenters_hashset.insert(dc.clone());
                    }
                }
                let mut datacenters: Vec<String> = datacenters_hashset.into_iter().collect();
                datacenters.sort();
                Ok(datacenters)
            }
            None => Err(CassError(CassErrorKind::Error(
                "'session' is not defined".to_string(),
            ))),
        }
    }

    /// Prepares a statement and stores it in an internal statement map for future use.
    pub async fn prepare(&self, key: &str, cql: &str) -> Result<(), CassError> {
        match &self.session {
            Some(session) => {
                let statement = session
                    .prepare(Statement::new(cql).with_page_size(self.page_size as i32))
                    .await
                    .map_err(|e| CassError::prepare_error(cql, e))?;
                self.statements
                    .try_lock()
                    .unwrap()
                    .insert(key.to_string(), Arc::new(statement));
                Ok(())
            }
            None => Err(CassError(CassErrorKind::Error(
                "'session' is not defined".to_string(),
            ))),
        }
    }

    /// Executes an ad-hoc CQL statement with no parameters. Does not prepare.
    pub async fn execute(&self, cql: &str) -> Result<Value, CassError> {
        self._execute(Some(cql), None, None, None, None, None, false)
            .await
    }

    /// Executes an ad-hoc CQL statement with no parameters. Does not prepare.
    /// Validates returning rows for `select` queries.
    pub async fn execute_with_validation(
        &self,
        cql: &str,
        expected_rows_num_min: u64,
        expected_rows_num_max: u64,
        custom_err_msg: &str,
    ) -> Result<Value, CassError> {
        if expected_rows_num_min > expected_rows_num_max {
            return Err(CassError(CassErrorKind::Error(format!(
                "Expected 'minimum' ({expected_rows_num_min}) of rows number \
                     cannot be less than 'maximum' ({expected_rows_num_max})"
            ))));
        }
        self._execute(
            Some(cql),
            None,
            None,
            Some(expected_rows_num_min),
            Some(expected_rows_num_max),
            Some(custom_err_msg),
            false,
        )
        .await
    }

    /// Executes a statement prepared and registered earlier by a call to `prepare`.
    pub async fn execute_prepared(&self, key: &str, params: Value) -> Result<Value, CassError> {
        self._execute(None, Some(key), Some(params), None, None, None, false)
            .await
    }

    /// Executes a statement prepared and registered earlier by a call to `prepare` validating
    /// returning rows for `select` queries.
    pub async fn execute_prepared_with_validation(
        &self,
        key: &str,
        params: Value,
        expected_rows_num_min: u64,
        expected_rows_num_max: u64,
        custom_err_msg: &str,
    ) -> Result<Value, CassError> {
        if expected_rows_num_min > expected_rows_num_max {
            return Err(CassError(CassErrorKind::Error(format!(
                "Expected 'minimum' ({expected_rows_num_min}) of rows number \
                     cannot be less than 'maximum' ({expected_rows_num_max})"
            ))));
        }
        self._execute(
            None,
            Some(key),
            Some(params),
            Some(expected_rows_num_min),
            Some(expected_rows_num_max),
            Some(custom_err_msg),
            false,
        )
        .await
    }

    /// Executes an ad-hoc CQL statement and returns the result data.
    pub async fn execute_with_result(&self, cql: &str) -> Result<Value, CassError> {
        self._execute(Some(cql), None, None, None, None, None, true)
            .await
    }

    /// Executes a statement prepared and registered earlier by a call to `prepare` and returns the result data.
    pub async fn execute_prepared_with_result(
        &self,
        key: &str,
        params: Value,
    ) -> Result<Value, CassError> {
        self._execute(None, Some(key), Some(params), None, None, None, true)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn _execute(
        &self,
        cql: Option<&str>,
        key: Option<&str>,
        params: Option<Value>,
        expected_rows_num_min: Option<u64>,
        expected_rows_num_max: Option<u64>,
        custom_err_msg: Option<&str>,
        process_and_return_data: bool,
    ) -> Result<Value, CassError> {
        let session = match &self.session {
            Some(session) => session,
            None => {
                return Err(CassError(CassErrorKind::Error(
                    "'session' is not defined".to_string(),
                )))
            }
        };
        if (cql.is_some() && key.is_some()) || (cql.is_none() && key.is_none()) {
            return Err(CassError(CassErrorKind::Error(
                "Either 'cql' or 'key' is allowed, not both".to_string(),
            )));
        }
        let stmt = if let Some(key) = key {
            self.statements
                .try_lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| {
                    CassError(CassErrorKind::PreparedStatementNotFound(key.to_string()))
                })?
        } else {
            let cql = cql.expect("failed to unwrap the 'cql' parameter");
            Arc::new(
                session
                    .prepare(Statement::new(cql).with_page_size(self.page_size as i32))
                    .await
                    .map_err(|e| CassError::prepare_error(cql, e))?,
            )
        };
        let cql = stmt.get_statement();
        let query_params = RuneQueryParams::new(params.as_ref());
        if (expected_rows_num_min.is_some() || expected_rows_num_max.is_some())
            && !IS_SELECT_QUERY.is_match(cql)
        {
            return Err(CassError::query_response_validation_not_applicable_error(
                cql,
                params.as_ref(),
            ));
        }
        if (expected_rows_num_min.is_some() || expected_rows_num_max.is_some())
            && process_and_return_data
        {
            return Err(CassError(CassErrorKind::Error(
                "Row count validation and rows data processing are not supported together"
                    .to_string(),
            )));
        }
        let is_select_count = IS_SELECT_COUNT_QUERY.is_match(cql);
        let mut all_pages_duration = Duration::ZERO;
        let mut paging_state = PagingState::start();
        let mut rune_rows = RuneVec::new();
        let mut rows_num: u64 = 0;
        let mut last_rows_result = None;
        let mut current_attempt_num = 0;
        while current_attempt_num <= self.retry_number {
            let start_time = self.stats.try_lock().unwrap().start_request();
            let rs = session
                .execute_single_page(&stmt, &query_params, paging_state.clone())
                .await;
            let current_duration = Instant::now() - start_time;
            let (page, paging_state_response) = match rs {
                Ok(result) => result,
                Err(e) => {
                    let current_error =
                        CassError::query_execution_error(cql, params.as_ref(), e.clone());
                    handle_retry_error(self, current_attempt_num, current_error).await;
                    current_attempt_num += 1;
                    continue; // try again the same query
                }
            };
            let rows_result = page.into_rows_result();
            if process_and_return_data {
                let rows_result = rows_result?;
                let row_iterator = rows_result.rows::<RuneRow>()?;
                for row_result in row_iterator {
                    match row_result {
                        Ok(RuneRow(row_obj)) => {
                            rune_rows
                                .push(Value::new(row_obj).map_err(|_| {
                                    CassError(CassErrorKind::Error(
                                        "Failed to create shared row object".to_string(),
                                    ))
                                })?)
                                .map_err(|_| {
                                    CassError(CassErrorKind::Error(
                                        "Failed to push row to result vector".to_string(),
                                    ))
                                })?;
                        }
                        Err(_) => {
                            break; // Exit the loop if row_result is invalid
                        }
                    }
                }
                rows_num = rune_rows.len() as u64;
            } else {
                if let Ok(ref rr) = rows_result {
                    rows_num += rr.rows_num() as u64;
                }
                if is_select_count {
                    last_rows_result = Some(rows_result);
                }
            }
            all_pages_duration += current_duration;
            match paging_state_response.into_paging_control_flow() {
                ControlFlow::Break(()) => {
                    self.stats
                        .try_lock()
                        .unwrap()
                        .complete_request(all_pages_duration, rows_num);
                    if process_and_return_data {
                        return Value::vec(rune_rows.into_inner()).map_err(|_| {
                            CassError(CassErrorKind::Error(
                                "Failed to create shared result vector".to_string(),
                            ))
                        });
                    } else {
                        let empty_rune_vec = Value::vec(Default::default())?;
                        let rows_min = match expected_rows_num_min {
                            None => return Ok(empty_rune_vec),
                            Some(rows_min) => rows_min,
                        };
                        let (rows_max, mut rows_cnt) = (expected_rows_num_max.unwrap(), rows_num);
                        if is_select_count {
                            rows_cnt = last_rows_result
                                .take()
                                .expect("SELECT COUNT should have rows_result")?
                                .first_row::<(i64,)>()?
                                .0 as u64;
                            if rows_num == 1 && rows_min <= rows_cnt && rows_cnt <= rows_max {
                                return Ok(empty_rune_vec); // SELECT COUNT(...) returned expected rows number
                            }
                        } else if rows_min <= rows_num && rows_num <= rows_max {
                            return Ok(empty_rune_vec); // Common 'SELECT' returned expected number of rows in total
                        }
                        let current_error = CassError::query_validation_error(
                            cql,
                            params.as_ref(),
                            rows_min,
                            rows_max,
                            rows_cnt,
                            custom_err_msg.unwrap_or("").to_string(),
                        );
                        if self.validation_strategy == ValidationStrategy::Retry {
                            handle_retry_error(self, current_attempt_num, current_error).await;
                            current_attempt_num += 1;
                            rows_num = 0; // we retry all pages, so reset cnt
                            last_rows_result = None;
                            continue; // try again the same query
                        } else if self.validation_strategy == ValidationStrategy::FailFast {
                            return Err(current_error); // stop stress execution
                        } else if self.validation_strategy == ValidationStrategy::Ignore {
                            handle_retry_error(self, current_attempt_num, current_error).await;
                            return Ok(empty_rune_vec); // handle/print error and go on.
                        } else {
                            // should never reach this code branch
                            return Err(CassError(CassErrorKind::Error(format!(
                                "Unexpected value for the validation strategy param: {:?}",
                                self.validation_strategy,
                            ))));
                        }
                    }
                }
                ControlFlow::Continue(new_paging_state) => {
                    paging_state = new_paging_state;
                    current_attempt_num = 0;
                    continue; // get next page
                }
            }
        }
        Err(CassError::query_retries_exceeded(self.retry_number))
    }

    pub async fn batch_prepared(
        &self,
        keys: Vec<&str>,
        params: Vec<Value>,
    ) -> Result<(), CassError> {
        let keys_len = keys.len();
        let params_len = params.len();
        if keys_len != params_len {
            return Err(CassError(CassErrorKind::Error(format!(
                "Number of prepared statements ({keys_len}) and values ({params_len}) must be equal"
            ))));
        } else if keys_len == 0 {
            return Err(CassError(CassErrorKind::Error("Empty batch".to_string())));
        }
        let mut batch: Batch = Batch::new(BatchType::Logged);
        let mut batch_values: Vec<RuneQueryParams<'_>> = Vec::with_capacity(keys_len);
        for (i, key) in keys.into_iter().enumerate() {
            let statement = self
                .statements
                .try_lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| {
                    CassError(CassErrorKind::PreparedStatementNotFound(key.to_string()))
                })?;
            batch.append_statement((*statement).clone());
            batch_values.push(RuneQueryParams::new(params.get(i)));
        }
        match &self.session {
            Some(session) => {
                let mut current_attempt_num = 0;
                while current_attempt_num <= self.retry_number {
                    let start_time = self.stats.try_lock().unwrap().start_request();
                    let rs = session.batch(&batch, &batch_values).await;
                    let duration = Instant::now() - start_time;
                    match rs {
                        Ok(_) => {
                            self.stats
                                .try_lock()
                                .unwrap()
                                .complete_request(duration, batch_values.len() as u64);
                            return Ok(());
                        }
                        Err(e) => {
                            let current_error = CassError(CassErrorKind::Error(format!(
                                "batch execution failed: {e}"
                            )));
                            handle_retry_error(self, current_attempt_num, current_error).await;
                            current_attempt_num += 1;
                            continue;
                        }
                    }
                }
                Err(CassError::query_retries_exceeded(self.retry_number))
            }
            None => Err(CassError(CassErrorKind::Error(
                "'session' is not defined".to_string(),
            ))),
        }
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

    pub fn record_metric(&self, name: &str, value: f64) {
        self.stats.try_lock().unwrap().record_metric(name, value);
    }

    pub fn declare_metric(&self, name: &str, orientation: i8) {
        self.metric_orientations
            .try_lock()
            .unwrap()
            .insert(name.to_string(), orientation);
    }

    pub fn metric_orientations_snapshot(&self) -> HashMap<String, i8> {
        self.metric_orientations.try_lock().unwrap().clone()
    }

    /// Returns the current accumulated request stats snapshot and resets the stats.
    pub fn take_session_stats(&self) -> SessionStats {
        let mut stats = self.stats.try_lock().unwrap();
        let result = stats.clone();
        stats.reset();
        result
    }

    /// Resets query and request counters
    pub fn reset(&self) {
        self.stats.try_lock().unwrap().reset();
        *self.start_time.try_lock().unwrap() = Instant::now();
    }
}
