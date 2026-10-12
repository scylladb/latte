use crate::scripting::rune_fmt::fmt_rune_value;
use openssl::error::ErrorStack;
use rune::alloc::error::Error as RuneAllocError;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{TypeInfo, VmResult};
use rune::{vm_write, Any, Value};
use scylla::errors::{
    DeserializationError, ExecutionError, NewSessionError, PrepareError, RowsError,
};
use scylla::response::query_result::{FirstRowError, IntoRowsResultError};
use scylla::value::ValueOverflow;
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
        params: Option<&Value>,
        err: ExecutionError,
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: rune_value_to_param_strings(params),
        };
        let kind = match err {
            ExecutionError::RequestTimeout(_) => CassErrorKind::Overloaded(query, err),
            _ => CassErrorKind::QueryExecution(query, err),
        };
        CassError(kind)
    }

    pub fn query_validation_error(
        cql: &str,
        params: Option<&Value>,
        expected_rows_num_min: u64,
        expected_rows_num_max: u64,
        actual_rows_num: u64,
        custom_err_msg: String,
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: rune_value_to_param_strings(params),
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
        params: Option<&Value>,
    ) -> CassError {
        let query = QueryInfo {
            cql: cql.to_string(),
            params: rune_value_to_param_strings(params),
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
    #[rune::function(protocol = DISPLAY_FMT)]
    pub fn string_display(&self, f: &mut rune::runtime::Formatter) -> VmResult<()> {
        let _ = vm_write!(f, "{}", self.to_string());
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

/// Formats a rune Value into a list of parameter display strings for error messages
fn rune_value_to_param_strings(value: Option<&Value>) -> Vec<String> {
    let Some(v) = value else {
        return vec![];
    };
    if let Ok(tuple) = v.borrow_ref::<rune::runtime::OwnedTuple>() {
        return tuple.iter().map(fmt_rune_value).collect();
    }
    if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
        return vec.iter().map(fmt_rune_value).collect();
    }
    if let Ok(obj) = v.borrow_ref::<rune::runtime::Object>() {
        return obj
            .iter()
            .map(|(k, v)| format!("{k}: {}", fmt_rune_value(v)))
            .collect();
    }
    vec![fmt_rune_value(v)]
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

#[cfg(test)]
mod tests {
    use super::*;

    use rune::alloc::String as RuneString;
    use rune::runtime::{Object, Vec as RuneVec};

    // Unit tests run with no rune VM environment set up, the same as code polled
    // from within native async functions, which is where the error messages are built.

    fn rune_string(s: &str) -> Value {
        Value::new(RuneString::try_from(s).unwrap()).unwrap()
    }

    fn rune_vec(items: Vec<Value>) -> Value {
        let mut v = RuneVec::new();
        for item in items {
            v.push(item).unwrap();
        }
        Value::vec(v.into_inner()).unwrap()
    }

    fn rune_object(pairs: Vec<(&str, Value)>) -> Value {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(RuneString::try_from(k).unwrap(), v).unwrap();
        }
        Value::new(obj).unwrap()
    }

    #[test]
    fn test_query_validation_error_shows_params() {
        let params = rune_vec(vec![Value::from(5i64), rune_string("hello")]);
        let err = CassError::query_validation_error(
            "INSERT INTO t (id, val) VALUES (?, ?)",
            Some(&params),
            1,
            1,
            0,
            String::new(),
        );
        let msg = err.to_string();
        assert!(msg.contains("with params [5, \"hello\"]"), "got: {msg}");
        assert!(!msg.contains("Missing interface environment"), "got: {msg}");
    }

    #[test]
    fn test_param_strings_of_object() {
        let params = rune_object(vec![("id", Value::from(1i64)), ("val", rune_string("x"))]);
        let mut strings = rune_value_to_param_strings(Some(&params));
        strings.sort();
        assert_eq!(strings, vec!["id: 1", "val: \"x\""]);
    }
}
