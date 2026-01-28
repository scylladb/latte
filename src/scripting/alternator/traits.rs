use super::alternator_error::AlternatorError;
use super::types::alternator_map_to_rune_object;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::operation::{
    create_table::CreateTableOutput, delete_item::DeleteItemOutput,
    delete_table::DeleteTableOutput, get_item::GetItemOutput, put_item::PutItemOutput,
    query::QueryOutput, scan::ScanOutput, update_item::UpdateItemOutput,
};
use aws_sdk_dynamodb::types::AttributeValue;
use rune::Value;
use std::collections::HashMap;
use std::future::Future;

pub(super) type AlternatorOutputResult =
    Result<(Vec<Value>, u64, Option<HashMap<String, AttributeValue>>), AlternatorError>;

pub(super) trait IntoAlternatorOutput {
    fn into_output(self) -> AlternatorOutputResult;
}

impl IntoAlternatorOutput for GetItemOutput {
    fn into_output(self) -> AlternatorOutputResult {
        if let Some(item) = self.item {
            Ok((vec![alternator_map_to_rune_object(item)?], 1, None))
        } else {
            Ok((vec![], 0, None))
        }
    }
}

impl IntoAlternatorOutput for QueryOutput {
    fn into_output(self) -> AlternatorOutputResult {
        let items = self.items.unwrap_or_default();
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            result.push(alternator_map_to_rune_object(item)?);
        }
        let len = result.len() as u64;
        Ok((result, len, self.last_evaluated_key))
    }
}

impl IntoAlternatorOutput for ScanOutput {
    fn into_output(self) -> AlternatorOutputResult {
        let items = self.items.unwrap_or_default();
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            result.push(alternator_map_to_rune_object(item)?);
        }
        let len = result.len() as u64;
        Ok((result, len, self.last_evaluated_key))
    }
}

macro_rules! impl_into_alternator_output_empty {
    ($($t:ty),*) => {
        $(
            impl IntoAlternatorOutput for $t {
                fn into_output(self) -> AlternatorOutputResult {
                    Ok((vec![], 0, None))
                }
            }
        )*
    };
}

impl_into_alternator_output_empty!(
    PutItemOutput,
    UpdateItemOutput,
    DeleteItemOutput,
    CreateTableOutput,
    DeleteTableOutput
);

impl<T, E, R> IntoAlternatorOutput for Result<T, SdkError<E, R>>
where
    T: IntoAlternatorOutput,
    E: ProvideErrorMetadata,
{
    fn into_output(self) -> AlternatorOutputResult {
        match self {
            Ok(val) => val.into_output(),
            Err(err) => Err(AlternatorError::from(err)),
        }
    }
}

pub(super) trait SendRequest {
    fn send(
        self,
    ) -> impl Future<
        Output = Result<impl IntoAlternatorOutput, SdkError<impl ProvideErrorMetadata, impl Send>>,
    >;
}

pub(super) trait AlternatorRequest: SendRequest + Clone {
    fn set_pagination(
        self,
        token: Option<HashMap<String, AttributeValue>>,
        limit: Option<i32>,
    ) -> Self;
    fn has_pagination(&self) -> bool;
    fn get_limit_val(&self) -> Option<i32>;
}

macro_rules! impl_send_request {
    ($($t:ty),*) => {
        $(
            impl SendRequest for $t {
                fn send(
                    self,
                ) -> impl Future<
                    Output = Result<impl IntoAlternatorOutput, SdkError<impl ProvideErrorMetadata, impl Send>>,
                > {
                    self.send()
                }
            }
        )*
    };
}

macro_rules! impl_alternator_request_no_pagination {
    ($($t:ty),*) => {
        $(
            impl_send_request!($t);
            impl AlternatorRequest for $t {
                fn set_pagination(self, _: Option<HashMap<String, AttributeValue>>, _: Option<i32>) -> Self { self }
                fn has_pagination(&self) -> bool { false }
                fn get_limit_val(&self) -> Option<i32> { None }
            }
        )*
    };
}

impl_alternator_request_no_pagination!(
    aws_sdk_dynamodb::operation::create_table::builders::CreateTableFluentBuilder,
    aws_sdk_dynamodb::operation::delete_table::builders::DeleteTableFluentBuilder,
    aws_sdk_dynamodb::operation::put_item::builders::PutItemFluentBuilder,
    aws_sdk_dynamodb::operation::delete_item::builders::DeleteItemFluentBuilder,
    aws_sdk_dynamodb::operation::get_item::builders::GetItemFluentBuilder,
    aws_sdk_dynamodb::operation::update_item::builders::UpdateItemFluentBuilder
);

impl_send_request!(aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder);
impl_send_request!(aws_sdk_dynamodb::operation::scan::builders::ScanFluentBuilder);

impl AlternatorRequest for aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder {
    fn set_pagination(
        self,
        token: Option<HashMap<String, AttributeValue>>,
        limit: Option<i32>,
    ) -> Self {
        let mut b = self.set_exclusive_start_key(token);
        if let Some(limit) = limit {
            b = b.limit(limit);
        }
        b
    }
    fn has_pagination(&self) -> bool {
        true
    }
    fn get_limit_val(&self) -> Option<i32> {
        *self.get_limit()
    }
}

impl AlternatorRequest for aws_sdk_dynamodb::operation::scan::builders::ScanFluentBuilder {
    fn set_pagination(
        self,
        token: Option<HashMap<String, AttributeValue>>,
        limit: Option<i32>,
    ) -> Self {
        let mut b = self.set_exclusive_start_key(token);
        if let Some(limit) = limit {
            b = b.limit(limit);
        }
        b
    }
    fn has_pagination(&self) -> bool {
        true
    }
    fn get_limit_val(&self) -> Option<i32> {
        *self.get_limit()
    }
}
