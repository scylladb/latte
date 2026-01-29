use super::alternator_error::{AlternatorError, AlternatorErrorKind};
use aws_sdk_dynamodb::types::AttributeValue;
use rune::runtime::{Object, Ref};
use rune::Value;
use std::collections::HashMap;

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

        Value::Object(o) => Ok(AttributeValue::M(rune_object_to_alternator_map(
            o.into_ref()?,
        )?)),

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
