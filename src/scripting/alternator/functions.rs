use crate::config::ValidationStrategy;
use crate::scripting::alternator::traits::{
    AlternatorRequest, IntoAlternatorOutput, PaginationToken,
};
use crate::scripting::functions_common::{extract_validation_args, ValidationArgs};
use crate::scripting::retry_error::handle_retry_error;

use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use super::context::Context;
use super::types::{
    alternator_map_to_rune_object, hashmap_to_rune_object, rune_object_to_alternator_map,
};
use super::types::{BSET_KEY, NSET_KEY, SSET_KEY};
use aws_sdk_dynamodb::client::Waiters;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, DeleteRequest, KeySchemaElement, KeyType, KeysAndAttributes, PutRequest,
    ScalarAttributeType, WriteRequest,
};
use rune::runtime::{Object, Ref, VmResult};
use rune::{ToValue, Value};
use std::cmp::min;
use std::collections::HashMap;
use std::ops::Deref;
use std::time::Duration;
use tokio::time::Instant;

fn bad_input<T>(msg: impl Into<String>) -> Result<T, AlternatorError> {
    Err(AlternatorError::new(AlternatorErrorKind::BadInput(
        msg.into(),
    )))
}

/// Gets the name and type of a primary or sort key from a object.
fn extract_key_definition(
    object: &Object,
) -> Result<(String, ScalarAttributeType), AlternatorError> {
    let key_name = if let Some(v) = object.get("name") {
        if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
            s.as_str().to_string()
        } else {
            return bad_input("Key definition object must have a 'name' field");
        }
    } else {
        return bad_input("Key definition object must have a 'name' field");
    };

    let key_type = if let Some(v) = object.get("type") {
        if let Ok(t) = v.borrow_ref::<rune::alloc::String>() {
            match t.as_str() {
                "N" => ScalarAttributeType::N,
                "S" => ScalarAttributeType::S,
                "B" => ScalarAttributeType::B,
                other => {
                    return bad_input(format!(
                        "Invalid key type: {}, only N, S, and B are allowed.",
                        other
                    ))
                }
            }
        } else {
            return bad_input("Key definition object must have a 'type' field");
        }
    } else {
        return bad_input("Key definition object must have a 'type' field");
    };

    Ok((key_name, key_type))
}

fn extract_attribute_names(object: &Object) -> Result<HashMap<String, String>, AlternatorError> {
    object
        .iter()
        .map(|(k, v)| {
            if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
                Ok((k.to_string(), s.as_str().to_string()))
            } else {
                bad_input("Attribute names must be strings")
            }
        })
        .collect::<Result<_, _>>()
}

async fn handle_request_with_pagination(
    ctx: &Context,
    builder: impl AlternatorRequest,
    auto_paginate: bool,
) -> Result<(Vec<Value>, Option<PaginationToken>), AlternatorError> {
    let mut token: Option<PaginationToken> = None;
    let mut current_attempt_num = 0;
    let mut all_pages_duration = Duration::ZERO;
    let mut all_items = Vec::new();
    let mut total_item_count = 0;
    let query_limit = builder.get_limit_val();

    while current_attempt_num <= ctx.retry_number {
        let mut current_builder = builder.clone();
        if builder.has_pagination() {
            let page_size = match query_limit {
                Some(limit) => min(ctx.get_page_size() as i32, limit - total_item_count as i32),
                None => ctx.get_page_size() as i32,
            };
            current_builder = current_builder.set_pagination(token.clone(), Some(page_size));
        }

        let start_time = ctx.stats.try_lock().unwrap().start_request();
        let resp = current_builder.send().await;
        let duration = Instant::now() - start_time;

        match resp.into_output() {
            Ok((page_items, item_count, next_token)) => {
                all_pages_duration += duration;
                all_items.extend(page_items);
                total_item_count += item_count;
                token = next_token;

                if let Some(limit) = query_limit {
                    if total_item_count as i32 >= limit {
                        ctx.stats
                            .try_lock()
                            .unwrap()
                            .complete_request(all_pages_duration, total_item_count);
                        return Ok((all_items, token));
                    }
                }

                if token.is_some() && builder.has_pagination() {
                    if auto_paginate {
                        current_attempt_num = 0; // reset retries for next page
                        continue;
                    } else {
                        ctx.stats
                            .try_lock()
                            .unwrap()
                            .complete_request(all_pages_duration, total_item_count);
                        return Ok((all_items, token));
                    }
                }

                ctx.stats
                    .try_lock()
                    .unwrap()
                    .complete_request(all_pages_duration, total_item_count);
                return Ok((all_items, token));
            }
            Err(e) => {
                let current_error = e;
                handle_retry_error(ctx, current_attempt_num, current_error).await;
                current_attempt_num += 1;
                continue; // try again the same page
            }
        };
    }
    Err(AlternatorError::query_retries_exceeded(ctx.retry_number))
}

async fn handle_request(
    ctx: &Context,
    builder: impl AlternatorRequest,
) -> Result<Vec<Value>, AlternatorError> {
    Ok(handle_request_with_pagination(ctx, builder, true).await?.0)
}

async fn handle_request_with_validation(
    ctx: &Context,
    builder: impl AlternatorRequest,
    validation: Option<ValidationArgs>,
    operation_name: &str,
) -> Result<Vec<Value>, AlternatorError> {
    let mut current_attempt_num: u64 = 0;
    loop {
        let (result, _) = handle_request_with_pagination(ctx, builder.clone(), true).await?;

        let validation = match validation {
            None => return Ok(result),
            Some(ref v) => v,
        };

        let item_count = result.len() as u64;
        if item_count >= validation.expected_min && item_count <= validation.expected_max {
            return Ok(result);
        }

        let current_error = AlternatorError::new(AlternatorErrorKind::ValidationError(format!(
            "{operation_name} returned {item_count} items, expected between {} and {} {}",
            validation.expected_min, validation.expected_max, validation.custom_err_msg
        )));

        match ctx.validation_strategy {
            ValidationStrategy::Retry => {
                if current_attempt_num >= ctx.retry_number {
                    return Err(current_error);
                }
                handle_retry_error(ctx, current_attempt_num, current_error).await;
                current_attempt_num += 1;
            }
            ValidationStrategy::FailFast => {
                return Err(current_error);
            }
            ValidationStrategy::Ignore => {
                handle_retry_error(ctx, current_attempt_num, current_error).await;
                return Ok(result);
            }
        }
    }
}

fn format_batch_result(
    items: Vec<Value>,
    token: Option<PaginationToken>,
    auto_paginate: bool,
    with_result: bool,
) -> Result<Value, AlternatorError> {
    if !with_result {
        return Ok(Value::from(()));
    }

    if auto_paginate {
        return Ok(items.to_value()?);
    }

    let mut res_map = HashMap::new();

    res_map.insert("items".to_string(), items.to_value()?);

    match token {
        Some(PaginationToken::UnprocessedKeys(u_keys)) => {
            let unprocessed_map: HashMap<String, Value> = u_keys
                .into_iter()
                .map(|(table_name, keys_attr)| {
                    let keys: Vec<Value> = keys_attr
                        .keys
                        .into_iter()
                        .map(alternator_map_to_rune_object)
                        .collect::<Result<_, _>>()?;
                    Ok((table_name, keys.to_value()?))
                })
                .collect::<Result<_, AlternatorError>>()?;

            res_map.insert(
                "unprocessed_keys".to_string(),
                hashmap_to_rune_object(unprocessed_map)?,
            );
        }
        Some(PaginationToken::UnprocessedItems(u_items)) => {
            let unprocessed_map: HashMap<String, Value> = u_items
                .into_iter()
                .map(|(table_name, reqs)| {
                    let requests: Vec<Value> = reqs
                        .into_iter()
                        .map(|req| {
                            let mut req_map = HashMap::new();

                            if let Some(put) = req.put_request {
                                req_map.insert("type".to_string(), "put".to_value()?);
                                req_map.insert(
                                    "item".to_string(),
                                    alternator_map_to_rune_object(put.item)?,
                                );
                            } else if let Some(del) = req.delete_request {
                                req_map.insert("type".to_string(), "delete".to_value()?);
                                req_map.insert(
                                    "key".to_string(),
                                    alternator_map_to_rune_object(del.key)?,
                                );
                            }

                            hashmap_to_rune_object(req_map)
                        })
                        .collect::<Result<_, AlternatorError>>()?;

                    Ok((table_name, requests.to_value()?))
                })
                .collect::<Result<_, AlternatorError>>()?;

            res_map.insert(
                "unprocessed_items".to_string(),
                hashmap_to_rune_object(unprocessed_map)?,
            );
        }
        _ => {}
    }

    hashmap_to_rune_object(res_map)
}

/// Creates a new table.
///
/// # Arguments
/// * `table_name` - The name of the table to create.
/// * `params` - Table definition parameters. Can be a string (defining just the primary key name) or an object containing:
///   - `primary_key`: The primary key definition. Can be a string (name) or an object with `name` and `type`.
///   - `sort_key`: The sort key definition (optional). Can be a string (name) or an object with `name` and `type`.
#[rune::function(instance)]
pub async fn create_table(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    params: Value,
) -> Result<(), AlternatorError> {
    let client = ctx.get_client()?;

    // Extract primary key definition
    let (pk_name, pk_type) = if let Ok(s) = params.borrow_ref::<rune::alloc::String>() {
        (s.as_str().to_string(), ScalarAttributeType::S)
    } else if let Ok(o) = params.borrow_ref::<Object>() {
        match o.get("primary_key") {
            Some(v) if v.borrow_ref::<rune::alloc::String>().is_ok() => (
                v.borrow_ref::<rune::alloc::String>()
                    .unwrap()
                    .as_str()
                    .to_string(),
                ScalarAttributeType::S,
            ),
            Some(v) => {
                if let Ok(pk_obj) = v.borrow_ref::<Object>() {
                    extract_key_definition(&pk_obj)?
                } else {
                    return bad_input("Invalid 'primary_key' object in params");
                }
            }
            _ => return bad_input("Invalid 'primary_key' object in params"),
        }
    } else {
        return bad_input("Params must be a string or an object");
    };

    // Extract sort key definition if present
    let sk = if let Ok(o) = params.borrow_ref::<Object>() {
        match o.get("sort_key") {
            Some(v) if v.borrow_ref::<rune::alloc::String>().is_ok() => Some((
                v.borrow_ref::<rune::alloc::String>()
                    .unwrap()
                    .as_str()
                    .to_string(),
                ScalarAttributeType::S,
            )),
            Some(v) => {
                if let Ok(sk_obj) = v.borrow_ref::<Object>() {
                    Some(extract_key_definition(&sk_obj)?)
                } else {
                    return bad_input("Invalid 'sort_key' object in params");
                }
            }
            None => None,
        }
    } else {
        None
    };

    let mut builder = client
        .create_table()
        .table_name(table_name.deref())
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest);

    builder = builder.key_schema(
        KeySchemaElement::builder()
            .attribute_name(pk_name.clone())
            .key_type(KeyType::Hash)
            .build()?,
    );

    builder = builder.attribute_definitions(
        AttributeDefinition::builder()
            .attribute_name(pk_name)
            .attribute_type(pk_type)
            .build()?,
    );

    if let Some((sk_name, sk_type)) = sk {
        builder = builder.key_schema(
            KeySchemaElement::builder()
                .attribute_name(sk_name.clone())
                .key_type(KeyType::Range)
                .build()?,
        );

        builder = builder.attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name(sk_name)
                .attribute_type(sk_type)
                .build()?,
        );
    }

    builder.send().await?;

    client
        .wait_until_table_exists()
        .table_name(table_name.deref())
        .wait(std::time::Duration::from_secs(15))
        .await?;

    Ok(())
}

/// Deletes a table.
#[rune::function(instance)]
pub async fn delete_table(ctx: Ref<Context>, table_name: Ref<str>) -> Result<(), AlternatorError> {
    let client = ctx.get_client()?;

    client
        .delete_table()
        .table_name(table_name.deref())
        .send()
        .await?;

    Ok(())
}

/// Puts an item into the table.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `item` - The item to insert. An object where keys are attribute names and values are attribute values.
#[rune::function(instance)]
pub async fn put(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    item: Ref<Object>,
) -> Result<(), AlternatorError> {
    let client = ctx.get_client()?;

    let builder = client
        .put_item()
        .table_name(table_name.deref())
        .set_item(Some(rune_object_to_alternator_map(&item)?));

    handle_request(&ctx, builder).await?;

    Ok(())
}

/// Deletes an item from the table.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `key` - The primary key of the item to delete. An object containing the partition key
///   (and sort key if the table has one).
#[rune::function(instance)]
pub async fn delete(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    key: Ref<Object>,
) -> Result<(), AlternatorError> {
    let client = ctx.get_client()?;

    let builder = client
        .delete_item()
        .table_name(table_name.deref())
        .set_key(Some(rune_object_to_alternator_map(&key)?));

    handle_request(&ctx, builder).await?;

    Ok(())
}

/// Gets an item from the table.
///
/// The `options` object can be replaced with `()` if no options are needed.
///
/// If `with_result` is set to true, an `Option<Value>` containing the item if present is returned.
/// Otherwise the unit value is returned.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `key` - The primary key of the item to get. An object containing the partition key
///   (and sort key if the table has one).
/// * `options` - Optional parameters object:
///   - `consistent_read`: Boolean to enable consistent read (default: false).
///   - `with_result`: If true, the result item is returned (default: false).
#[rune::function(instance)]
pub async fn get(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    key: Ref<Object>,
    options: Value,
) -> Result<Value, AlternatorError> {
    let client = ctx.get_client()?;

    let mut builder = client
        .get_item()
        .table_name(table_name.deref())
        .set_key(Some(rune_object_to_alternator_map(&key)?));

    if let Ok(opts) = options.borrow_ref::<Object>() {
        if let Some(b) = opts.get("consistent_read").and_then(|v| v.as_bool().ok()) {
            builder = builder.consistent_read(b);
        }
    }

    let result = handle_request(&ctx, builder).await?;

    if let Ok(opts) = options.borrow_ref::<Object>() {
        if opts.get("with_result").and_then(|v| v.as_bool().ok()) == Some(true) {
            return Ok(result.into_iter().next().to_value()?);
        }
    }

    Ok(Value::from(()))
}

/// Updates an item in the table.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `key` - The primary key of the item to update. An object containing the partition key
///   (and sort key if the table has one).
/// * `params` - Parameters for the update operation. An object containing:
///   - `update`: The update expression string.
///   - `attribute_names`: A map of attribute name placeholders (starting with #) to actual names.
///   - `attribute_values`: A map of attribute value placeholders (starting with :) to values.
#[rune::function(instance)]
pub async fn update(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    key: Ref<Object>,
    params: Ref<Object>,
) -> Result<(), AlternatorError> {
    let client = ctx.get_client()?;

    let mut builder = client
        .update_item()
        .table_name(table_name.deref())
        .set_key(Some(rune_object_to_alternator_map(&key)?));

    if let Some(v) = params.get("update") {
        if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
            builder = builder.update_expression(s.as_str().to_string());
        }
    }

    if let Some(v) = params.get("attribute_names") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder = builder.set_expression_attribute_names(Some(extract_attribute_names(&obj)?));
        }
    }

    if let Some(v) = params.get("attribute_values") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder =
                builder.set_expression_attribute_values(Some(rune_object_to_alternator_map(&obj)?));
        }
    }

    handle_request(&ctx, builder).await?;

    Ok(())
}

/// Batch retrieves items from one or multiple tables.
///
/// If `with_result` is set to true, the retrieved items are returned.
/// Otherwise, the unit value is returned.
///
/// # Arguments
/// * `requests` - An object mapping table names to a list of primary key objects.
/// * `options` - Optional parameters. An object containing:
///   - `consistent_read`: Boolean to enable consistent read for all tables (default: false).
///   - `with_result`: If true, the retrieved items are returned (default: false).
///   - `get_unprocessed`: If true, disables auto-pagination. When `with_result: true` returns an object with `items` and `unprocessed_keys`.
#[rune::function(instance)]
pub async fn batch_get_item(
    ctx: Ref<Context>,
    requests: Ref<Object>,
    options: Value,
) -> Result<Value, AlternatorError> {
    let client = ctx.get_client()?;

    let mut with_result = false;
    let mut get_unprocessed = false;
    let mut consistent_read = false;

    if let Ok(opts_ref) = options.borrow_ref::<Object>() {
        if let Some(c) = opts_ref
            .get("consistent_read")
            .and_then(|v| v.as_bool().ok())
        {
            consistent_read = c;
        }
        if let Some(w) = opts_ref.get("with_result").and_then(|v| v.as_bool().ok()) {
            with_result = w;
        }
        if let Some(u) = opts_ref
            .get("get_unprocessed")
            .and_then(|v| v.as_bool().ok())
        {
            get_unprocessed = u;
        }
    }

    let request_items: HashMap<String, KeysAndAttributes> = requests
        .iter()
        .map(|(table_name, keys_val)| {
            let keys_vec = if let Ok(vec) = keys_val.borrow_ref::<rune::runtime::Vec>() {
                vec
            } else {
                return bad_input("Each table's requests must be a list of keys");
            };

            let keys_list = keys_vec
                .iter()
                .map(|key_val| {
                    if let Ok(key_obj) = key_val.borrow_ref::<Object>() {
                        rune_object_to_alternator_map(&key_obj)
                    } else {
                        bad_input("Each key in the keys list must be an object")
                    }
                })
                .collect::<Result<_, _>>()?;

            let keys_and_attributes = KeysAndAttributes::builder()
                .set_keys(Some(keys_list))
                .consistent_read(consistent_read)
                .build()?;

            Ok((table_name.to_string(), keys_and_attributes))
        })
        .collect::<Result<_, AlternatorError>>()?;

    let builder = client
        .batch_get_item()
        .set_request_items(Some(request_items));

    let (result_items, token) =
        handle_request_with_pagination(&ctx, builder, !get_unprocessed).await?;

    format_batch_result(result_items, token, !get_unprocessed, with_result)
}

/// Batch writes items to one or multiple tables.
///
/// # Arguments
/// * `requests` - An object mapping table names to a list of write requests. Each request is an object containing:
///   - `type`: Either "put" or "delete".
///   - `item`: For put requests, the item object to insert.
///   - `key`: For delete requests, the key object to delete.
/// * `options` - Optional parameters. An object containing:
///   - `get_unprocessed`: If true, disables auto-pagination. Returns an object with `unprocessed_items`.
#[rune::function(instance)]
pub async fn batch_write_item(
    ctx: Ref<Context>,
    requests: Ref<Object>,
    options: Value,
) -> Result<Value, AlternatorError> {
    let client = ctx.get_client()?;

    let mut get_unprocessed = false;

    if let Ok(opts_ref) = options.borrow_ref::<Object>() {
        if let Some(x) = opts_ref
            .get("get_unprocessed")
            .and_then(|v| v.as_bool().ok())
        {
            get_unprocessed = x;
        }
    }

    let request_items: HashMap<String, Vec<WriteRequest>> = requests
        .iter()
        .map(|(table_name, reqs_val)| {
            let reqs_vec = if let Ok(vec) = reqs_val.borrow_ref::<rune::runtime::Vec>() {
                vec
            } else {
                return bad_input("Each table's requests must be a list of write requests");
            };

            let writes = reqs_vec
                .iter()
                .map(|req_val| {
                    let req_ref = if let Ok(obj) = req_val.borrow_ref::<Object>() {
                        obj
                    } else {
                        return bad_input("Each write request must be an object");
                    };

                    let req_type = match req_ref.get("type") {
                        Some(t) if t.borrow_ref::<rune::alloc::String>().is_ok() => t
                            .borrow_ref::<rune::alloc::String>()
                            .unwrap()
                            .as_str()
                            .to_string(),
                        _ => {
                            return bad_input(
                                "Write request must have a 'type' field (put or delete)",
                            )
                        }
                    };

                    match req_type.as_str() {
                        "put" => {
                            let item_obj = match req_ref.get("item") {
                                Some(v) if v.borrow_ref::<Object>().is_ok() => {
                                    v.borrow_ref::<Object>().unwrap()
                                }
                                _ => return bad_input("Put request must have an 'item' field"),
                            };

                            let item_map = rune_object_to_alternator_map(&item_obj)?;

                            Ok(WriteRequest::builder()
                                .put_request(
                                    PutRequest::builder().set_item(Some(item_map)).build()?,
                                )
                                .build())
                        }
                        "delete" => {
                            let key_obj = match req_ref.get("key") {
                                Some(v) if v.borrow_ref::<Object>().is_ok() => {
                                    v.borrow_ref::<Object>().unwrap()
                                }
                                _ => return bad_input("Delete request must have a 'key' field"),
                            };

                            let key_map = rune_object_to_alternator_map(&key_obj)?;

                            Ok(WriteRequest::builder()
                                .delete_request(
                                    DeleteRequest::builder().set_key(Some(key_map)).build()?,
                                )
                                .build())
                        }
                        _ => bad_input(format!(
                            "Invalid request type: {}, must be 'put' or 'delete'",
                            req_type
                        )),
                    }
                })
                .collect::<Result<_, _>>()?;

            Ok((table_name.to_string(), writes))
        })
        .collect::<Result<_, AlternatorError>>()?;

    let builder = client
        .batch_write_item()
        .set_request_items(Some(request_items));

    let (result_items, token) =
        handle_request_with_pagination(&ctx, builder, !get_unprocessed).await?;

    format_batch_result(result_items, token, !get_unprocessed, get_unprocessed)
}

/// Queries items from the table.
///
/// Unlike `get`, which retrieves a single item by its exact primary key,
/// `query` retrieves multiple items that share the same partition key.
/// It also allows specifying conditions on the sort key and filtering the results based on non-key attributes.
///
/// If `with_result` is set to true, the query result is returned as a `Vec<Object>`.
/// Otherwise, the unit value is returned.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `params` - Parameters for the query operation. An object containing:
///   - `query`: The key condition expression string (required).
///   - `filter`: The filter expression string (optional, applied after query).
///   - `attribute_names`: A map of attribute name placeholders (starting with #) to actual names.
///   - `attribute_values`: A map of attribute value placeholders (starting with :) to values.
///   - `consistent_read`: Boolean to enable consistent read (default: false).
///   - `limit`: The maximum number of items to evaluate (optional).
///   - `validation`: An optional item count validation. Look at [extract_validation_args] for details.
///   - `with_result`: If true, the query result is returned (default: false).
#[rune::function(instance)]
pub async fn query(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    params: Ref<Object>,
) -> Result<Value, AlternatorError> {
    let client = ctx.get_client()?;

    let mut builder = client.query().table_name(table_name.deref());

    if let Some(v) = params.get("query") {
        if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
            builder = builder.key_condition_expression(s.as_str().to_string());
        }
    }

    if let Some(v) = params.get("filter") {
        if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
            builder = builder.filter_expression(s.as_str().to_string());
        }
    }

    if let Some(v) = params.get("attribute_names") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder = builder.set_expression_attribute_names(Some(extract_attribute_names(&obj)?));
        }
    }

    if let Some(v) = params.get("attribute_values") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder =
                builder.set_expression_attribute_values(Some(rune_object_to_alternator_map(&obj)?));
        }
    }

    if let Some(b) = params.get("consistent_read").and_then(|v| v.as_bool().ok()) {
        builder = builder.consistent_read(b);
    }

    if let Some(limit_val) = params.get("limit") {
        if let Ok(i) = limit_val.as_signed() {
            builder = builder.limit(match i32::try_from(i) {
                Ok(val) => val,
                Err(_) => return bad_input("limit is out of range"),
            });
        } else {
            return bad_input("limit must be an integer");
        }
    }

    let validation = if let Some(v) = params.get("validation") {
        if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
            Some(
                extract_validation_args(vec.to_vec())
                    .map_err(|s| AlternatorError::new(AlternatorErrorKind::BadInput(s)))?,
            )
        } else {
            None
        }
    } else {
        None
    };

    let result = handle_request_with_validation(&ctx, builder, validation, "Query").await?;

    if params.get("with_result").and_then(|v| v.as_bool().ok()) == Some(true) {
        return Ok(result.to_value()?);
    }

    Ok(Value::from(()))
}

/// Scans items from the table.
///
/// If `with_result` is set to true, the scan result is returned as a `Vec<Object>`.
/// Otherwise, the unit value is returned.
///
/// # Arguments
/// * `table_name` - The name of the table.
/// * `params` - Parameters for the scan operation. An object containing:
///   - `filter`: The filter expression string (optional).
///   - `attribute_names`: A map of attribute name placeholders (starting with #) to actual names.
///   - `attribute_values`: A map of attribute value placeholders (starting with :) to values.
///   - `consistent_read`: Boolean to enable consistent read (default: false).
///   - `limit`: The maximum number of items to evaluate (optional).
///   - `validation`: An optional item count validation. Look at [extract_validation_args] for details.
///   - `with_result`: If true, the scan result is returned (default: false).
#[rune::function(instance)]
pub async fn scan(
    ctx: Ref<Context>,
    table_name: Ref<str>,
    params: Ref<Object>,
) -> Result<Value, AlternatorError> {
    let client = ctx.get_client()?;

    let mut builder = client.scan().table_name(table_name.deref());

    if let Some(v) = params.get("filter") {
        if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
            builder = builder.filter_expression(s.as_str().to_string());
        }
    }

    if let Some(v) = params.get("attribute_names") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder = builder.set_expression_attribute_names(Some(extract_attribute_names(&obj)?));
        }
    }

    if let Some(v) = params.get("attribute_values") {
        if let Ok(obj) = v.borrow_ref::<Object>() {
            builder =
                builder.set_expression_attribute_values(Some(rune_object_to_alternator_map(&obj)?));
        }
    }

    if let Some(b) = params.get("consistent_read").and_then(|v| v.as_bool().ok()) {
        builder = builder.consistent_read(b);
    }

    if let Some(limit_val) = params.get("limit") {
        if let Ok(i) = limit_val.as_signed() {
            builder = builder.limit(i as i32);
        } else {
            return bad_input("limit must be an integer");
        }
    }

    let validation = if let Some(v) = params.get("validation") {
        if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
            Some(
                extract_validation_args(vec.to_vec())
                    .map_err(|s| AlternatorError::new(AlternatorErrorKind::BadInput(s)))?,
            )
        } else {
            None
        }
    } else {
        None
    };

    let result = handle_request_with_validation(&ctx, builder, validation, "Scan").await?;

    if params.get("with_result").and_then(|v| v.as_bool().ok()) == Some(true) {
        return Ok(result.to_value()?);
    }

    Ok(Value::from(()))
}

/// Marks a list of items as an Alternator string set.
#[rune::function]
pub fn string_set(items: Vec<Value>) -> VmResult<Value> {
    let mut obj = Object::new();
    let rune_key = rune::vm_try!(rune::alloc::String::try_from(SSET_KEY));
    let items_val = rune::vm_try!(items.to_value());
    rune::vm_try!(obj.insert(rune_key, items_val));
    VmResult::Ok(rune::vm_try!(Value::new(obj)))
}

/// Marks a list of items as an Alternator number set.
#[rune::function]
pub fn number_set(items: Vec<Value>) -> VmResult<Value> {
    let mut obj = Object::new();
    let rune_key = rune::vm_try!(rune::alloc::String::try_from(NSET_KEY));
    let items_val = rune::vm_try!(items.to_value());
    rune::vm_try!(obj.insert(rune_key, items_val));
    VmResult::Ok(rune::vm_try!(Value::new(obj)))
}

/// Marks a list of items as an Alternator binary set.
#[rune::function]
pub fn binary_set(items: Vec<Value>) -> VmResult<Value> {
    let mut obj = Object::new();
    let rune_key = rune::vm_try!(rune::alloc::String::try_from(BSET_KEY));
    let items_val = rune::vm_try!(items.to_value());
    rune::vm_try!(obj.insert(rune_key, items_val));
    VmResult::Ok(rune::vm_try!(Value::new(obj)))
}
