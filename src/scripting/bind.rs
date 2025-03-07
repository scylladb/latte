//! Functions for binding rune values to CQL parameters

use crate::scripting::cass_error::{CassError, CassErrorKind};
use crate::scripting::cql_types::Uuid;
use rune::{Any, ToValue, Value};
use scylla::_macro_internal::ColumnType;
use scylla::frame::response::result::{CollectionType, ColumnSpec, NativeType};
use scylla::response::query_result::ColumnSpecs;
use scylla::value::{CqlTimeuuid, CqlValue};
use std::net::IpAddr;
use std::str::FromStr;

use itertools::*;

fn to_scylla_value(v: &Value, typ: &ColumnType) -> Result<CqlValue, CassError> {
    // TODO: add support for the following native CQL types:
    //       'counter', 'date', 'duration', 'time' and 'variant'.
    //       Also, for the 'tuple'.
    match (v, typ) {
        (Value::Bool(v), ColumnType::Native(NativeType::Boolean)) => Ok(CqlValue::Boolean(*v)),

        (Value::Byte(v), ColumnType::Native(NativeType::TinyInt)) => {
            Ok(CqlValue::TinyInt(*v as i8))
        }
        (Value::Byte(v), ColumnType::Native(NativeType::SmallInt)) => {
            Ok(CqlValue::SmallInt(*v as i16))
        }
        (Value::Byte(v), ColumnType::Native(NativeType::Int)) => Ok(CqlValue::Int(*v as i32)),
        (Value::Byte(v), ColumnType::Native(NativeType::BigInt)) => Ok(CqlValue::BigInt(*v as i64)),

        (Value::Integer(v), ColumnType::Native(NativeType::TinyInt)) => {
            convert_int(*v, NativeType::TinyInt, CqlValue::TinyInt)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::SmallInt)) => {
            convert_int(*v, NativeType::SmallInt, CqlValue::SmallInt)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Int)) => {
            convert_int(*v, NativeType::Int, CqlValue::Int)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::BigInt)) => Ok(CqlValue::BigInt(*v)),
        (Value::Integer(v), ColumnType::Native(NativeType::Timestamp)) => {
            Ok(CqlValue::Timestamp(scylla::value::CqlTimestamp(*v)))
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Decimal)) => Ok(CqlValue::Decimal(
            scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                (*v).to_be_bytes().to_vec(),
                0,
            ),
        )),

        (Value::Float(v), ColumnType::Native(NativeType::Float)) => Ok(CqlValue::Float(*v as f32)),
        (Value::Float(v), ColumnType::Native(NativeType::Double)) => Ok(CqlValue::Double(*v)),
        (Value::Float(v), ColumnType::Native(NativeType::Decimal)) => {
            let decimal = rust_decimal::Decimal::from_f64_retain(*v).unwrap();
            Ok(CqlValue::Decimal(
                scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                    decimal.mantissa().to_be_bytes().to_vec(),
                    decimal.scale().try_into().unwrap(),
                ),
            ))
        }

        (Value::String(s), ColumnType::Native(NativeType::Timeuuid)) => {
            let timeuuid_str = s.borrow_ref().unwrap();
            let timeuuid = CqlTimeuuid::from_str(timeuuid_str.as_str());
            match timeuuid {
                Ok(timeuuid) => Ok(CqlValue::Timeuuid(timeuuid)),
                Err(e) => Err(CassError(CassErrorKind::QueryParamConversion(
                    format!("{:?}", v),
                    "NativeType::Timeuuid".to_string(),
                    Some(format!("{}", e)),
                ))),
            }
        }
        (
            Value::String(v),
            ColumnType::Native(NativeType::Text) | ColumnType::Native(NativeType::Ascii),
        ) => Ok(CqlValue::Text(v.borrow_ref().unwrap().as_str().to_string())),
        (Value::String(s), ColumnType::Native(NativeType::Inet)) => {
            let ipaddr_str = s.borrow_ref().unwrap();
            let ipaddr = IpAddr::from_str(ipaddr_str.as_str());
            match ipaddr {
                Ok(ipaddr) => Ok(CqlValue::Inet(ipaddr)),
                Err(e) => Err(CassError(CassErrorKind::QueryParamConversion(
                    format!("{:?}", v),
                    "NativeType::Inet".to_string(),
                    Some(format!("{}", e)),
                ))),
            }
        }
        (Value::String(s), ColumnType::Native(NativeType::Decimal)) => {
            let dec_str = s.borrow_ref().unwrap();
            let decimal = rust_decimal::Decimal::from_str_exact(&dec_str).unwrap();
            Ok(CqlValue::Decimal(
                scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                    decimal.mantissa().to_be_bytes().to_vec(),
                    decimal.scale().try_into().unwrap(),
                ),
            ))
        }
        (Value::Bytes(v), ColumnType::Native(NativeType::Blob)) => {
            Ok(CqlValue::Blob(v.borrow_ref().unwrap().to_vec()))
        }
        (Value::Vec(v), ColumnType::Native(NativeType::Blob)) => {
            let v: Vec<Value> = v.borrow_ref().unwrap().to_vec();
            let byte_vec: Vec<u8> = v
                .into_iter()
                .map(|value| value.as_byte().unwrap())
                .collect();
            Ok(CqlValue::Blob(byte_vec))
        }
        (Value::Option(v), typ) => match v.borrow_ref().unwrap().as_ref() {
            Some(v) => to_scylla_value(v, typ),
            None => Ok(CqlValue::Empty),
        },
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: false,
                typ: CollectionType::List(elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let elements = v
                .as_ref()
                .iter()
                .map(|v| to_scylla_value(v, elt))
                .try_collect()?;
            Ok(CqlValue::List(elements))
        }
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: false,
                typ: CollectionType::Set(elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let elements = v
                .as_ref()
                .iter()
                .map(|v| to_scylla_value(v, elt))
                .try_collect()?;
            Ok(CqlValue::Set(elements))
        }
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: false,
                typ: CollectionType::Map(key_elt, value_elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let mut map_vec = Vec::with_capacity(v.len());
            for tuple in v.iter() {
                match tuple {
                    Value::Tuple(tuple) if tuple.borrow_ref().unwrap().len() == 2 => {
                        let tuple = tuple.borrow_ref().unwrap();
                        let key = to_scylla_value(tuple.first().unwrap(), key_elt)?;
                        let value = to_scylla_value(tuple.get(1).unwrap(), value_elt)?;
                        map_vec.push((key, value));
                    }
                    _ => {
                        return Err(CassError(CassErrorKind::QueryParamConversion(
                            format!("{:?}", tuple),
                            "CollectionType::Map".to_string(),
                            None,
                        )));
                    }
                }
            }
            Ok(CqlValue::Map(map_vec))
        }
        (
            Value::Object(obj),
            ColumnType::Collection {
                frozen: false,
                typ: CollectionType::Map(key_elt, value_elt),
            },
        ) => {
            let obj = obj.borrow_ref().unwrap();
            let mut map_vec = Vec::with_capacity(obj.keys().len());
            for (k, v) in obj.iter() {
                let key = String::from(k.as_str());
                let key = to_scylla_value(&(key.to_value().unwrap()), key_elt)?;
                let value = to_scylla_value(v, value_elt)?;
                map_vec.push((key, value));
            }
            Ok(CqlValue::Map(map_vec))
        }
        (
            Value::Object(v),
            ColumnType::UserDefinedType {
                frozen: false,
                definition,
            },
        ) => {
            let obj = v.borrow_ref().unwrap();
            let field_types: Vec<(String, ColumnType)> = definition
                .field_types
                .iter()
                .map(|(name, typ)| (name.to_string(), typ.clone()))
                .collect();
            let fields = read_fields(|s| obj.get(s), &field_types)?;
            Ok(CqlValue::UserDefinedType {
                name: definition.name.to_string(),
                keyspace: definition.keyspace.to_string(),
                fields,
            })
        }
        (
            Value::Struct(v),
            ColumnType::UserDefinedType {
                frozen: false,
                definition,
            },
        ) => {
            let obj = v.borrow_ref().unwrap();
            let field_types: Vec<(String, ColumnType)> = definition
                .field_types
                .iter()
                .map(|(name, typ)| (name.to_string(), typ.clone()))
                .collect();
            let fields = read_fields(|s| obj.get(s), &field_types)?;
            Ok(CqlValue::UserDefinedType {
                name: definition.name.to_string(),
                keyspace: definition.keyspace.to_string(),
                fields,
            })
        }

        (Value::Any(obj), ColumnType::Native(NativeType::Uuid)) => {
            let obj = obj.borrow_ref().unwrap();
            let h = obj.type_hash();
            if h == Uuid::type_hash() {
                let uuid: &Uuid = obj.downcast_borrow_ref().unwrap();
                Ok(CqlValue::Uuid(uuid.0))
            } else {
                Err(CassError(CassErrorKind::QueryParamConversion(
                    format!("{:?}", v),
                    "NativeType::Uuid".to_string(),
                    None,
                )))
            }
        }
        (value, typ) => Err(CassError(CassErrorKind::QueryParamConversion(
            format!("{:?}", value),
            format!("{:?}", typ).to_string(),
            None,
        ))),
    }
}

fn convert_int<T: TryFrom<i64>, R>(
    value: i64,
    typ: NativeType,
    f: impl Fn(T) -> R,
) -> Result<R, CassError> {
    let converted = value.try_into().map_err(|_| {
        CassError(CassErrorKind::ValueOutOfRange(
            value.to_string(),
            format!("{:?}", typ).to_string(),
        ))
    })?;
    Ok(f(converted))
}

/// Binds parameters passed as a single rune value to the arguments of the statement.
/// The `params` value can be a tuple, a vector, a struct or an object.
pub fn to_scylla_query_params(
    params: &Value,
    types: ColumnSpecs,
) -> Result<Vec<CqlValue>, CassError> {
    Ok(match params {
        Value::Tuple(tuple) => {
            let mut values = Vec::new();
            let tuple = tuple.borrow_ref().unwrap();
            if tuple.len() != types.len() {
                return Err(CassError(CassErrorKind::InvalidNumberOfQueryParams));
            }
            for (v, t) in tuple.iter().zip(types.as_slice()) {
                values.push(to_scylla_value(v, t.typ())?);
            }
            values
        }
        Value::Vec(vec) => {
            let mut values = Vec::new();

            let vec = vec.borrow_ref().unwrap();
            for (v, t) in vec.iter().zip(types.as_slice()) {
                values.push(to_scylla_value(v, t.typ())?);
            }
            values
        }
        Value::Object(obj) => {
            let obj = obj.borrow_ref().unwrap();
            read_params(|f| obj.get(f), types.as_slice())?
        }
        Value::Struct(obj) => {
            let obj = obj.borrow_ref().unwrap();
            read_params(|f| obj.get(f), types.as_slice())?
        }
        other => {
            return Err(CassError(CassErrorKind::InvalidQueryParamsObject(
                other.type_info().unwrap(),
            )));
        }
    })
}

fn read_params<'a, 'b>(
    get_value: impl Fn(&str) -> Option<&'a Value>,
    params: &[ColumnSpec],
) -> Result<Vec<CqlValue>, CassError> {
    let mut values = Vec::with_capacity(params.len());
    for column in params {
        let value = match get_value(column.name()) {
            Some(value) => to_scylla_value(value, column.typ())?,
            None => CqlValue::Empty,
        };
        values.push(value)
    }
    Ok(values)
}

fn read_fields<'a, 'b>(
    get_value: impl Fn(&str) -> Option<&'a Value>,
    fields: &[(String, ColumnType)],
) -> Result<Vec<(String, Option<CqlValue>)>, CassError> {
    let mut values = Vec::with_capacity(fields.len());
    for (field_name, field_type) in fields {
        if let Some(value) = get_value(field_name) {
            let value = Some(to_scylla_value(value, field_type)?);
            values.push((field_name.to_string(), value))
        };
    }
    Ok(values)
}
