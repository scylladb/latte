use crate::scripting::functions_common::extract_validation_args;

use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use super::context::Context;
use super::types::{alternator_map_to_rune_object, rune_object_to_alternator_map};
use aws_sdk_dynamodb::client::Waiters;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, KeySchemaElement, KeyType, ScalarAttributeType,
};
use rune::runtime::{Object, Ref, Shared};
use rune::{ToValue, Value};
use std::collections::HashMap;
use std::ops::Deref;

fn bad_input<T>(msg: impl Into<String>) -> Result<T, AlternatorError> {
    Err(AlternatorError::new(AlternatorErrorKind::BadInput(
        msg.into(),
    )))
}

/// Gets the name and type of a primary or sort key from a object.
fn extract_key_definition(
    object: &Shared<Object>,
) -> Result<(String, ScalarAttributeType), AlternatorError> {
    let key_name = if let Some(Value::String(s)) = object.borrow_ref()?.get("name") {
        s.borrow_ref()?.to_string()
    } else {
        return bad_input("Key definition object must have a 'name' field");
    };

    let key_type = if let Some(Value::String(t)) = object.borrow_ref()?.get("type") {
        match t.borrow_ref()?.as_str() {
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
    };

    Ok((key_name, key_type))
}

fn extract_attribute_names(
    object: &Shared<Object>,
) -> Result<HashMap<String, String>, AlternatorError> {
    object
        .borrow_ref()?
        .iter()
        .map(|(k, v)| {
            Ok((
                k.to_string(),
                match v {
                    Value::String(s) => s.borrow_ref()?.to_string(),
                    _ => return bad_input("Attribute names must be strings"),
                },
            ))
        })
        .collect::<Result<_, _>>()
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
    let (pk_name, pk_type) = match &params {
        Value::String(s) => (s.borrow_ref()?.to_string(), ScalarAttributeType::S),
        Value::Object(o) => match o.borrow_ref()?.get("primary_key") {
            Some(Value::String(s)) => (s.borrow_ref()?.to_string(), ScalarAttributeType::S),
            Some(Value::Object(pk_obj)) => extract_key_definition(pk_obj)?,
            _ => return bad_input("Invalid 'primary_key' object in params"),
        },
        _ => return bad_input("Params must be a string or an object"),
    };

    // Extract sort key definition if present
    let sk = match &params {
        Value::Object(o) => match o.borrow_ref()?.get("sort_key") {
            Some(Value::String(s)) => Some((s.borrow_ref()?.to_string(), ScalarAttributeType::S)),
            Some(Value::Object(sk_obj)) => Some(extract_key_definition(sk_obj)?),
            Some(_) => return bad_input("Invalid 'sort_key' object in params"),
            None => None,
        },
        _ => None,
    };

    let mut builder = client.create_table().table_name(table_name.deref());

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

    builder
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
        .send()
        .await?;

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

    client
        .put_item()
        .table_name(table_name.deref())
        .set_item(Some(rune_object_to_alternator_map(item)?))
        .send()
        .await?;

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

    client
        .delete_item()
        .table_name(table_name.deref())
        .set_key(Some(rune_object_to_alternator_map(key)?))
        .send()
        .await?;

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
        .set_key(Some(rune_object_to_alternator_map(key)?));

    if let Value::Object(opts) = &options {
        if let Some(Value::Bool(consistent_read)) = opts.borrow_ref()?.get("consistent_read") {
            builder = builder.consistent_read(*consistent_read);
        }
    }

    let output = builder.send().await?;

    if let Value::Object(opts) = &options {
        if let Some(Value::Bool(with_result)) = opts.borrow_ref()?.get("with_result") {
            if *with_result {
                return Ok(output
                    .item
                    .map(alternator_map_to_rune_object)
                    .transpose()?
                    .to_value()
                    .into_result()?);
            }
        }
    }

    Ok(Value::EmptyTuple)
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
        .set_key(Some(rune_object_to_alternator_map(key)?));

    if let Some(Value::String(update_expression)) = params.get("update") {
        builder = builder.update_expression(update_expression.borrow_ref()?.to_string());
    }

    if let Some(Value::Object(attr_names)) = params.get("attribute_names") {
        builder =
            builder.set_expression_attribute_names(Some(extract_attribute_names(attr_names)?));
    }

    if let Some(Value::Object(attr_values)) = params.get("attribute_values") {
        builder = builder.set_expression_attribute_values(Some(rune_object_to_alternator_map(
            attr_values.clone().into_ref()?,
        )?));
    }

    builder.send().await?;

    Ok(())
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

    if let Some(Value::String(key_condition_expression)) = params.get("query") {
        builder =
            builder.key_condition_expression(key_condition_expression.borrow_ref()?.to_string());
    }

    if let Some(Value::String(filter_expression)) = params.get("filter") {
        builder = builder.filter_expression(filter_expression.borrow_ref()?.to_string());
    }

    if let Some(Value::Object(attr_names)) = params.get("attribute_names") {
        builder =
            builder.set_expression_attribute_names(Some(extract_attribute_names(attr_names)?));
    }

    if let Some(Value::Object(attr_values)) = params.get("attribute_values") {
        builder = builder.set_expression_attribute_values(Some(rune_object_to_alternator_map(
            attr_values.clone().into_ref()?,
        )?));
    }

    if let Some(Value::Bool(consistent_read)) = params.get("consistent_read") {
        builder = builder.consistent_read(*consistent_read);
    }

    if let Some(limit_val) = params.get("limit") {
        builder = builder.limit(match limit_val {
            Value::Integer(i) => match i32::try_from(*i) {
                Ok(val) => val,
                Err(_) => return bad_input("limit is out of range"),
            },
            _ => return bad_input("limit must be an integer"),
        });
    }

    let validation = if let Some(Value::Vec(validation)) = params.get("validation") {
        Some(
            extract_validation_args(validation.borrow_ref()?.to_vec())
                .map_err(|s| AlternatorError::new(AlternatorErrorKind::BadInput(s)))?,
        )
    } else {
        None
    };

    let output = builder.send().await?;

    if let Some(validation) = validation {
        let item_count = output.items().len() as u64;

        if item_count < validation.expected_min || item_count > validation.expected_max {
            return Err(AlternatorError::new(AlternatorErrorKind::ValidationError(
                format!(
                    "Query returned {item_count} items, expected between {} and {} {}",
                    validation.expected_min, validation.expected_max, validation.custom_err_msg
                ),
            )));
        }
    }

    if let Some(Value::Bool(with_result)) = params.get("with_result") {
        if *with_result {
            return Ok(output
                .items
                .unwrap_or_default()
                .into_iter()
                .map(alternator_map_to_rune_object)
                .collect::<Result<Vec<_>, _>>()?
                .to_value()
                .into_result()?);
        }
    }

    Ok(Value::EmptyTuple)
}
