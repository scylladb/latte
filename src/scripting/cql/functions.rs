use crate::scripting::functions_common::extract_validation_args;

use super::cass_error::{CassError, CassErrorKind};
use super::context::Context;
use rune::runtime::{Mut, Ref};
use rune::Value;
use std::ops::Deref;

#[rune::function(instance)]
pub async fn prepare(mut ctx: Mut<Context>, key: Ref<str>, cql: Ref<str>) -> Result<(), CassError> {
    ctx.prepare(&key, &cql).await
}

#[rune::function(instance)]
pub async fn execute(ctx: Ref<Context>, cql: Ref<str>) -> Result<Value, CassError> {
    ctx.execute(cql.deref()).await
}

#[rune::function(instance)]
pub async fn execute_with_validation(
    ctx: Ref<Context>,
    cql: Ref<str>,
    validation_args: Vec<Value>,
) -> Result<Value, CassError> {
    match validation_args.as_slice() {
        // (int): expected_rows
        [Value::Integer(expected_rows)] => {
            ctx.execute_with_validation(
                cql.deref(),
                *expected_rows as u64,
                *expected_rows as u64,
                "",
            )
            .await
        }
        // (int, int): expected_rows_num_min, expected_rows_num_max
        [Value::Integer(min), Value::Integer(max)] => {
            ctx.execute_with_validation(cql.deref(), *min as u64, *max as u64, "")
                .await
        }
        // (int, str): expected_rows, custom_err_msg
        [Value::Integer(expected_rows), Value::String(custom_err_msg)] => {
            ctx.execute_with_validation(
                cql.deref(),
                *expected_rows as u64,
                *expected_rows as u64,
                &custom_err_msg.borrow_ref().unwrap(),
            )
            .await
        }
        // (int, int, str): expected_rows_num_min, expected_rows_num_max, custom_err_msg
        [Value::Integer(min), Value::Integer(max), Value::String(custom_err_msg)] => {
            ctx.execute_with_validation(
                cql.deref(),
                *min as u64,
                *max as u64,
                &custom_err_msg.borrow_ref().unwrap(),
            )
            .await
        }
        _ => Err(CassError(CassErrorKind::Error(
            "Invalid arguments for execute_with_validation".to_string(),
        ))),
    }
}

#[rune::function(instance)]
pub async fn execute_with_result(ctx: Ref<Context>, cql: Ref<str>) -> Result<Value, CassError> {
    ctx.execute_with_result(cql.deref()).await
}

#[rune::function(instance)]
pub async fn execute_prepared(
    ctx: Ref<Context>,
    key: Ref<str>,
    params: Value,
) -> Result<Value, CassError> {
    ctx.execute_prepared(&key, params).await
}

#[rune::function(instance)]
pub async fn execute_prepared_with_validation(
    ctx: Ref<Context>,
    key: Ref<str>,
    params: Value,
    validation_args: Vec<Value>,
) -> Result<Value, CassError> {
    let args = extract_validation_args(validation_args).map_err(|e| {
        CassError(CassErrorKind::Error(format!(
            "execute_prepared_with_validation: {e}"
        )))
    })?;

    ctx.execute_prepared_with_validation(
        &key,
        params,
        args.expected_min,
        args.expected_max,
        &args.custom_err_msg,
    )
    .await
}

#[rune::function(instance)]
pub async fn execute_prepared_with_result(
    ctx: Ref<Context>,
    key: Ref<str>,
    params: Value,
) -> Result<Value, CassError> {
    ctx.execute_prepared_with_result(&key, params).await
}

#[rune::function(instance)]
pub async fn batch_prepared(
    ctx: Ref<Context>,
    keys: Vec<Ref<str>>,
    params: Vec<Value>,
) -> Result<(), CassError> {
    ctx.batch_prepared(keys.iter().map(|k| k.deref()).collect(), params)
        .await
}

#[rune::function(instance)]
pub async fn get_datacenters(ctx: Ref<Context>) -> Result<Vec<String>, CassError> {
    ctx.get_datacenters().await
}
