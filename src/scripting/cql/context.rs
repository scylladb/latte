use super::bind::to_scylla_query_params;
use super::cass_error::{CassError, CassErrorKind};
use super::connect::ClusterInfo;
use crate::config::{RetryInterval, ValidationStrategy};
use crate::error::LatteError;
use crate::scripting::row_distribution::RowDistributionPreset;
use crate::stats::session::SessionStats;
use chrono::Utc;
use itertools::enumerate;
use once_cell::sync::Lazy;
use rand::prelude::ThreadRng;
use rand::random;
use regex::Regex;
use rune::alloc::vec::Vec as RuneAllocVec;
use rune::alloc::String as RuneString;
use rune::runtime::{Object, OwnedTuple, Shared, Vec as RuneVec};
use rune::{Any, Value};
use scylla::client::session::Session;
use scylla::response::PagingState;
use scylla::statement::batch::{Batch, BatchType};
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::unprepared::Statement;
use scylla::value::{CqlValue, Row};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::error;
use try_lock::TryLock;

static IS_SELECT_QUERY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*select\b").unwrap());
static IS_SELECT_COUNT_QUERY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*select\s+count\s*\(\s*[^)]*\s*\)").unwrap());

/// Converts a Scylla CqlValue to a Rune Value
fn cql_value_to_rune_value(value: Option<&CqlValue>) -> Result<Value, Box<CassError>> {
    match value {
        Some(CqlValue::Ascii(s)) | Some(CqlValue::Text(s)) => Ok(Value::String(Shared::new(
            RuneString::try_from(s.clone()).expect("Failed to create RuneString"),
        )?)),
        Some(CqlValue::Boolean(b)) => Ok(Value::Bool(*b)),
        Some(CqlValue::TinyInt(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::SmallInt(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::Int(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::BigInt(i)) => Ok(Value::Integer(*i)),
        Some(CqlValue::Float(f)) => Ok(Value::Float(*f as f64)),
        Some(CqlValue::Double(f)) => Ok(Value::Float(*f)),
        Some(CqlValue::Counter(c)) => Ok(Value::Integer(c.0)),
        Some(CqlValue::Timestamp(ts)) => Ok(Value::Integer(ts.0)),
        Some(CqlValue::Date(date)) => Ok(Value::Integer(date.0 as i64)),
        Some(CqlValue::Time(time)) => Ok(Value::Integer(time.0)),
        Some(CqlValue::Blob(blob)) => {
            let mut rune_vec = RuneVec::new();
            for byte in blob {
                rune_vec.push(Value::Byte(*byte)).map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to push byte to Rune vector".to_string(),
                    )))
                })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector for blob".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Uuid(uuid)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(uuid.to_string())
                    .expect("Failed to create RuneString for UUID"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for UUID".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Timeuuid(timeuuid)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(timeuuid.to_string())
                    .expect("Failed to create RuneString for TimeUuid"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for TimeUuid".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Inet(addr)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(addr.to_string())
                    .expect("Failed to create RuneString for IpAddr"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for IpAddr".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Vector(vector)) => {
            let mut rune_vec = RuneVec::new();
            for item in vector {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::List(list)) => {
            let mut rune_vec = RuneVec::new();
            for item in list {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Set(set)) => {
            let mut rune_vec = RuneVec::new();
            for item in set {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Map(map)) => {
            let mut rune_vec = RuneVec::new();
            for (key, value) in map {
                let mut pair = RuneAllocVec::new();
                pair.try_push(cql_value_to_rune_value(Some(key))?)?;
                pair.try_push(cql_value_to_rune_value(Some(value))?)?;
                rune_vec
                    .push(Value::Tuple(Shared::new(OwnedTuple::try_from(pair)?)?))
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push map key-value pair to the Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared Rune vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::UserDefinedType { fields, .. }) => {
            let mut rune_obj = Object::new();
            for (field_name, field_value) in fields {
                rune_obj
                    .insert(
                        RuneString::try_from(field_name.clone())
                            .expect("Failed to create RuneString"),
                        cql_value_to_rune_value(field_value.as_ref())?,
                    )
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to insert UDT field into Rune object".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Object(Shared::new(rune_obj).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared object for UDT".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Tuple(tuple)) => {
            let mut rune_vec = RuneVec::new();
            for item in tuple {
                rune_vec
                    .push(cql_value_to_rune_value(item.as_ref())?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push tuple item to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector for tuple".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Varint(varint)) => Ok(Value::Integer({
            let varint_bytes = varint.as_signed_bytes_be_slice();
            if varint_bytes.len() > 8 {
                return Err(Box::new(CassError(CassErrorKind::Error(
                    "Varint is too large to fit into an i64".to_string(),
                ))));
            };
            let mut padded = [0u8; 8];
            if varint_bytes[0] & 0x80 != 0 {
                padded[..8 - varint_bytes.len()].fill(0xFF);
            }
            padded[8 - varint_bytes.len()..].copy_from_slice(varint_bytes);
            i64::from_be_bytes(padded)
        })),
        Some(CqlValue::Decimal(decimal)) => {
            let (mantissa_be, scale) = &decimal.clone().into_signed_be_bytes_and_exponent();
            let mantissa = if mantissa_be.len() == 8 {
                i64::from_be_bytes(mantissa_be.as_slice().try_into().unwrap())
            } else if mantissa_be.len() < 8 {
                let mut mantissa_array = [0u8; 8];
                mantissa_array[8 - mantissa_be.len()..].copy_from_slice(mantissa_be);
                i64::from_be_bytes(mantissa_array)
            } else {
                let truncated = &mantissa_be[mantissa_be.len() - 8..];
                i64::from_be_bytes(truncated.try_into().unwrap())
            };
            let dec = rust_decimal::Decimal::try_new(mantissa, u32::try_from(*scale)?).unwrap();
            Ok(Value::String(
                Shared::new(
                    RuneString::try_from(dec.to_string()).expect("Failed to create RuneString"),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to create shared string for Decimal".to_string(),
                    )))
                })?,
            ))
        }
        Some(CqlValue::Duration(duration)) => {
            // TODO: update the logic for duration to provide also a duration-like string such as "1h2m3s"
            let mut rune_obj = Object::new();
            rune_obj
                .insert(
                    RuneString::try_from("months").expect("Failed to create RuneString"),
                    Value::Integer(duration.months as i64),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert months into duration object".to_string(),
                    )))
                })?;
            rune_obj
                .insert(
                    RuneString::try_from("days").expect("Failed to create RuneString"),
                    Value::Integer(duration.days as i64),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert days into duration object".to_string(),
                    )))
                })?;
            rune_obj
                .insert(
                    RuneString::try_from("nanoseconds").expect("Failed to create RuneString"),
                    Value::Integer(duration.nanoseconds),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert nanoseconds into duration object".to_string(),
                    )))
                })?;
            Ok(Value::Object(Shared::new(rune_obj).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared object for Duration".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Empty) => Ok(Value::Option(Shared::new(None)?)),
        None => Ok(Value::Option(Shared::new(None)?)),
        Some(&_) => todo!(), // unexpected, should never be reached
    }
}

/// This is the main object that a workload script uses to interface with the outside world.
/// It also tracks query execution metrics such as number of requests, rows, response times etc.
#[derive(Any)]
pub struct Context {
    start_time: TryLock<Instant>,
    // NOTE: 'session' is defined as optional for being able to test methods
    // which don't 'depend on'/'use' the 'session' object.
    session: Option<Arc<Session>>,
    page_size: u64,
    statements: HashMap<String, Arc<PreparedStatement>>,
    stats: TryLock<SessionStats>,
    pub retry_number: u64,
    retry_interval: RetryInterval,
    pub validation_strategy: ValidationStrategy,
    pub partition_row_presets: HashMap<String, RowDistributionPreset>,
    #[rune(get, set, add_assign, copy)]
    pub load_cycle_count: u64,
    #[rune(get)]
    pub preferred_datacenter: String,
    #[rune(get)]
    pub preferred_rack: String,
    #[rune(get)]
    pub data: Value,
    pub rng: ThreadRng,
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
        Context {
            start_time: TryLock::new(Instant::now()),
            session: session.map(Arc::new),
            page_size,
            statements: HashMap::new(),
            stats: TryLock::new(SessionStats::new()),
            retry_number,
            retry_interval,
            validation_strategy,
            partition_row_presets: HashMap::new(),
            load_cycle_count: 0,
            preferred_datacenter,
            preferred_rack,
            data: Value::Object(Shared::new(Object::new()).unwrap()),
            rng: rand::thread_rng(),
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
            statements: self.statements.clone(),
            stats: TryLock::new(SessionStats::default()),
            retry_number: self.retry_number,
            retry_interval: self.retry_interval,
            validation_strategy: self.validation_strategy,
            partition_row_presets: self.partition_row_presets.clone(),
            load_cycle_count: self.load_cycle_count,
            preferred_datacenter: self.preferred_datacenter.clone(),
            preferred_rack: self.preferred_rack.clone(),
            data: deserialized,
            start_time: TryLock::new(*self.start_time.try_lock().unwrap()),
            rng: rand::thread_rng(),
        })
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
            .map_err(|e| CassError::query_execution_error(scylla_cql, &[], e));
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
                    .map_err(|e| CassError::query_execution_error(cass_cql, &[], e));
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
    pub async fn prepare(&mut self, key: &str, cql: &str) -> Result<(), CassError> {
        match &self.session {
            Some(session) => {
                let statement = session
                    .prepare(Statement::new(cql).with_page_size(self.page_size as i32))
                    .await
                    .map_err(|e| CassError::prepare_error(cql, e))?;
                self.statements.insert(key.to_string(), Arc::new(statement));
                Ok(())
            }
            None => Err(CassError(CassErrorKind::Error(
                "'session' is not defined".to_string(),
            ))),
        }
    }

    pub async fn signal_failure(&self, message: &str) -> Result<(), CassError> {
        let err = CassError(CassErrorKind::CustomError(message.to_string()));
        Err(err)
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
            self.statements.get(key).ok_or_else(|| {
                CassError(CassErrorKind::PreparedStatementNotFound(key.to_string()))
            })?
        } else {
            let cql = cql.expect("failed to unwrap the 'cql' parameter");
            &Arc::new(
                session
                    .prepare(Statement::new(cql).with_page_size(self.page_size as i32))
                    .await
                    .map_err(|e| CassError::prepare_error(cql, e))?,
            )
        };
        let cql = stmt.get_statement();
        let params = match params {
            Some(params) => to_scylla_query_params(&params, stmt.get_variable_col_specs())?,
            None => vec![],
        };
        if (expected_rows_num_min.is_some() || expected_rows_num_max.is_some())
            && !IS_SELECT_QUERY.is_match(cql)
        {
            return Err(CassError::query_response_validation_not_applicable_error(
                cql, &params,
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
        let mut all_pages_duration = Duration::ZERO;
        let mut paging_state = PagingState::start();
        // NOTE: outer vector is container of rows, inner one is container of row column data
        let mut all_rows: Vec<Vec<(String, CqlValue)>> = Vec::new();
        let mut rows_num: u64 = 0;
        let mut current_attempt_num = 0;
        while current_attempt_num <= self.retry_number {
            let start_time = self.stats.try_lock().unwrap().start_request();
            let rs = session
                .execute_single_page(stmt, params.clone(), paging_state.clone())
                .await;
            let current_duration = Instant::now() - start_time;
            let (page, paging_state_response) = match rs {
                Ok((ref page, ref paging_state_response)) => (page, paging_state_response),
                Err(e) => {
                    let current_error = CassError::query_execution_error(cql, &params, e.clone());
                    handle_retry_error(self, current_attempt_num, current_error).await;
                    current_attempt_num += 1;
                    continue; // try again the same query
                }
            };
            let rows_result = page.clone().into_rows_result();
            if process_and_return_data {
                let rows_result_unwrapped = rows_result?;
                let column_specs = rows_result_unwrapped.column_specs();
                let row_iterator = rows_result_unwrapped.rows::<Row>()?;
                for row_result in row_iterator {
                    let mut current_row_data: Vec<(String, CqlValue)> = vec![];
                    match row_result {
                        Ok(row) => {
                            for (index, cql_value) in row.columns.iter().enumerate() {
                                let col_name = column_specs
                                    .iter()
                                    .nth(index)
                                    .map(|spec| spec.name())
                                    .unwrap_or_else(|| "unknown");
                                current_row_data.push((
                                    col_name.to_string(),
                                    cql_value.clone().unwrap_or(CqlValue::Empty),
                                ));
                            }
                        }
                        Err(_) => {
                            break; // Exit the loop if row_result is invalid
                        }
                    }
                    all_rows.push(current_row_data);
                }
                rows_num = all_rows.len() as u64;
            } else {
                rows_num += rows_result.map(|r| r.rows_num()).unwrap_or(0) as u64;
            }
            all_pages_duration += current_duration;
            match paging_state_response.clone().into_paging_control_flow() {
                ControlFlow::Break(()) => {
                    self.stats
                        .try_lock()
                        .unwrap()
                        .complete_request(all_pages_duration, rows_num);
                    if process_and_return_data {
                        // Convert the collected rows to Rune values
                        let mut rune_rows = RuneVec::new();
                        for current_row_vec in all_rows {
                            let mut row_obj = Object::new();
                            for (col_name, col_value) in current_row_vec {
                                row_obj
                                    .insert(
                                        RuneString::try_from(col_name)
                                            .expect("Failed to create RuneString for column name"),
                                        cql_value_to_rune_value(Some(&col_value))?,
                                    )
                                    .map_err(|_| {
                                        CassError(CassErrorKind::Error(
                                            "Failed to insert column into row object".to_string(),
                                        ))
                                    })?;
                            }
                            rune_rows
                                .push(Value::Object(Shared::new(row_obj).map_err(|_| {
                                    CassError(CassErrorKind::Error(
                                        "Failed to create shared row object".to_string(),
                                    ))
                                })?))
                                .map_err(|_| {
                                    CassError(CassErrorKind::Error(
                                        "Failed to push row to result vector".to_string(),
                                    ))
                                })?;
                        }

                        return Ok(Value::Vec(Shared::new(rune_rows).map_err(|_| {
                            CassError(CassErrorKind::Error(
                                "Failed to create shared result vector".to_string(),
                            ))
                        })?));
                    } else {
                        let empty_rune_vec = Value::Vec(Shared::new(RuneVec::new())?);
                        let rows_min = match expected_rows_num_min {
                            None => return Ok(empty_rune_vec),
                            Some(rows_min) => rows_min,
                        };
                        let (rows_max, mut rows_cnt) = (expected_rows_num_max.unwrap(), rows_num);
                        if IS_SELECT_COUNT_QUERY.is_match(cql) {
                            rows_cnt =
                                page.clone().into_rows_result()?.first_row::<(i64,)>()?.0 as u64;
                            if rows_num == 1 && rows_min <= rows_cnt && rows_cnt <= rows_max {
                                return Ok(empty_rune_vec); // SELECT COUNT(...) returned expected rows number
                            }
                        } else if rows_min <= rows_num && rows_num <= rows_max {
                            return Ok(empty_rune_vec); // Common 'SELECT' returned expected number of rows in total
                        }
                        let current_error = CassError::query_validation_error(
                            cql,
                            &params,
                            rows_min,
                            rows_max,
                            rows_cnt,
                            custom_err_msg.unwrap_or("").to_string(),
                        );
                        if self.validation_strategy == ValidationStrategy::Retry {
                            handle_retry_error(self, current_attempt_num, current_error).await;
                            current_attempt_num += 1;
                            rows_num = 0; // we retry all pages, so reset cnt
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
        let mut batch_values: Vec<Vec<Option<CqlValue>>> = vec![];
        for (i, key) in enumerate(keys) {
            let statement = self.statements.get(key).ok_or_else(|| {
                CassError(CassErrorKind::PreparedStatementNotFound(key.to_string()))
            })?;
            let statement_col_specs = statement.get_variable_col_specs();
            batch.append_statement((**statement).clone());
            batch_values.push(to_scylla_query_params(
                params
                    .get(i)
                    .expect("failed to bind rune values to the statement columns"),
                statement_col_specs,
            )?);
        }
        match &self.session {
            Some(session) => {
                let mut current_attempt_num = 0;
                while current_attempt_num <= self.retry_number {
                    let start_time = self.stats.try_lock().unwrap().start_request();
                    let rs = session.batch(&batch, batch_values.clone()).await;
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

    pub fn elapsed_secs(&self) -> f64 {
        self.start_time.try_lock().unwrap().elapsed().as_secs_f64()
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

pub fn get_exponential_retry_interval(
    min_interval: Duration,
    max_interval: Duration,
    current_attempt_num: u64,
) -> Duration {
    let min_interval_float: f64 = min_interval.as_secs_f64();
    let mut current_interval: f64 =
        min_interval_float * (2u64.pow(current_attempt_num.try_into().unwrap_or(0)) as f64);

    // Add jitter
    current_interval += random::<f64>() * min_interval_float;
    current_interval -= min_interval_float / 2.0;

    Duration::from_secs_f64(current_interval.min(max_interval.as_secs_f64()))
}

pub async fn handle_retry_error(
    ctxt: &Context,
    current_attempt_num: u64,
    current_error: CassError,
) {
    let current_retry_interval = get_exponential_retry_interval(
        ctxt.retry_interval.min,
        ctxt.retry_interval.max,
        current_attempt_num,
    );

    let mut next_attempt_str = String::new();
    let is_last_attempt = current_attempt_num == ctxt.retry_number;
    if !is_last_attempt {
        next_attempt_str += &format!("[Retry in {} ms]", current_retry_interval.as_millis());
    }
    let err_msg = format!(
        "{}: [ERROR][Attempt {}/{}]{} {}",
        Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        current_attempt_num,
        ctxt.retry_number,
        next_attempt_str,
        current_error,
    );
    error!("{}", err_msg);
    if !is_last_attempt {
        ctxt.stats.try_lock().unwrap().store_retry_error(err_msg);
        tokio::time::sleep(current_retry_interval).await;
    } else {
        eprintln!("{err_msg}");
    }
}
