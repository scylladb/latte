use openssl::error::ErrorStack;
use rune::alloc::error::Error as RuneAllocError;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{TypeInfo, VmResult};
use rune::{vm_write, Any};
use scylla::errors::{
    DeserializationError, ExecutionError, NewSessionError, PrepareError, RowsError,
};
use scylla::response::query_result::{FirstRowError, IntoRowsResultError};
use scylla::value::{CqlValue, ValueOverflow};
use std::fmt::{Display, Formatter};

#[derive(Any, Debug)]
pub struct CassError(pub CassErrorKind);

impl CassError {
    pub fn new(kind: CassErrorKind) -> CassError {
        CassError(kind)
    }

    pub fn prepare_error(cql: &str, err: PrepareError) -> CassError {
        CassError(CassErrorKind::Prepare(cql.to_string(), err))
    }

    pub fn query_execution_error(
        cql: &str,
        params: &[Option<CqlValue>],
        err: ExecutionError,
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: params
                .iter()
                .map(|v| cql_value_obj_to_string(v.as_ref()))
                .collect(),
        };
        let kind = match err {
            ExecutionError::RequestTimeout(_) => CassErrorKind::Overloaded(query, err),
            _ => CassErrorKind::QueryExecution(query, err),
        };
        CassError(kind)
    }

    pub fn query_validation_error(
        cql: &str,
        params: &[Option<CqlValue>],
        expected_rows_num_min: u64,
        expected_rows_num_max: u64,
        actual_rows_num: u64,
        custom_err_msg: String,
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: params
                .iter()
                .map(|v| cql_value_obj_to_string(v.as_ref()))
                .collect(),
        };
        CassError(CassErrorKind::QueryResponseValidationError(
            query,
            expected_rows_num_min,
            expected_rows_num_max,
            actual_rows_num,
            custom_err_msg,
        ))
    }

    pub fn query_response_validation_not_applicable_error(
        cql: &str,
        params: &[Option<CqlValue>],
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: params
                .iter()
                .map(|v| cql_value_obj_to_string(v.as_ref()))
                .collect(),
        };
        CassError(CassErrorKind::QueryResponseValidationNotApplicableError(
            query,
        ))
    }

    pub fn query_retries_exceeded(retry_number: u64) -> CassError {
        CassError(CassErrorKind::QueryRetriesExceeded(format!(
            "Max retry attempts ({retry_number}) reached",
        )))
    }
}

impl From<IntoRowsResultError> for CassError {
    fn from(err: IntoRowsResultError) -> Self {
        CassError(CassErrorKind::Error(format!(
            "Failed to get result rows: {err}"
        )))
    }
}

#[derive(Debug)]
pub enum CassErrorKind {
    SslConfiguration(ErrorStack),
    FailedToConnect(Vec<String>, NewSessionError),
    PreparedStatementNotFound(String),
    PartitionRowPresetNotFound(String),
    QueryRetriesExceeded(String),
    QueryParamConversion(String, String, Option<String>),
    ValueOutOfRange(String, String),
    InvalidNumberOfQueryParams,
    InvalidQueryParamsObject(TypeInfo),
    Prepare(String, PrepareError),
    Overloaded(QueryInfo, ExecutionError),

    QueryExecution(QueryInfo, ExecutionError),
    QueryResponseValidationError(QueryInfo, u64, u64, u64, String),
    QueryResponseValidationNotApplicableError(QueryInfo),

    Error(String),
    CustomError(String),
}

#[derive(Debug)]
pub struct QueryInfo {
    cql: String,
    params: Vec<String>,
}

impl CassError {
    #[rune::function(protocol = STRING_DISPLAY)]
    pub fn string_display(&self, f: &mut rune::runtime::Formatter) -> VmResult<()> {
        vm_write!(f, "{}", self.to_string());
        VmResult::Ok(())
    }

    pub fn display(&self, buf: &mut String) -> std::fmt::Result {
        use std::fmt::Write;
        match &self.0 {
            CassErrorKind::SslConfiguration(e) => {
                write!(buf, "SSL configuration error: {e}")
            }
            CassErrorKind::FailedToConnect(hosts, e) => {
                write!(buf, "Could not connect to {}: {}", hosts.join(","), e)
            }
            CassErrorKind::PreparedStatementNotFound(s) => {
                write!(buf, "Prepared statement not found: {s}")
            }
            CassErrorKind::PartitionRowPresetNotFound(s) => {
                write!(buf, "Partition-row preset not found: {s}")
            }
            CassErrorKind::QueryRetriesExceeded(s) => {
                write!(buf, "QueryRetriesExceeded: {s}")
            }
            CassErrorKind::ValueOutOfRange(v, t) => {
                write!(buf, "Value {v} out of range for CQL type {t:?}")
            }
            CassErrorKind::QueryParamConversion(v, t, None) => {
                write!(buf, "Cannot convert value {v} to CQL type {t:?}")
            }
            CassErrorKind::QueryParamConversion(v, t, Some(e)) => {
                write!(buf, "Cannot convert value {v} to CQL type {t:?}: {e}")
            }
            CassErrorKind::InvalidNumberOfQueryParams => {
                write!(buf, "Incorrect number of query parameters")
            }
            CassErrorKind::InvalidQueryParamsObject(t) => {
                write!(buf, "Value of type {t} cannot by used as query parameters; expected a list or object")
            }
            CassErrorKind::Prepare(q, e) => {
                write!(buf, "Failed to prepare query \"{q}\": {e}")
            }
            CassErrorKind::Overloaded(q, e) => {
                write!(buf, "Overloaded when executing query {q}: {e}")
            }
            CassErrorKind::QueryExecution(q, e) => {
                write!(buf, "Failed to execute query {q}: {e}")
            }
            CassErrorKind::QueryResponseValidationError(q, emin, emax, a, err) => {
                let custom_err = if !err.is_empty() {
                    format!(" . Custom error msg: {err}")
                } else {
                    "".to_string()
                };
                let expected = if emin == emax {
                    format!("'{emin}' rows")
                } else {
                    format!("'{emin}<=N<={emax}' rows")
                };
                write!(
                    buf,
                    "Expected {expected} in the response, but got '{a}'. Query: {q}{custom_err}"
                )
            }
            CassErrorKind::QueryResponseValidationNotApplicableError(q) => {
                write!(
                    buf,
                    "Response rows can be validated only for 'SELECT' queries, Query: {q}"
                )
            }
            CassErrorKind::Error(s) => {
                write!(buf, "Error: {s}")
            }
            CassErrorKind::CustomError(s) => {
                write!(buf, "CustomError: {s}")
            }
        }
    }
}

impl Display for CassError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut buf = String::new();
        self.display(&mut buf).unwrap();
        write!(f, "{buf}")
    }
}

impl From<Box<CassError>> for CassError {
    fn from(boxed_err: Box<CassError>) -> Self {
        *boxed_err
    }
}

impl From<ErrorStack> for Box<CassError> {
    fn from(e: ErrorStack) -> Box<CassError> {
        Box::new(CassError(CassErrorKind::SslConfiguration(e)))
    }
}

impl From<ErrorStack> for CassError {
    fn from(e: ErrorStack) -> CassError {
        CassError(CassErrorKind::SslConfiguration(e))
    }
}

impl From<ValueOverflow> for Box<CassError> {
    fn from(e: ValueOverflow) -> Box<CassError> {
        Box::new(CassError(CassErrorKind::Error(e.to_string())))
    }
}

impl From<ValueOverflow> for CassError {
    fn from(e: ValueOverflow) -> CassError {
        CassError(CassErrorKind::Error(e.to_string()))
    }
}

impl From<FirstRowError> for CassError {
    fn from(e: FirstRowError) -> CassError {
        CassError(CassErrorKind::Error(e.to_string()))
    }
}

impl From<DeserializationError> for CassError {
    fn from(e: DeserializationError) -> CassError {
        CassError(CassErrorKind::Error(e.to_string()))
    }
}

impl From<RowsError> for CassError {
    fn from(e: RowsError) -> CassError {
        CassError(CassErrorKind::Error(e.to_string()))
    }
}

impl From<RowsError> for Box<CassError> {
    fn from(e: RowsError) -> std::boxed::Box<CassError> {
        Box::new(CassError(CassErrorKind::Error(e.to_string())))
    }
}

impl From<RuneAllocError> for CassError {
    fn from(e: RuneAllocError) -> CassError {
        CassError(CassErrorKind::Error(e.to_string()))
    }
}

impl From<RuneAllocError> for Box<CassError> {
    fn from(e: RuneAllocError) -> std::boxed::Box<CassError> {
        Box::new(CassError(CassErrorKind::Error(e.to_string())))
    }
}

impl From<std::num::TryFromIntError> for Box<CassError> {
    fn from(e: std::num::TryFromIntError) -> std::boxed::Box<CassError> {
        Box::new(CassError(CassErrorKind::Error(e.to_string())))
    }
}

impl std::error::Error for CassError {}

/// Transforms a CqlValue object to a string dedicated to be part of CassError message
pub fn cql_value_obj_to_string(v: Option<&CqlValue>) -> String {
    let no_transformation_size_limit = 32;
    match v {
        // Replace big string- and bytes-alike object values with its size labels
        Some(CqlValue::Text(param)) if param.len() > no_transformation_size_limit => {
            format!("Text(<size>={})", param.len())
        }
        Some(CqlValue::Ascii(param)) if param.len() > no_transformation_size_limit => {
            format!("Ascii(<size>={})", param.len())
        }
        Some(CqlValue::Blob(param)) if param.len() > no_transformation_size_limit => {
            format!("Blob(<size>={})", param.len())
        }
        Some(CqlValue::UserDefinedType {
            name,
            keyspace,
            fields,
        }) => {
            let mut result =
                format!("UDT {{ keyspace: \"{keyspace}\", type_name: \"{name}\", fields: [");
            for (field_name, field_value) in fields {
                let field_string = match field_value {
                    Some(field) => cql_value_obj_to_string(Some(field)),
                    None => String::from("None"),
                };
                result.push_str(&format!("(\"{field_name}\", {field_string}), "));
            }
            if result.len() >= 2 {
                result.truncate(result.len() - 2);
            }
            result.push_str("] }");
            result
        }
        Some(CqlValue::List(elements)) => {
            let mut result = String::from("List([");
            for element in elements {
                let element_string = cql_value_obj_to_string(Some(element));
                result.push_str(&element_string);
                result.push_str(", ");
            }
            if result.len() >= 2 {
                result.truncate(result.len() - 2);
            }
            result.push_str("])");
            result
        }
        Some(CqlValue::Set(elements)) => {
            let mut result = String::from("Set([");
            for element in elements {
                let element_string = cql_value_obj_to_string(Some(element));
                result.push_str(&element_string);
                result.push_str(", ");
            }
            if result.len() >= 2 {
                result.truncate(result.len() - 2);
            }
            result.push_str("])");
            result
        }
        Some(CqlValue::Map(pairs)) => {
            let mut result = String::from("Map({");
            for (key, value) in pairs {
                let key_string = cql_value_obj_to_string(Some(key));
                let value_string = cql_value_obj_to_string(Some(value));
                result.push_str(&format!("({key_string}: {value_string}), "));
            }
            if result.len() >= 2 {
                result.truncate(result.len() - 2);
            }
            result.push_str("})");
            result
        }
        None => String::from("None"),
        _ => format!("{v:?}"),
    }
}

impl Display for QueryInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\"{}\" with params [{}]",
            self.cql,
            self.params.join(", ")
        )
    }
}

pub type DbError = CassError;
pub type DbErrorKind = CassErrorKind;
