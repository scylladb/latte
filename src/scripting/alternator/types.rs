use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use aws_sdk_dynamodb::types::AttributeValue;
use rune::alloc::String as RuneString;
use rune::runtime::{Bytes, Object};
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

    let mut obj = Object::new();
    let rune_key = RuneString::try_from(key)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))?;
    let items_val = items.to_value()?;
    obj.insert(rune_key, items_val)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))?;
    Value::new(obj)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))
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
    if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
        let items = vec
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
    if let Ok(b) = v.as_bool() {
        return Ok(AttributeValue::Bool(b));
    }
    if let Ok(i) = v.as_signed() {
        return Ok(AttributeValue::N(i.to_string()));
    }
    if let Ok(f) = v.as_float() {
        return Ok(AttributeValue::N(format!("{:?}", f)));
    }
    if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
        return Ok(AttributeValue::S(s.as_str().to_string()));
    }
    if let Ok(b) = v.borrow_ref::<Bytes>() {
        return Ok(AttributeValue::B(b.to_vec().into()));
    }
    if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
        let list = vec
            .iter()
            .map(|v| rune_value_to_alternator_attribute(v.clone()))
            .collect::<Result<_, _>>()?;
        return Ok(AttributeValue::L(list));
    }
    if let Ok(obj) = v.borrow_ref::<Object>() {
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

        return Ok(AttributeValue::M(rune_object_to_alternator_map(&obj)?));
    }
    if let Ok(opt) = v.borrow_ref::<Option<Value>>() {
        return match opt.as_ref() {
            Some(inner) => rune_value_to_alternator_attribute(inner.clone()),
            None => Ok(AttributeValue::Null(true)),
        };
    }
    Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
        format!("Unsupported Rune Value type for: {:?}", v),
    )))
}

pub fn alternator_attribute_to_rune_value(attr: AttributeValue) -> Result<Value, AlternatorError> {
    match attr {
        AttributeValue::Bool(b) => Ok(Value::from(b)),

        AttributeValue::N(n) => {
            // Try parsing as integer first, then as float
            if let Ok(i) = n.parse::<i64>() {
                Ok(Value::from(i))
            } else if let Ok(f) = n.parse::<f64>() {
                Ok(Value::from(f))
            } else {
                Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
                    format!("Invalid number format: {}", n),
                )))
            }
        }

        AttributeValue::S(s) => Ok(Value::new(RuneString::try_from(s).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))?),

        AttributeValue::B(b) => Ok(Bytes::try_from(b.into_inner())?.to_value()?),

        AttributeValue::L(l) => {
            let mut rune_vec = rune::runtime::Vec::new();
            for attr in l {
                let val = alternator_attribute_to_rune_value(attr)?;
                rune_vec.push(val).map_err(|e| {
                    AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
                })?;
            }
            Ok(Value::vec(rune_vec.into_inner()).map_err(|e| {
                AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
            })?)
        }

        AttributeValue::M(map) => Ok(alternator_map_to_rune_object(map)?),

        AttributeValue::Null(_) => Ok(Value::try_from(None::<Value>).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?),

        AttributeValue::Ss(ss) => alternator_set_to_rune(SSET_KEY, ss, AttributeValue::S),
        AttributeValue::Ns(ns) => alternator_set_to_rune(NSET_KEY, ns, AttributeValue::N),
        AttributeValue::Bs(bs) => alternator_set_to_rune(BSET_KEY, bs, AttributeValue::B),

        _ => Err(AlternatorError::new(AlternatorErrorKind::ConversionError(
            format!("Unsupported Alternator AttributeValue type: {:?}", attr),
        ))),
    }
}

pub fn rune_object_to_alternator_map(
    obj: &Object,
) -> Result<HashMap<String, AttributeValue>, AlternatorError> {
    obj.iter()
        .map(|(k, v)| {
            Ok((
                k.to_string(),
                rune_value_to_alternator_attribute(v.clone())?,
            ))
        })
        .collect()
}

pub fn alternator_map_to_rune_object(
    map: HashMap<String, AttributeValue>,
) -> Result<Value, AlternatorError> {
    let mut obj = Object::new();
    for (k, v) in map {
        let rune_key = RuneString::try_from(k).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?;
        let rune_val = alternator_attribute_to_rune_value(v)?;
        obj.insert(rune_key, rune_val).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?;
    }
    Value::new(obj)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))
}

/// Converts a HashMap<String, Value> to a proper rune Object Value.
/// Unlike HashMap.to_value() which wraps as an opaque type in rune 0.14,
/// this creates a proper Object that can be pattern-matched.
pub fn hashmap_to_rune_object(map: HashMap<String, Value>) -> Result<Value, AlternatorError> {
    let mut obj = Object::new();
    for (k, v) in map {
        let rune_key = RuneString::try_from(k).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?;
        obj.insert(rune_key, v).map_err(|e| {
            AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string()))
        })?;
    }
    Value::new(obj)
        .map_err(|e| AlternatorError::new(AlternatorErrorKind::ConversionError(e.to_string())))
}
