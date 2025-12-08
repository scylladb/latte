use super::bind::to_scylla_query_params;
use super::cass_error::{CassError, CassErrorKind};
use super::connect::ClusterInfo;
use crate::config::{RetryInterval, ValidationStrategy};
use crate::error::LatteError;
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

#[derive(Clone, Debug, PartialEq)]
pub struct PartitionGroup {
    pub n_rows_per_group: u64,
    pub n_partitions: u64,
    pub n_rows_per_partition: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowDistribution {
    pub n_cycles: u64,
    pub n_rows_for_left: u64,
    pub n_rows_for_right: u64,
    pub n_rows_for_left_and_right: u64,
    pub n_rows_for_all_cycles: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowDistributionPreset {
    pub total_rows: u64,
    pub partition_groups: Vec<PartitionGroup>,
    pub row_distributions: Vec<(RowDistribution, RowDistribution)>,
}

impl RowDistributionPreset {
    pub fn new(partition_groups: Vec<PartitionGroup>) -> RowDistributionPreset {
        let total_rows: u64 = partition_groups.iter().map(|pg| pg.n_rows_per_group).sum();
        RowDistributionPreset {
            total_rows,
            partition_groups,
            row_distributions: vec![],
        }
    }

    pub fn generate_row_distributions(&mut self) {
        let mut other_rows: u64 = self.total_rows;
        for partition_group in &self.partition_groups {
            // NOTE: Calculate the greatest common divisor allowing it to split it to 2 groups
            //       for getting better distribution results.
            //       This "greatest common divisioner" will be used as a number of distribution cycles
            //       based on the partition group proportions.
            other_rows -= partition_group.n_rows_per_group;
            let (cycles_num, (mult_n1, tail_n1), (mult_n2, tail_n2)) =
                max_gcd_with_tail(partition_group.n_rows_per_group, other_rows);
            let cycle_type_1 = (
                tail_n1 + tail_n2,
                mult_n1 + (tail_n1 > 0) as u64,
                mult_n2 + (tail_n2 > 0) as u64,
            );
            let cycle_type_2 = ((cycles_num - tail_n1 - tail_n2), mult_n1, mult_n2);
            self.row_distributions.push((
                RowDistribution {
                    n_cycles: cycle_type_1.0,
                    n_rows_for_left: cycle_type_1.1,
                    n_rows_for_right: cycle_type_1.2,
                    n_rows_for_left_and_right: cycle_type_1.1 + cycle_type_1.2,
                    n_rows_for_all_cycles: cycle_type_1.0 * cycle_type_1.1
                        + cycle_type_1.0 * cycle_type_1.2,
                },
                RowDistribution {
                    n_cycles: cycle_type_2.0,
                    n_rows_for_left: cycle_type_2.1,
                    n_rows_for_right: cycle_type_2.2,
                    n_rows_for_left_and_right: cycle_type_2.1 + cycle_type_2.2,
                    n_rows_for_all_cycles: cycle_type_2.0 * cycle_type_2.1
                        + cycle_type_2.0 * cycle_type_2.2,
                },
            ));
        }
    }

    /// Returns partition index and number of expected rows in it
    /// based on the provided stress iteration index.
    pub async fn get_partition_info(&self, idx: u64) -> (u64, u64) {
        self._get_partition_info(
            idx % self.total_rows,
            0,
            self.partition_groups.clone(),
            self.row_distributions.clone(),
        )
        .await
    }

    async fn _get_partition_info(
        &self,
        mut idx: u64,
        mut partn_offset: u64,
        partition_groups: Vec<PartitionGroup>,
        row_distributions: Vec<(RowDistribution, RowDistribution)>,
    ) -> (u64, u64) {
        if partition_groups.is_empty() {
            panic!("No partition groups found, cannot proceed");
        }
        if row_distributions.is_empty() {
            panic!("No row_distributions found, cannot proceed");
        }
        for (loop_i, current_partn) in enumerate(partition_groups) {
            let current_partn_count = current_partn.n_partitions;

            let current_row_distribution = row_distributions[loop_i].clone();
            let cycle_type_1 = current_row_distribution.0;
            let cycle_type_2 = current_row_distribution.1;

            let cycle_type_1_size = cycle_type_1.n_rows_for_left_and_right;
            let done_cycle_type_1_num: u64;
            let done_cycle_type_1_rows: u64;

            let cycle_type_2_size: u64;
            let mut done_cycle_type_2_num: u64 = 0;
            let done_cycle_type_2_rows: u64;

            if idx < cycle_type_1.n_rows_for_all_cycles {
                // NOTE: we must add shift equal to the size of right group to make it's idx
                //       be calculated correctly on the recursive call step.
                done_cycle_type_1_num = (idx + cycle_type_1.n_rows_for_right) / cycle_type_1_size;
                done_cycle_type_1_rows = done_cycle_type_1_num * cycle_type_1_size;
                if done_cycle_type_1_rows <= idx
                    && idx < cycle_type_1.n_rows_for_left + done_cycle_type_1_rows
                {
                    let ret = partn_offset
                        + (idx - done_cycle_type_1_rows
                            + done_cycle_type_1_num * cycle_type_1.n_rows_for_left)
                            % current_partn_count;
                    return (ret, current_partn.n_rows_per_partition);
                }
            } else {
                done_cycle_type_1_num = cycle_type_1.n_cycles;
                done_cycle_type_1_rows = done_cycle_type_1_num * cycle_type_1_size;

                cycle_type_2_size = cycle_type_2.n_rows_for_left_and_right;
                // NOTE: exclude cumulative size of all the cycles of the first type because it's number
                //       gets considered separately in other parts.
                //       Also, we must add shift equal to the size of the right group to make it's idx
                //       be calculated correctly on the recursive call step.
                done_cycle_type_2_num = (idx - done_cycle_type_1_rows
                    + cycle_type_2.n_rows_for_right)
                    / cycle_type_2_size;
                done_cycle_type_2_rows = done_cycle_type_2_num * cycle_type_2_size;

                let total_done_rows = done_cycle_type_1_rows + done_cycle_type_2_rows;
                if total_done_rows <= idx && idx < total_done_rows + cycle_type_2.n_rows_for_left {
                    let ret = partn_offset
                        + (idx
                            - done_cycle_type_1_num * cycle_type_1.n_rows_for_right
                            - done_cycle_type_2_rows
                            + done_cycle_type_2_num * cycle_type_2.n_rows_for_left)
                            % current_partn_count;
                    return (ret, current_partn.n_rows_per_partition);
                }
            }
            idx = idx
                - done_cycle_type_1_num * cycle_type_1.n_rows_for_left
                - done_cycle_type_2_num * cycle_type_2.n_rows_for_left;
            partn_offset += current_partn_count;
        }
        panic!(
            "Failed to match idx and partition idx! \
            Most probably row distribution values were incorrectly calculated \
            according to the partition groups data."
        );
    }
}

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
    partition_row_presets: HashMap<String, RowDistributionPreset>,
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

    /// Creates a preset for uneven row distribution among partitions
    #[allow(clippy::comparison_chain)]
    pub async fn init_partition_row_distribution_preset(
        &mut self,
        preset_name: &str,
        row_count: u64,
        rows_per_partitions_base: u64,
        mut rows_per_partitions_groups: &str, // "percent:base_multiplier, ..." -> "80:1,15:2,5:4"
    ) -> Result<(), CassError> {
        // Validate input data
        if preset_name.is_empty() {
            return Err(CassError(CassErrorKind::Error(
                "init_partition_row_distribution_preset: 'preset_name' cannot be empty".to_string(),
            )));
        }
        if row_count < 1 {
            return Err(CassError(CassErrorKind::Error(
                "init_partition_row_distribution_preset: 'row_count' cannot be less than 1"
                    .to_string(),
            )));
        }
        if rows_per_partitions_base < 1 {
            return Err(CassError(CassErrorKind::Error(
                "init_partition_row_distribution_preset: 'rows_per_partitions_base' cannot be less than 1".to_string()
            )));
        }

        // Parse the 'rows_per_partitions_groups' string parameter into a HashMap
        let mut partn_multipliers: HashMap<String, (f64, f64)> = HashMap::new();
        if rows_per_partitions_groups.is_empty() {
            rows_per_partitions_groups = "100:1";
        }
        let mut summary_percentage: f64 = 0.0;
        let mut duplicates_dump: Vec<String> = Vec::new();
        for pair in rows_per_partitions_groups.split(',') {
            let processed_pair = &pair.replace(" ", "");
            if duplicates_dump.contains(processed_pair) {
                return Err(CassError(CassErrorKind::Error(format!(
                    "init_partition_row_distribution_preset: found duplicates pairs - '{processed_pair}'")
                )));
            }
            let parts: Vec<&str> = processed_pair.split(':').collect();
            if let (Some(key), Some(value)) = (parts.first(), parts.get(1)) {
                if let (Ok(k), Ok(v)) = (key.parse::<f64>(), value.parse::<f64>()) {
                    let current_pair_key = format!("{k}:{v}");
                    partn_multipliers.insert(current_pair_key.clone(), (k, v));
                    summary_percentage += k;
                    duplicates_dump.push(current_pair_key);
                } else {
                    return Err(CassError(CassErrorKind::Error(format!(
                        "init_partition_row_distribution_preset: \
                        Wrong sub-value provided in the 'rows_per_partitions_groups' parameter: '{processed_pair}'. \
                        It must be set of integer pairs separated with a ':' symbol. Example: '49.1:1,49:2,1.9:2.5'")
                    )));
                }
            }
        }
        if (summary_percentage - 100.0).abs() > 0.01 {
            return Err(CassError(CassErrorKind::Error(format!(
                "init_partition_row_distribution_preset: \
                summary of partition percentage must be '100'. Got '{summary_percentage}' instead"
            ))));
        }

        // Calculate values
        let mut partn_sizes: HashMap<String, (f64, u64)> = HashMap::new();
        let mut partn_counts: HashMap<String, (f64, u64)> = HashMap::new();
        let mut partn_cycle_size: f64 = 0.0;
        for (key, (partn_percent, partn_multiplier)) in &partn_multipliers {
            partn_sizes.insert(
                key.to_string(),
                (
                    *partn_percent,
                    ((rows_per_partitions_base as f64) * partn_multiplier) as u64,
                ),
            );
            let partition_type_size: f64 =
                rows_per_partitions_base as f64 * partn_multiplier * partn_percent / 100.0;
            partn_cycle_size += partition_type_size;
        }
        let mut partn_count: u64 = (row_count as f64 / partn_cycle_size) as u64;
        for (key, (partn_percent, _partn_multiplier)) in &partn_multipliers {
            let current_partn_count: u64 = ((partn_count as f64) * partn_percent / 100.0) as u64;
            partn_counts.insert(key.to_string(), (*partn_percent, current_partn_count));
        }
        partn_count = partn_counts.values().map(|&(_, last)| last).sum();

        // Combine calculated data into a vector of tuples
        let mut actual_row_count: u64 = 0;
        let mut partitions: Vec<(f64, u64, u64, f64)> = Vec::new();
        for (key, (_partn_percent, partn_cnt)) in &partn_counts {
            if let Some((_partn_percent, partn_size)) = partn_sizes.get(key) {
                if let Some((partn_percent, partn_multiplier)) = partn_multipliers.get(key) {
                    partitions.push((*partn_percent, *partn_cnt, *partn_size, *partn_multiplier));
                    actual_row_count += partn_cnt * partn_size;
                }
            }
        }
        partitions.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

        // Adjust partitions based on the difference between requested and total row count
        let mut row_count_diff: u64 = 0;
        if row_count > actual_row_count {
            row_count_diff = row_count - actual_row_count;
            let smallest_partn_count_diff = row_count_diff / partitions[0].2;
            if smallest_partn_count_diff > 0 {
                partn_count += smallest_partn_count_diff;
                partitions[0].1 += smallest_partn_count_diff;
                let additional_rows: u64 = smallest_partn_count_diff * partitions[0].2;
                actual_row_count += additional_rows;
                row_count_diff -= additional_rows;
            }
        } else if row_count < actual_row_count {
            row_count_diff = actual_row_count - row_count;
            let mut smallest_partn_count_diff = row_count_diff / partitions[0].2;
            if !row_count_diff.is_multiple_of(partitions[0].2) {
                smallest_partn_count_diff += 1;
            }
            if smallest_partn_count_diff > 0 {
                partn_count -= smallest_partn_count_diff;
                partitions[0].1 -= smallest_partn_count_diff;
                actual_row_count -= smallest_partn_count_diff * partitions[0].2;
                let additional_rows: u64 = smallest_partn_count_diff * partitions[0].2;
                actual_row_count -= additional_rows;
                row_count_diff = additional_rows - row_count_diff;
            }
        }
        if row_count_diff > 0 {
            partn_count += 1;
            let mut same_size_exists = false;
            for (i, partition) in enumerate(partitions.clone()) {
                if partition.2 == row_count_diff {
                    partitions[i].1 += 1;
                    same_size_exists = true;
                    break;
                }
            }
            if !same_size_exists {
                partitions.push((
                    (100000.0 / (partn_count as f64)).round() / 1000.0,
                    1,
                    row_count_diff,
                    1.0,
                ));
            }
            actual_row_count += row_count_diff;
        }
        partitions.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

        // Print calculated values
        let partitions_str = partitions
            .iter()
            .map(|(_percent, partns, rows, _multiplier)| {
                let percent = *partns as f64 / partn_count as f64 * 100.0;
                let percent_str = format!("{percent:.10}");
                let parts = percent_str.split('.').collect::<Vec<_>>();
                if parts.len() == 2 {
                    let int_part = parts[0];
                    let mut frac_part: String = "".to_string();
                    if parts[1].matches("0").count() != parts[1].len() {
                        frac_part = parts[1]
                            .chars()
                            .take_while(|&ch| ch == '0')
                            .chain(parts[1].chars().filter(|&ch| ch != '0').take(2))
                            .collect::<String>();
                    }
                    if !frac_part.is_empty() {
                        frac_part = format!(".{frac_part}");
                    }
                    format!("{partns}(~{int_part}{frac_part}%):{rows}")
                } else {
                    format!("{}(~{}%):{}", partns, parts[0], rows)
                }
            })
            .collect::<Vec<String>>()
            .join(", ");
        println!(
            "info: init_partition_row_distribution_preset: \
             preset_name={preset_name}\
             , total_partitions={partn_count}\
             , total_rows={actual_row_count}\
             , partitions/rows -> {partitions_str}",
        );

        // Save data for further usage
        let mut partition_groups = vec![];
        for partition in partitions {
            if partition.1 > 0 {
                partition_groups.push(PartitionGroup {
                    n_rows_per_group: partition.1 * partition.2,
                    n_partitions: partition.1,
                    n_rows_per_partition: partition.2,
                });
            }
        }
        // NOTE: sort partition groups in the size descending order to minimize the cumulative
        // computation cost for determining the stress_idx-partition_idx relations.
        partition_groups.sort_by(|a, b| (b.n_rows_per_group).cmp(&(a.n_rows_per_group)));
        let mut row_distribution_preset = RowDistributionPreset::new(partition_groups);
        // NOTE: generate row distributions only after the partition groups are finished with changes
        row_distribution_preset.generate_row_distributions();
        self.partition_row_presets
            .insert(preset_name.to_string(), row_distribution_preset);

        Ok(())
    }

    /// Returns a partition index and size based on the stress operation index and a preset of values
    pub async fn get_partition_info(
        &self,
        preset_name: &str,
        idx: u64,
    ) -> Result<(u64, u64), CassError> {
        let preset = self.partition_row_presets.get(preset_name).ok_or_else(|| {
            CassError(CassErrorKind::PartitionRowPresetNotFound(
                preset_name.to_string(),
            ))
        })?;
        Ok(preset.get_partition_info(idx).await)
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

/// Computes the greatest common divisor of 2 numbers, useful for rows distribution among DB partitions
fn gcd(n1: u64, n2: u64) -> u64 {
    if n2 == 0 {
        n1
    } else {
        gcd(n2, n1 % n2)
    }
}

/// Takes numbers of rows for 2 DB partition groups and calculates the best approach
///   for getting the most dispered and the least clustered, by partition sizes, distribution.
#[rustfmt::skip]
fn max_gcd_with_tail(n1: u64, n2: u64) -> (
    u64,             // greatest common divisor
    (u64, u64),      // (multiplier_based_on_n1, tail_n1)
    (u64, u64),      // (multiplier_based_on_n2, tail_n2)
) {
    let mut max_gcd = 0;
    let mut best_split_n1 = (0, 0);
    let mut best_split_n2 = (0, 0);

    // NOTE: allow to vary number by 1 percent of it's size for extending chances to bigger common divisor
    // That 'tail'/'diff' which is taken out of rows number for computing greatest common divisor
    // later will be used in one of two cycle types utilized for distribution of rows among DB partitions.
    let max_tail_n1 = n1 / 100;
    // Try to split 'n1'
    for tail_n1 in 0..=max_tail_n1 {
        let head_n1 = n1 - tail_n1;
        let gcd_value = gcd(head_n1, n2);
        if gcd_value > max_gcd {
            max_gcd = gcd_value;
            best_split_n1 = ((head_n1 / gcd_value), tail_n1);
            best_split_n2 = ((n2 / gcd_value), 0);
        }
    }

    let max_tail_n2 = n2 / 100;
    // Try to split 'n2'
    for tail_n2 in 0..=max_tail_n2 {
        let head_n2 = n2 - tail_n2;
        let gcd_value = gcd(n1, head_n2);
        if gcd_value > max_gcd {
            max_gcd = gcd_value;
            best_split_n1 = ((n1 / gcd_value), 0);
            best_split_n2 = ((head_n2 / gcd_value), tail_n2);
        }
    }

    (max_gcd, best_split_n1, best_split_n2)
}

#[rustfmt::skip]
#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: if tests which use session object get added
    // then need to define the 'SCYLLA_URI="172.17.0.2:9042"' env var
    // and create a DB session like following:
    //     let session = tokio::runtime::Runtime::new()
    //         .unwrap()
    //         .block_on(async {
    //             let uri = std::env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    //             SessionBuilder::new().known_node(uri).build().await.unwrap()
    //         });
    //      let mut ctxt: Context = Context::new(Some(session), ...);

    fn init_and_use_partition_row_distribution_preset(
        row_count: u64,
        rows_per_partitions_base_and_groups_mapping: Vec<(u64, String)>,
        expected_partition_groups: Vec<PartitionGroup>,
        expected_idx_partition_idx_mapping: Vec<(u64, u64)>,
    ) {
        for (rows_per_partitions_base, rows_per_partitions_groups) in rows_per_partitions_base_and_groups_mapping {
            let mut ctxt: Context = Context::new(
                None, 501, "foo-dc".to_string(), "foo-rack".to_string(), 0,
                RetryInterval::new("1,2").expect("failed to parse retry interval"),
                ValidationStrategy::Ignore,
            );
            let preset_name = "foo_name";

            assert!(ctxt.partition_row_presets.is_empty(), "The 'partition_row_presets' HashMap should not be empty");

            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let _ = ctxt.init_partition_row_distribution_preset(
                    preset_name, row_count, rows_per_partitions_base, &rows_per_partitions_groups).await;
            });

            assert!(!ctxt.partition_row_presets.is_empty(), "The 'partition_row_presets' HashMap should not be empty");
            let actual_preset = ctxt.partition_row_presets.get(preset_name)
                .unwrap_or_else(|| panic!("Preset with name '{preset_name}' was not found"));
            assert_eq!(expected_partition_groups, actual_preset.partition_groups);

            for (idx, expected_partition_idx) in expected_idx_partition_idx_mapping.clone() {
                let (p_idx, _p_size) = tokio::runtime::Runtime::new().unwrap().block_on(async {
                    ctxt.get_partition_info(preset_name, idx).await
                }).expect("Failed to get partition index");
                assert_eq!(
                    expected_partition_idx, p_idx, "{}",
                    format_args!(
                        "Using '{}' idx expected partition_idx is '{}', but got '{}'",
                        idx, expected_partition_idx, p_idx
                    )
                );
            }
        }
    }

    #[test]
    fn test_partition_row_distribution_preset_01_pos_single_group_evenly_divisible() {
        // total_partitions=40, total_rows=1000, partitions/rows -> 40(~100%):25
        init_and_use_partition_row_distribution_preset(
            1000,
            vec![(25, "100:1".to_string())],
            vec![PartitionGroup{ n_rows_per_group: 1000, n_partitions: 40, n_rows_per_partition: 25}],
            vec![
                (0, 0), (1, 1), (2, 2), (39, 39), (40, 0), (41, 1), (42, 2), (999, 39),
                (1000, 0), (1001, 1), (1039, 39), (1040, 0), (1999, 39),
                (2000, 0), (2001, 1), (2039, 39), (2040, 0), (2999, 39),
            ],
        );
    }

    #[test]
    fn test_partition_row_distribution_preset_02_pos_single_group_unevenly_divisible() {
        // total_partitions=77, total_rows=1000, partitions/rows -> 76(~98.71%):13, 1(~1.29%):12
        init_and_use_partition_row_distribution_preset(
            1000,
            vec![(13, "100:1".to_string())],
            vec![
                PartitionGroup{ n_rows_per_group: 988, n_partitions: 76, n_rows_per_partition: 13},
                PartitionGroup{ n_rows_per_group: 12, n_partitions: 1, n_rows_per_partition: 12},
            ],
            vec![
                // 'stress_idx/rows_count' < 1
                // 4 cycles 83+1
                (0, 0),    (75, 75),  (76, 0),   (77, 1),  (82, 6),   (83, 76),
                (84, 7),   (85, 8),   (152, 75), (153, 0), (166, 13), (167, 76),
                (168, 14), (169, 15), (229, 75), (230, 0), (250, 20), (251, 76),
                (252, 21), (253, 22), (306, 75), (307, 0), (334, 27), (335, 76),
                // 8 cycles 82+1
                (336, 28), (337, 29), (383, 75), (384, 0), (417, 33), (418, 76),
                (419, 34), (420, 35), (460, 75), (461, 0), (500, 39), (501, 76),
                (502, 40), (503, 41), (537, 75), (538, 0), (583, 45), (584, 76),
                (585, 46), (586, 47), (614, 75), (615, 0), (666, 51), (667, 76),
                (668, 52), (669, 53), (691, 75), (692, 0), (749, 57), (750, 76),
                (751, 58), (752, 59), (768, 75), (769, 0), (832, 63), (833, 76),
                (834, 64), (835, 65), (845, 75), (846, 0), (915, 69), (916, 76),
                (917, 70), (918, 71), (922, 75), (923, 0), (998, 75), (999, 76),

                // 1 <= 'stress_idx/rows_count' < 2
                (1000, 0),  (1075, 75), (1076, 0),  (1077, 1), (1082, 6),  (1083, 76),
                (1917, 70), (1918, 71), (1922, 75), (1923, 0), (1998, 75), (1999, 76),

                // 2 <= 'stress_idx/rows_count' < 3
                (2000, 0),  (2075, 75), (2076, 0),  (2077, 1), (2082, 6),  (2083, 76),
                (2917, 70), (2918, 71), (2922, 75), (2923, 0), (2998, 75), (2999, 76),
            ],
        );
    }

    #[test]
    fn test_partition_row_distribution_preset_03_pos_multiple_groups_with_implicit_one() {
        // total_partitions=90, total_rows=1000,
        //   partitions/rows -> 46(~51.11%):6, 26(~28.88%):12, 17(~18.88%):24, 1(~1.11%):4
        init_and_use_partition_row_distribution_preset(
            1000,
            vec![
                (6, "50:1,30:2,20:4".to_string()),
                (12, "50:0.5,30:1,20:2".to_string()),
                (24, "50:0.25,30:0.5,20:1".to_string()),
            ],
            vec![
                PartitionGroup{ n_rows_per_group: 408, n_partitions: 17, n_rows_per_partition: 24},
                PartitionGroup{ n_rows_per_group: 312, n_partitions: 26, n_rows_per_partition: 12},
                PartitionGroup{ n_rows_per_group: 276, n_partitions: 46, n_rows_per_partition: 6},
                PartitionGroup{ n_rows_per_group: 4, n_partitions: 1, n_rows_per_partition: 4},
            ],
            vec![
                // 1) Partitions 0-16, 24 rows each. 1 cycle of 12+16, then 16 cycles of 11+16
                //    0-11, 28-38, 55-65, 82-92, 109-119, 136-146, 163-173,
                //    ..., 190-200, 217-227, 244-254, 271-281, ...
                (0, 0), (1, 1), (11, 11),
                (28, 12), (32, 16), (33, 0), (34, 1), (38, 5),
                (55, 6), (56, 7), (65, 16),
                (82, 0), (83, 1), (92, 10),
                (109, 11), (114, 16), (115, 0), (119, 4),
                (136, 5), (146, 15),
                (163, 16), (164, 0), (165, 1), (173, 9),

                // 2) Partitions 17-42, 12 rows each. 2 cycles of 32+28 then 8 cycles of 31+28
                //    12-27, 39-54, 105-108, 120-135, 147-158, 209-216, 228-243, 255-261, ...
                (12, 17), (27, 32),
                (39, 33), (48, 42), (49, 17), (54, 22),
                (105, 23), (108, 26),
                (120, 27), (135, 42),
                (147, 17), (158, 28),
                (209, 29), (216, 36),
                (228, 37), (233, 42), (234, 17), (243, 26),
                (255, 27), (261, 33),

                // 3) Partitions 43-88 , 6 rows each. 4 cycles 69+1
                // 66-81, 93-104, 159-162, 174-189, 201-208, 262-270, 282-285, 287-297, ...
                (66, 43), (81, 58),
                (93, 59), (104, 70),
                (159, 71), (162, 74),
                (174, 75), (187, 88), (188, 43), (189, 44),
                (201, 45), (208, 52),
                (262, 53), (270, 61),
                (282, 62), (285, 65), (287, 66), (297, 76),

                // 4) Partition 89, 4 rows.
                (286, 89), (506, 89), (779, 89), (999, 89),
            ],
        );
    }

    #[test]
    fn test_partition_row_distribution_preset_04_pos_multiple_groups_without_implicit_one() {
        // total_partitions=664, total_rows=10000,
        //   partitions/rows -> 332(~50%):20, 331(~49.84%):10, 1(~0.15%):50
        init_and_use_partition_row_distribution_preset(
            10000,
            vec![(10, "49.9:1,49.9:2, 0.2:5".to_string())],
            vec![
                PartitionGroup{ n_rows_per_group: 6640, n_partitions: 332, n_rows_per_partition: 20},
                PartitionGroup{ n_rows_per_group: 3310, n_partitions: 331, n_rows_per_partition: 10},
                PartitionGroup{ n_rows_per_group: 50, n_partitions: 1, n_rows_per_partition: 50},
            ],
            vec![
                // 1) Partitions 0-331, 20 rows each. 60 cycles of 48+24 then 80 cycles of 47:24
                (0, 0), (47, 47),
                (72, 48), (119, 95),
                (144, 96), (191, 143),
                (216, 144), (263, 191),

                // 2) Partitions 332-662, 10 rows each. 10 cycles of 67+1 then 40 cycles of 66+1
                (48, 332), (71, 355),
                (120, 356), (143, 379),
                (192, 380), (210, 398),
                (212, 399), (215, 402),

                // 3) Partition 663. 50 rows.
                (211, 663), (9999, 663),

                // Repetition
                (10000, 0), (19999, 663),
            ],
        );
    }

    #[test]
    fn test_partition_row_distribution_preset_05_pos_multiple_presets() {
        let name_foo: String = "foo".to_string();
        let name_bar: String = "bar".to_string();
        let mut ctxt: Context = Context::new(
            None, 501, "foo-dc".to_string(), "foo-rack".to_string(), 0,
            RetryInterval::new("1,2").expect("failed to parse retry interval"),
            ValidationStrategy::Ignore,
        );

        assert!(ctxt.partition_row_presets.is_empty(), "The 'partition_row_presets' HashMap should be empty");
        let foo_value = ctxt.partition_row_presets.get(&name_foo);
        assert_eq!(None, foo_value);

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            ctxt.init_partition_row_distribution_preset(
                &name_foo, 1000, 10, "100:1").await
        }).unwrap_or_else(|_| panic!("The '{name_foo}' preset must have been created successfully"));
        assert!(!ctxt.partition_row_presets.is_empty(), "The 'partition_row_presets' HashMap should not be empty");
        ctxt.partition_row_presets.get(&name_foo)
            .unwrap_or_else(|| panic!("Preset with name '{name_foo}' was not found"));

        let absent_bar = ctxt.partition_row_presets.get(&name_bar);
        assert_eq!(None, absent_bar, "{}", format_args!("The '{}' preset was expected to be absent", name_bar));

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            ctxt.init_partition_row_distribution_preset(
                &name_bar, 1000, 10, "90:1,10:2").await
        }).unwrap_or_else(|_| panic!("The '{name_bar}' preset must have been created successfully"));
        ctxt.partition_row_presets.get(&name_bar)
            .unwrap_or_else(|| panic!("Preset with name '{name_bar}' was not found"));
    }

    fn false_input_for_partition_row_distribution_preset(
        preset_name: String,
        row_count: u64,
        rows_per_partitions_base: u64,
        rows_per_partitions_groups: String,
    ) {
        let mut ctxt: Context = Context::new(
            None, 501, "foo-dc".to_string(), "".to_string(), 0,
            RetryInterval::new("1,2").expect("failed to parse retry interval"),
            ValidationStrategy::Ignore,
        );
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ctxt.init_partition_row_distribution_preset(
                &preset_name, row_count, rows_per_partitions_base, &rows_per_partitions_groups).await
        });

        assert!(matches!(result, Err(ref _e)), "Error result was expected, but got: {result:?}");
    }

    #[test]
    fn test_partition_row_distribution_preset_06_neg_empty_preset_name() {
        false_input_for_partition_row_distribution_preset("".to_string(), 1000, 10, "100:1".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_07_neg_zero_rows() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 0, 10, "100:1".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_08_neg_zero_base() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 1000, 0, "100:1".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_09_neg_percentage_is_less_than_100() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 1000, 10, "90:1,9.989:2".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_10_neg_percentage_is_more_than_100() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 1000, 10, "90:1,10.011:2".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_11_neg_duplicated_percentages() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 1000, 10, "50:1 , 50:1".to_string())
    }

    #[test]
    fn test_partition_row_distribution_preset_12_neg_wrong_percentages() {
        false_input_for_partition_row_distribution_preset("foo".to_string(), 1000, 10, "90:1,ten:1".to_string())
    }
}
