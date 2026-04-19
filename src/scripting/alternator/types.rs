use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use aws_sdk_dynamodb::types::AttributeValue;
use rune::runtime::{Bytes, Object, Ref};
use rune::{ToValue, Value};
use std::collections::HashMap;

pub const SSET_KEY: &str = "__sset";
pub const NSET_KEY: &str = "__nset";
pub const BSET_KEY: &str = "__bset";

fn alternator_set_to_rune<I, T, F>(key: &str, iter: I, wrapper: F) -> Result<Value, AlternatorError>
where
    I: IntoIterator<Item = T>,
    F: Fn(T) -> AttributeValue,
{
    let items = iter
        .into_iter()
        .map(|item| alternator_attribute_to_rune_value(wrapper(item)))
        .collect::<Result<Vec<Value>, AlternatorError>>()?;

    let mut map = HashMap::new();
    map.insert(key.to_string(), items.to_value().into_result()?);
    Ok(map.to_value().into_result()?)
}

fn rune_set_to_alternator<T, W, U>(
    v: Value,
    key: &str,
    attribute_constructor: W,
    rune_unwrapper: U,
) -> Result<AttributeValue, AlternatorError>
where
    W: Fn(Vec<T>) -> AttributeValue,
    U: Fn(AttributeValue) -> Option<T>,
{
    if let Value::Vec(vec) = v {
        let items = vec
            .into_ref()?
            .iter()
            .map(|item| {
                rune_unwrapper(rune_value_to_alternator_attribute(item.clone())?).ok_or_else(|| {
                    AlternatorError::new(AlternatorErrorKind::ConversionError(format!(
                        "Invalid element type found in set {}: {:?}",
                        key, item
                    )))
                })
            })
            .collect::<Result<Vec<T>, _>>()?;
        Ok(attribute_constructor(items))
    } else {
        Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
            format!("Expected a vector of elements for {}", key),
        )))
    }
}

pub fn rune_value_to_alternator_attribute(v: Value) -> Result<AttributeValue, AlternatorError> {
    match v {
        Value::Bool(b) => Ok(AttributeValue::Bool(b)),

        // DynamoDB represents all numbers as strings
        Value::Integer(i) => Ok(AttributeValue::N(i.to_string())),
        // To distinguish floats from integers, we print them with the decimal point
        Value::Float(f) => Ok(AttributeValue::N(format!("{:?}", f))),

        Value::String(s) => Ok(AttributeValue::S(s.into_ref()?.to_string())),

        Value::Bytes(b) => Ok(AttributeValue::B(b.into_ref()?.to_vec().into())),

        Value::Vec(v) => Ok(AttributeValue::L(
            v.into_ref()?
                .iter()
                .map(|v| rune_value_to_alternator_attribute(v.clone()))
                .collect::<Result<_, _>>()?,
        )),

        Value::Object(o) => {
            let obj = o.into_ref()?;

            // Check for special Set representations.
            // They have to be objects with exactly one key with special name, and the value has to be a vector of appropriate types.
            if obj.len() == 1 {
                let mut iter = obj.iter();
                let (k, val) = iter.next().unwrap();

                match k.as_str() {
                    SSET_KEY => {
                        return rune_set_to_alternator(val.clone(), k, AttributeValue::Ss, |a| {
                            if let AttributeValue::S(s) = a {
                                Some(s)
                            } else {
                                None
                            }
                        });
                    }
                    NSET_KEY => {
                        return rune_set_to_alternator(val.clone(), k, AttributeValue::Ns, |a| {
                            if let AttributeValue::N(n) = a {
                                Some(n)
                            } else {
                                None
                            }
                        });
                    }
                    BSET_KEY => {
                        return rune_set_to_alternator(val.clone(), k, AttributeValue::Bs, |a| {
                            if let AttributeValue::B(b) = a {
                                Some(b)
                            } else {
                                None
                            }
                        });
                    }
                    // Does not match any of the special set keys, so we treat it as a regular object.
                    _ => {}
                }
            }

            Ok(AttributeValue::M(rune_object_to_alternator_map(obj)?))
        }

        Value::Option(o) => match o.into_ref()?.as_ref() {
            Some(v) => rune_value_to_alternator_attribute(v.clone()),
            None => Ok(AttributeValue::Null(true)),
        },

        _ => Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
            format!("Unsupported Rune Value type for: {:?}", v),
        ))),
    }
}

pub fn rune_object_to_alternator_map(
    o: Ref<Object>,
) -> Result<HashMap<String, AttributeValue>, AlternatorError> {
    o.iter()
        .map(|(k, v)| {
            Ok((
                k.to_string(),
                rune_value_to_alternator_attribute(v.clone())?,
            ))
        })
        .collect()
}

pub fn alternator_attribute_to_rune_value(attr: AttributeValue) -> Result<Value, AlternatorError> {
    match attr {
        AttributeValue::Bool(b) => Ok(Value::Bool(b)),

        AttributeValue::N(n) => {
            // Try parsing as integer first, then as float
            if let Ok(i) = n.parse::<i64>() {
                Ok(Value::Integer(i))
            } else if let Ok(f) = n.parse::<f64>() {
                Ok(Value::Float(f))
            } else {
                Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
                    format!("Invalid number format: {}", n),
                )))
            }
        }

        AttributeValue::S(s) => Ok(s.as_str().to_value().into_result()?),

        AttributeValue::B(b) => Ok(Bytes::try_from(b.into_inner())?.to_value().into_result()?),

        AttributeValue::L(l) => Ok(l
            .into_iter()
            .map(alternator_attribute_to_rune_value)
            .collect::<Result<Vec<Value>, _>>()?
            .to_value()
            .into_result()?),

        AttributeValue::M(map) => Ok(alternator_map_to_rune_object(map)?),

        AttributeValue::Null(_) => Ok(None::<bool>.to_value().into_result()?),

        AttributeValue::Ss(ss) => alternator_set_to_rune(SSET_KEY, ss, AttributeValue::S),
        AttributeValue::Ns(ns) => alternator_set_to_rune(NSET_KEY, ns, AttributeValue::N),
        AttributeValue::Bs(bs) => alternator_set_to_rune(BSET_KEY, bs, AttributeValue::B),

        _ => Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
            format!("Unsupported Alternator AttributeValue type: {:?}", attr),
        ))),
    }
}

pub fn alternator_map_to_rune_object(
    map: HashMap<String, AttributeValue>,
) -> Result<Value, AlternatorError> {
    Ok(map
        .into_iter()
        .map(|(k, v)| Ok((k, alternator_attribute_to_rune_value(v)?)))
        .collect::<Result<HashMap<String, Value>, AlternatorError>>()?
        .to_value()
        .into_result()?)
}
