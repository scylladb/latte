use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{VmError, VmResult};
use rune::{vm_write, Any};
use std::fmt::{Debug, Display, Formatter};

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
    SdkError(String),
    BadInput(String),
    ConversionError(String),
    ValidationError(String),
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
            AlternatorErrorKind::BadInput(s) => write!(f, "BadInput: {s}"),
            AlternatorErrorKind::SdkError(s) => write!(f, "SdkError: {s}"),
            AlternatorErrorKind::ConversionError(s) => write!(f, "ConversionError: {s}"),
            AlternatorErrorKind::ValidationError(s) => write!(f, "ValidationError: {s}"),
        }
    }
}

impl std::error::Error for AlternatorError {}

impl From<rune::runtime::AccessError> for AlternatorError {
    fn from(error: rune::runtime::AccessError) -> Self {
        AlternatorError::new(AlternatorErrorKind::Error(error.to_string()))
    }
}

impl From<aws_sdk_dynamodb::error::BuildError> for AlternatorError {
    fn from(error: aws_sdk_dynamodb::error::BuildError) -> Self {
        AlternatorError::new(AlternatorErrorKind::SdkError(error.to_string()))
    }
}

impl From<aws_sdk_dynamodb::waiters::table_exists::WaitUntilTableExistsError> for AlternatorError {
    fn from(error: aws_sdk_dynamodb::waiters::table_exists::WaitUntilTableExistsError) -> Self {
        AlternatorError::new(AlternatorErrorKind::SdkError(error.to_string()))
    }
}

impl<E, R> From<SdkError<E, R>> for AlternatorError
where
    E: ProvideErrorMetadata,
{
    fn from(err: SdkError<E, R>) -> Self {
        AlternatorError::new(AlternatorErrorKind::SdkError(
            err.message().unwrap_or("No message").to_string(),
        ))
    }
}

impl From<VmError> for AlternatorError {
    fn from(error: VmError) -> Self {
        AlternatorError::new(AlternatorErrorKind::ConversionError(error.to_string()))
    }
}

impl From<rune::alloc::Error> for AlternatorError {
    fn from(error: rune::alloc::Error) -> Self {
        AlternatorError::new(AlternatorErrorKind::ConversionError(error.to_string()))
    }
}

pub type DbError = AlternatorError;
pub type DbErrorKind = AlternatorErrorKind;
