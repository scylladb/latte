use rune::alloc::fmt::TryWrite;
use rune::runtime::VmResult;
use rune::{vm_write, Any};
use std::fmt::{Display, Formatter};

#[derive(Any, Debug)]
pub struct AlternatorError(pub AlternatorErrorKind);

#[derive(Debug)]
pub enum AlternatorErrorKind {
    FailedToConnect(String, String),
    QueryRetriesExceeded(String),
    Overloaded(String),
    PartitionRowPresetNotFound(String),
    CustomError(String),
    Error(String),
}

impl AlternatorError {
    pub fn new(kind: AlternatorErrorKind) -> AlternatorError {
        AlternatorError(kind)
    }

    pub fn query_retries_exceeded(retry_number: u64) -> AlternatorError {
        AlternatorError(AlternatorErrorKind::QueryRetriesExceeded(format!(
            "Max retry attempts ({retry_number}) reached"
        )))
    }

    #[rune::function(protocol = STRING_DISPLAY)]
    pub fn string_display(&self, f: &mut rune::runtime::Formatter) -> VmResult<()> {
        vm_write!(f, "{}", self.to_string());
        VmResult::Ok(())
    }
}

impl Display for AlternatorError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            AlternatorErrorKind::FailedToConnect(addr, e) => {
                write!(f, "Failed to connect to Alternator at {}: {}", addr, e)
            }
            AlternatorErrorKind::QueryRetriesExceeded(s) => write!(f, "QueryRetriesExceeded: {s}"),
            AlternatorErrorKind::Overloaded(s) => write!(f, "Overloaded: {s}"),
            AlternatorErrorKind::CustomError(s) => write!(f, "{s}"),
            AlternatorErrorKind::Error(s) => write!(f, "{s}"),
            AlternatorErrorKind::PartitionRowPresetNotFound(s) => {
                write!(f, "Partition row preset not found: {s}")
            }
        }
    }
}

impl std::error::Error for AlternatorError {}

pub type DbError = AlternatorError;
pub type DbErrorKind = AlternatorErrorKind;
