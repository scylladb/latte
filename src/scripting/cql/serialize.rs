//! Functions for binding rune values to CQL parameters

use super::cass_error::{CassError, CassErrorKind};
use crate::scripting::rune_uuid::Uuid;
use chrono::{NaiveDate, NaiveTime};
use once_cell::sync::Lazy;
use regex::Regex;
use rune::{Any, ToValue, Value};
use scylla::_macro_internal::ColumnType;
use scylla::frame::response::result::{CollectionType, ColumnSpec, NativeType};
use scylla::serialize::row::{RowSerializationContext, SerializeRow};
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::RowWriter;
use scylla::serialize::SerializationError;
use scylla::value::{CqlDate, CqlDuration, CqlTime, CqlTimeuuid, CqlValue, CqlVarint};
use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use itertools::*;

/// RuneQueryParams is a wrapper-type for the optional rune `Value` that implements `SerializeRow`,
/// allowing direct serialization without constructing an intermediate `Vec<Option<CqlValue>>`.
pub struct RuneQueryParams<'a> {
    value: Option<&'a Value>,
}

impl<'a> RuneQueryParams<'a> {
    pub fn new(value: Option<&'a Value>) -> Self {
        Self { value }
    }
}

impl SerializeRow for RuneQueryParams<'_> {
    fn serialize(
        &self,
        ctx: &RowSerializationContext<'_>,
        writer: &mut RowWriter<'_>,
    ) -> Result<(), SerializationError> {
        let columns = ctx.columns();
        match self.value {
            None => {
                // No params provided — verify nothing is expected
                if !columns.is_empty() {
                    return Err(SerializationError::new(CassError(
                        CassErrorKind::InvalidNumberOfQueryParams,
                    )));
                }
                Ok(())
            }
            Some(value) => serialize_rune_params(value, columns, writer),
        }
    }

    fn is_empty(&self) -> bool {
        self.value.is_none()
    }
}

fn serialize_rune_params(
    value: &Value,
    columns: &[ColumnSpec<'_>],
    writer: &mut RowWriter<'_>,
) -> Result<(), SerializationError> {
    match value {
        Value::Tuple(tuple) => {
            let tuple = tuple.borrow_ref().map_err(|e| {
                SerializationError::new(CassError(CassErrorKind::Error(e.to_string())))
            })?;
            if tuple.len() != columns.len() {
                return Err(SerializationError::new(CassError(
                    CassErrorKind::InvalidNumberOfQueryParams,
                )));
            }
            for (v, col) in tuple.iter().zip(columns) {
                serialize_rune_cell(v, col.typ(), writer)?;
            }
            Ok(())
        }
        Value::Vec(vec) => {
            let vec = vec.borrow_ref().map_err(|e| {
                SerializationError::new(CassError(CassErrorKind::Error(e.to_string())))
            })?;
            for (v, col) in vec.iter().zip(columns) {
                serialize_rune_cell(v, col.typ(), writer)?;
            }
            Ok(())
        }
        Value::Object(obj) => {
            let obj = obj.borrow_ref().map_err(|e| {
                SerializationError::new(CassError(CassErrorKind::Error(e.to_string())))
            })?;
            for col in columns {
                let cql_val = match obj.get(col.name()) {
                    Some(v) => {
                        to_scylla_value(v, col.typ()).map_err(|e| SerializationError::new(*e))?
                    }
                    None => Some(CqlValue::Empty),
                };
                cql_val
                    .serialize(col.typ(), writer.make_cell_writer())
                    .map_err(SerializationError::new)?;
            }
            Ok(())
        }
        Value::Struct(obj) => {
            let obj = obj.borrow_ref().map_err(|e| {
                SerializationError::new(CassError(CassErrorKind::Error(e.to_string())))
            })?;
            for col in columns {
                let cql_val = match obj.get(col.name()) {
                    Some(v) => {
                        to_scylla_value(v, col.typ()).map_err(|e| SerializationError::new(*e))?
                    }
                    None => Some(CqlValue::Empty),
                };
                cql_val
                    .serialize(col.typ(), writer.make_cell_writer())
                    .map_err(SerializationError::new)?;
            }
            Ok(())
        }
        other => Err(SerializationError::new(CassError(
            CassErrorKind::InvalidQueryParamsObject(other.type_info().unwrap()),
        ))),
    }
}

/// Serializes a single rune value as a CQL cell.
fn serialize_rune_cell(
    v: &Value,
    typ: &ColumnType,
    writer: &mut RowWriter<'_>,
) -> Result<(), SerializationError> {
    let cql_val = to_scylla_value(v, typ).map_err(|e| SerializationError::new(*e))?;
    cql_val
        .serialize(typ, writer.make_cell_writer())
        .map_err(SerializationError::new)?;
    Ok(())
}

static DURATION_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?P<years>\d+)y|",
        r"(?P<months>\d+)mo|",
        r"(?P<weeks>\d+)w|",
        r"(?P<days>\d+)d|",
        r"(?P<hours>\d+)h|",
        r"(?P<seconds>\d+)s|",
        r"(?P<millis>\d+)ms|",
        r"(?P<micros>\d+)us|",
        r"(?P<nanoseconds>\d+)ns|",
        r"(?P<minutes>\d+)m|", // must be after 'mo' and 'ms' matchers
        r"(?P<invalid>.+)",    // must be last, used for all incorrect matches
    ))
    .unwrap()
});

fn to_scylla_value(v: &Value, typ: &ColumnType) -> Result<Option<CqlValue>, Box<CassError>> {
    match (v, typ) {
        (Value::Bool(v), ColumnType::Native(NativeType::Boolean)) => {
            Ok(Some(CqlValue::Boolean(*v)))
        }

        (Value::Byte(v), ColumnType::Native(NativeType::TinyInt)) => {
            Ok(Some(CqlValue::TinyInt(*v as i8)))
        }
        (Value::Byte(v), ColumnType::Native(NativeType::SmallInt)) => {
            Ok(Some(CqlValue::SmallInt(*v as i16)))
        }
        (Value::Byte(v), ColumnType::Native(NativeType::Int)) => Ok(Some(CqlValue::Int(*v as i32))),
        (Value::Byte(v), ColumnType::Native(NativeType::BigInt)) => {
            Ok(Some(CqlValue::BigInt(*v as i64)))
        }

        (Value::Integer(v), ColumnType::Native(NativeType::TinyInt)) => {
            convert_int(*v, NativeType::TinyInt, CqlValue::TinyInt)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::SmallInt)) => {
            convert_int(*v, NativeType::SmallInt, CqlValue::SmallInt)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Int)) => {
            convert_int(*v, NativeType::Int, CqlValue::Int)
        }
        (Value::Integer(v), ColumnType::Native(NativeType::BigInt)) => {
            Ok(Some(CqlValue::BigInt(*v)))
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Counter)) => {
            Ok(Some(CqlValue::Counter(scylla::value::Counter(*v))))
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Timestamp)) => {
            Ok(Some(CqlValue::Timestamp(scylla::value::CqlTimestamp(*v))))
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Date)) => match (*v).try_into() {
            Ok(date) => Ok(Some(CqlValue::Date(CqlDate(date)))),
            Err(_) => Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                format!("{v:?}"),
                "NativeType::Date".to_string(),
                Some("Invalid date value".to_string()),
            )))),
        },
        (Value::Integer(v), ColumnType::Native(NativeType::Time)) => {
            Ok(Some(CqlValue::Time(CqlTime(*v))))
        }
        (Value::Integer(v), ColumnType::Native(NativeType::Varint)) => Ok(Some(CqlValue::Varint(
            CqlVarint::from_signed_bytes_be((*v).to_be_bytes().to_vec()),
        ))),
        (Value::Integer(v), ColumnType::Native(NativeType::Decimal)) => {
            Ok(Some(CqlValue::Decimal(
                scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                    (*v).to_be_bytes().to_vec(),
                    0,
                ),
            )))
        }

        (Value::Float(v), ColumnType::Native(NativeType::Float)) => {
            Ok(Some(CqlValue::Float(*v as f32)))
        }
        (Value::Float(v), ColumnType::Native(NativeType::Double)) => Ok(Some(CqlValue::Double(*v))),
        (Value::Float(v), ColumnType::Native(NativeType::Decimal)) => {
            let decimal = rust_decimal::Decimal::from_f64_retain(*v).unwrap();
            Ok(Some(CqlValue::Decimal(
                scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                    decimal.mantissa().to_be_bytes().to_vec(),
                    decimal.scale().try_into().unwrap(),
                ),
            )))
        }

        (Value::String(s), ColumnType::Native(NativeType::Date)) => {
            let date_str = s.borrow_ref().unwrap();
            let naive_date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").map_err(|e| {
                CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Date".to_string(),
                    Some(format!("{e}")),
                ))
            })?;
            let cql_date = CqlDate::from(naive_date);
            Ok(Some(CqlValue::Date(cql_date)))
        }
        (Value::String(s), ColumnType::Native(NativeType::Time)) => {
            let time_str = s.borrow_ref().unwrap();
            let mut time_format = "%H:%M:%S".to_string();
            if time_str.contains('.') {
                time_format = format!("{time_format}.%f");
            }
            let naive_time = NaiveTime::parse_from_str(&time_str, &time_format).map_err(|e| {
                Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Time".to_string(),
                    Some(format!("{e}")),
                )))
            })?;
            let cql_time = CqlTime::try_from(naive_time)?;
            Ok(Some(CqlValue::Time(cql_time)))
        }
        (Value::String(s), ColumnType::Native(NativeType::Duration)) => {
            // TODO: add support for the following 'ISO 8601' format variants:
            // - ISO 8601 format: P[n]Y[n]M[n]DT[n]H[n]M[n]S or P[n]W
            // - ISO 8601 alternative format: P[YYYY]-[MM]-[DD]T[hh]:[mm]:[ss]
            // See: https://opensource.docs.scylladb.com/stable/cql/types.html#working-with-durations
            let duration_str = s.borrow_ref().unwrap();
            if duration_str.is_empty() {
                return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Duration".to_string(),
                    Some("Duration cannot be empty".to_string()),
                ))));
            }
            // NOTE: we parse the duration explicitly because of the 'CqlDuration' type specifics.
            // It stores only months, days and nanoseconds.
            // So, we do not translate days to months and hours to days because those are ambiguous
            let (mut months, mut days, mut nanoseconds) = (0, 0, 0);
            let mut matches_counter = HashMap::from([
                ("y", 0),
                ("mo", 0),
                ("w", 0),
                ("d", 0),
                ("h", 0),
                ("m", 0),
                ("s", 0),
                ("ms", 0),
                ("us", 0),
                ("ns", 0),
            ]);
            for cap in DURATION_REGEX.captures_iter(&duration_str) {
                if let Some(m) = cap.name("years") {
                    months += m.as_str().parse::<i32>().unwrap() * 12;
                    *matches_counter.entry("y").or_insert(1) += 1;
                } else if let Some(m) = cap.name("months") {
                    months += m.as_str().parse::<i32>().unwrap();
                    *matches_counter.entry("mo").or_insert(1) += 1;
                } else if let Some(m) = cap.name("weeks") {
                    days += m.as_str().parse::<i32>().unwrap() * 7;
                    *matches_counter.entry("w").or_insert(1) += 1;
                } else if let Some(m) = cap.name("days") {
                    days += m.as_str().parse::<i32>().unwrap();
                    *matches_counter.entry("d").or_insert(1) += 1;
                } else if let Some(m) = cap.name("hours") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap() * 3_600_000_000_000;
                    *matches_counter.entry("h").or_insert(1) += 1;
                } else if let Some(m) = cap.name("minutes") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap() * 60_000_000_000;
                    *matches_counter.entry("m").or_insert(1) += 1;
                } else if let Some(m) = cap.name("seconds") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap() * 1_000_000_000;
                    *matches_counter.entry("s").or_insert(1) += 1;
                } else if let Some(m) = cap.name("millis") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap() * 1_000_000;
                    *matches_counter.entry("ms").or_insert(1) += 1;
                } else if let Some(m) = cap.name("micros") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap() * 1_000;
                    *matches_counter.entry("us").or_insert(1) += 1;
                } else if let Some(m) = cap.name("nanoseconds") {
                    nanoseconds += m.as_str().parse::<i64>().unwrap();
                    *matches_counter.entry("ns").or_insert(1) += 1;
                } else if cap.name("invalid").is_some() {
                    return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                        format!("{v:?}"),
                        "NativeType::Duration".to_string(),
                        Some("Got invalid duration value".to_string()),
                    ))));
                }
            }
            if matches_counter.values().all(|&v| v == 0) {
                return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Duration".to_string(),
                    Some("None time units were found".to_string()),
                ))));
            }
            let duplicated_units: Vec<&str> = matches_counter
                .iter()
                .filter(|&(_, &count)| count > 1)
                .map(|(&unit, _)| unit)
                .collect();
            if !duplicated_units.is_empty() {
                return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Duration".to_string(),
                    Some(format!(
                        "Got multiple matches for time unit(s): {}",
                        duplicated_units.join(", ")
                    )),
                ))));
            }
            let cql_duration = CqlDuration {
                months,
                days,
                nanoseconds,
            };
            Ok(Some(CqlValue::Duration(cql_duration)))
        }

        (Value::String(s), ColumnType::Native(NativeType::Varint)) => {
            let varint_str = s.borrow_ref().unwrap();
            if !varint_str.chars().all(|c| c.is_ascii_digit()) {
                return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Varint".to_string(),
                    Some("Input contains non-digit characters".to_string()),
                ))));
            }
            let byte_vector: Vec<u8> = varint_str
                .chars()
                .map(|c| c.to_digit(10).expect("Invalid digit") as u8)
                .collect();
            Ok(Some(CqlValue::Varint(
                scylla::value::CqlVarint::from_signed_bytes_be(byte_vector),
            )))
        }
        (Value::String(s), ColumnType::Native(NativeType::Timeuuid)) => {
            let timeuuid_str = s.borrow_ref().unwrap();
            let timeuuid = CqlTimeuuid::from_str(timeuuid_str.as_str());
            match timeuuid {
                Ok(timeuuid) => Ok(Some(CqlValue::Timeuuid(timeuuid))),
                Err(e) => Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Timeuuid".to_string(),
                    Some(format!("{e}")),
                )))),
            }
        }
        (
            Value::String(v),
            ColumnType::Native(NativeType::Text) | ColumnType::Native(NativeType::Ascii),
        ) => Ok(Some(CqlValue::Text(
            v.borrow_ref().unwrap().as_str().to_string(),
        ))),
        (Value::String(s), ColumnType::Native(NativeType::Inet)) => {
            let ipaddr_str = s.borrow_ref().unwrap();
            let ipaddr = IpAddr::from_str(ipaddr_str.as_str());
            match ipaddr {
                Ok(ipaddr) => Ok(Some(CqlValue::Inet(ipaddr))),
                Err(e) => Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Inet".to_string(),
                    Some(format!("{e}")),
                )))),
            }
        }
        (Value::String(s), ColumnType::Native(NativeType::Decimal)) => {
            let dec_str = s.borrow_ref().unwrap();
            let decimal = rust_decimal::Decimal::from_str_exact(&dec_str).unwrap();
            Ok(Some(CqlValue::Decimal(
                scylla::value::CqlDecimal::from_signed_be_bytes_and_exponent(
                    decimal.mantissa().to_be_bytes().to_vec(),
                    decimal.scale().try_into().unwrap(),
                ),
            )))
        }
        (Value::Bytes(v), ColumnType::Native(NativeType::Blob)) => {
            Ok(Some(CqlValue::Blob(v.borrow_ref().unwrap().to_vec())))
        }
        (Value::Vec(v), ColumnType::Native(NativeType::Blob)) => {
            let v: Vec<Value> = v.borrow_ref().unwrap().to_vec();
            let byte_vec: Vec<u8> = v
                .into_iter()
                .map(|value| value.as_byte().unwrap())
                .collect();
            Ok(Some(CqlValue::Blob(byte_vec)))
        }
        (Value::Option(v), typ) => match v.borrow_ref().unwrap().as_ref() {
            Some(v) => to_scylla_value(v, typ),
            None => Ok(None),
        },
        (Value::Tuple(v), ColumnType::Tuple(tuple)) => {
            let v = v.borrow_ref().unwrap();
            let mut elements = Vec::with_capacity(v.len());
            for (i, current_element) in v.iter().enumerate() {
                let element = to_scylla_value(current_element, &tuple[i])?;
                elements.push(element);
            }
            Ok(Some(CqlValue::Tuple(elements)))
        }
        (Value::Vec(v), ColumnType::Tuple(tuple)) => {
            let v = v.borrow_ref().unwrap();
            let mut elements = Vec::with_capacity(v.len());
            for (i, current_element) in v.iter().enumerate() {
                let element = to_scylla_value(current_element, &tuple[i])?;
                elements.push(element);
            }
            Ok(Some(CqlValue::Tuple(elements)))
        }
        (Value::Vec(v), ColumnType::Vector { typ, .. }) => {
            let v = v.borrow_ref().unwrap();
            let elements = v
                .as_ref()
                .iter()
                .map(|v| {
                    to_scylla_value(v, typ).and_then(|opt| {
                        opt.ok_or_else(|| {
                            Box::new(CassError(CassErrorKind::QueryParamConversion(
                                format!("{v:?}"),
                                "ColumnType::Vector".to_string(),
                                None,
                            )))
                        })
                    })
                })
                .try_collect()?;
            Ok(Some(CqlValue::Vector(elements)))
        }
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: _,
                typ: CollectionType::List(elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let elements = v
                .as_ref()
                .iter()
                .map(|v| {
                    to_scylla_value(v, elt).and_then(|opt| {
                        opt.ok_or_else(|| {
                            Box::new(CassError(CassErrorKind::QueryParamConversion(
                                format!("{v:?}"),
                                "CollectionType::List".to_string(),
                                None,
                            )))
                        })
                    })
                })
                .try_collect()?;
            Ok(Some(CqlValue::List(elements)))
        }
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: _,
                typ: CollectionType::Set(elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let elements = v
                .as_ref()
                .iter()
                .map(|v| {
                    to_scylla_value(v, elt).and_then(|opt| {
                        opt.ok_or_else(|| {
                            Box::new(CassError(CassErrorKind::QueryParamConversion(
                                format!("{v:?}"),
                                "CollectionType::Set".to_string(),
                                None,
                            )))
                        })
                    })
                })
                .try_collect()?;
            Ok(Some(CqlValue::Set(elements)))
        }
        (
            Value::Vec(v),
            ColumnType::Collection {
                frozen: _,
                typ: CollectionType::Map(key_elt, value_elt),
            },
        ) => {
            let v = v.borrow_ref().unwrap();
            let mut map_vec = Vec::with_capacity(v.len());
            for tuple in v.iter() {
                match tuple {
                    Value::Tuple(tuple) if tuple.borrow_ref().unwrap().len() == 2 => {
                        let tuple = tuple.borrow_ref().unwrap();
                        let key = to_scylla_value(tuple.first().unwrap(), key_elt)?.unwrap();
                        let value = to_scylla_value(tuple.get(1).unwrap(), value_elt)?.unwrap();
                        map_vec.push((key, value));
                    }
                    _ => {
                        return Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                            format!("{tuple:?}"),
                            "CollectionType::Map".to_string(),
                            None,
                        ))));
                    }
                }
            }
            Ok(Some(CqlValue::Map(map_vec)))
        }
        (
            Value::Object(obj),
            ColumnType::Collection {
                frozen: _,
                typ: CollectionType::Map(key_elt, value_elt),
            },
        ) => {
            let obj = obj.borrow_ref().unwrap();
            let mut map_vec = Vec::with_capacity(obj.keys().len());
            for (k, v) in obj.iter() {
                let key = String::from(k.as_str());
                let key = to_scylla_value(&(key.to_value().unwrap()), key_elt)?.unwrap();
                let value = to_scylla_value(v, value_elt)?.unwrap();
                map_vec.push((key, value));
            }
            Ok(Some(CqlValue::Map(map_vec)))
        }
        (
            Value::Object(v),
            ColumnType::UserDefinedType {
                frozen: _,
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
            Ok(Some(CqlValue::UserDefinedType {
                name: definition.name.to_string(),
                keyspace: definition.keyspace.to_string(),
                fields,
            }))
        }
        (
            Value::Struct(v),
            ColumnType::UserDefinedType {
                frozen: _,
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
            Ok(Some(CqlValue::UserDefinedType {
                name: definition.name.to_string(),
                keyspace: definition.keyspace.to_string(),
                fields,
            }))
        }

        (Value::Any(obj), ColumnType::Native(NativeType::Uuid)) => {
            let obj = obj.borrow_ref().unwrap();
            let h = obj.type_hash();
            if h == Uuid::type_hash() {
                let uuid: &Uuid = obj.downcast_borrow_ref().unwrap();
                Ok(Some(CqlValue::Uuid(uuid.0)))
            } else {
                Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
                    format!("{v:?}"),
                    "NativeType::Uuid".to_string(),
                    None,
                ))))
            }
        }
        (value, typ) => Err(Box::new(CassError(CassErrorKind::QueryParamConversion(
            format!("{value:?}"),
            format!("{typ:?}").to_string(),
            None,
        )))),
    }
}

fn convert_int<T: TryFrom<i64>, R>(
    value: i64,
    typ: NativeType,
    f: impl Fn(T) -> R,
) -> Result<Option<R>, Box<CassError>> {
    let converted = value.try_into().map_err(|_| {
        Box::new(CassError(CassErrorKind::ValueOutOfRange(
            value.to_string(),
            format!("{typ:?}").to_string(),
        )))
    })?;
    Ok(Some(f(converted)))
}

fn read_fields<'a, 'b>(
    get_value: impl Fn(&str) -> Option<&'a Value>,
    fields: &[(String, ColumnType)],
) -> Result<Vec<(String, Option<CqlValue>)>, Box<CassError>> {
    let mut values = Vec::with_capacity(fields.len());
    for (field_name, field_type) in fields {
        if let Some(value) = get_value(field_name) {
            let value = to_scylla_value(value, field_type)?;
            values.push((field_name.to_string(), value))
        };
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    use rstest::rstest;
    use rune::alloc::String as RuneString;
    use rune::runtime::{Object, Shared, Vec as RuneVec};
    use scylla::frame::response::result::TableSpec;
    use scylla::serialize::row::RowSerializationContext;
    use scylla::serialize::writers::RowWriter;

    const NS_MULT: i64 = 1_000_000_000;

    // ── helpers ──────────────────────────────────────────────────────

    fn rune_string(s: &str) -> Value {
        Value::String(Shared::new(RuneString::try_from(s).unwrap()).unwrap())
    }

    fn rune_int(i: i64) -> Value {
        Value::Integer(i)
    }

    fn rune_float(f: f64) -> Value {
        Value::Float(f)
    }

    fn rune_bool(b: bool) -> Value {
        Value::Bool(b)
    }

    fn rune_vec(items: Vec<Value>) -> Value {
        let mut v = RuneVec::new();
        for item in items {
            v.push(item).unwrap();
        }
        Value::Vec(Shared::new(v).unwrap())
    }

    fn rune_tuple(items: Vec<Value>) -> Value {
        let mut v = rune::alloc::vec::Vec::new();
        for item in items {
            v.try_push(item).unwrap();
        }
        Value::Tuple(Shared::new(rune::runtime::OwnedTuple::try_from(v).unwrap()).unwrap())
    }

    fn rune_object(pairs: Vec<(&str, Value)>) -> Value {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(RuneString::try_from(k).unwrap(), v).unwrap();
        }
        Value::Object(Shared::new(obj).unwrap())
    }

    fn col_spec<'a>(name: &'a str, typ: ColumnType<'a>) -> ColumnSpec<'a> {
        ColumnSpec::borrowed(name, typ, TableSpec::borrowed("ks", "tbl"))
    }

    fn do_serialize(params: &RuneQueryParams<'_>, columns: &[ColumnSpec<'_>]) -> Vec<u8> {
        let ctx = RowSerializationContext::from_specs(columns);
        let mut buf = Vec::new();
        let mut writer = RowWriter::new(&mut buf);
        params.serialize(&ctx, &mut writer).unwrap();
        buf
    }

    fn do_serialize_err(
        params: &RuneQueryParams<'_>,
        columns: &[ColumnSpec<'_>],
    ) -> SerializationError {
        let ctx = RowSerializationContext::from_specs(columns);
        let mut buf = Vec::new();
        let mut writer = RowWriter::new(&mut buf);
        params.serialize(&ctx, &mut writer).unwrap_err()
    }

    // ── to_scylla_value tests (primitives) ──────────────────────────

    #[test]
    fn test_to_scylla_value_bool() {
        let result = to_scylla_value(&rune_bool(true), &ColumnType::Native(NativeType::Boolean));
        assert_eq!(result.unwrap(), Some(CqlValue::Boolean(true)));
    }

    #[test]
    fn test_to_scylla_value_integer_types() {
        assert_eq!(
            to_scylla_value(&rune_int(42), &ColumnType::Native(NativeType::TinyInt)).unwrap(),
            Some(CqlValue::TinyInt(42))
        );
        assert_eq!(
            to_scylla_value(&rune_int(1000), &ColumnType::Native(NativeType::SmallInt)).unwrap(),
            Some(CqlValue::SmallInt(1000))
        );
        assert_eq!(
            to_scylla_value(&rune_int(100_000), &ColumnType::Native(NativeType::Int)).unwrap(),
            Some(CqlValue::Int(100_000))
        );
        assert_eq!(
            to_scylla_value(&rune_int(i64::MAX), &ColumnType::Native(NativeType::BigInt)).unwrap(),
            Some(CqlValue::BigInt(i64::MAX))
        );
    }

    #[test]
    fn test_to_scylla_value_integer_overflow() {
        let result = to_scylla_value(&rune_int(256), &ColumnType::Native(NativeType::TinyInt));
        assert!(result.is_err());
    }

    #[test]
    fn test_to_scylla_value_float_types() {
        assert_eq!(
            to_scylla_value(&rune_float(2.55), &ColumnType::Native(NativeType::Float)).unwrap(),
            Some(CqlValue::Float(2.55_f32))
        );
        assert_eq!(
            to_scylla_value(&rune_float(2.55), &ColumnType::Native(NativeType::Double)).unwrap(),
            Some(CqlValue::Double(2.55))
        );
    }

    #[test]
    fn test_to_scylla_value_text() {
        let result = to_scylla_value(&rune_string("hello"), &ColumnType::Native(NativeType::Text));
        assert_eq!(result.unwrap(), Some(CqlValue::Text("hello".to_string())));
    }

    #[test]
    fn test_to_scylla_value_ascii() {
        let result = to_scylla_value(
            &rune_string("hello"),
            &ColumnType::Native(NativeType::Ascii),
        );
        assert_eq!(result.unwrap(), Some(CqlValue::Text("hello".to_string())));
    }

    #[test]
    fn test_to_scylla_value_timestamp() {
        let result = to_scylla_value(
            &rune_int(1234567890),
            &ColumnType::Native(NativeType::Timestamp),
        );
        assert_eq!(
            result.unwrap(),
            Some(CqlValue::Timestamp(scylla::value::CqlTimestamp(1234567890)))
        );
    }

    #[test]
    fn test_to_scylla_value_counter() {
        let result = to_scylla_value(&rune_int(5), &ColumnType::Native(NativeType::Counter));
        assert_eq!(
            result.unwrap(),
            Some(CqlValue::Counter(scylla::value::Counter(5)))
        );
    }

    #[test]
    fn test_to_scylla_value_inet_v4() {
        let result = to_scylla_value(
            &rune_string("127.0.0.1"),
            &ColumnType::Native(NativeType::Inet),
        );
        assert_eq!(
            result.unwrap(),
            Some(CqlValue::Inet("127.0.0.1".parse().unwrap()))
        );
    }

    #[test]
    fn test_to_scylla_value_inet_v6() {
        let result = to_scylla_value(&rune_string("::1"), &ColumnType::Native(NativeType::Inet));
        assert_eq!(
            result.unwrap(),
            Some(CqlValue::Inet("::1".parse().unwrap()))
        );
    }

    #[test]
    fn test_to_scylla_value_inet_invalid() {
        let result = to_scylla_value(
            &rune_string("not-an-ip"),
            &ColumnType::Native(NativeType::Inet),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_to_scylla_value_date_string() {
        let result = to_scylla_value(
            &rune_string("2024-01-15"),
            &ColumnType::Native(NativeType::Date),
        );
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_to_scylla_value_date_invalid_string() {
        let result = to_scylla_value(
            &rune_string("not-a-date"),
            &ColumnType::Native(NativeType::Date),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_to_scylla_value_time_string() {
        let result = to_scylla_value(
            &rune_string("13:30:00"),
            &ColumnType::Native(NativeType::Time),
        );
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_to_scylla_value_time_string_with_fractional() {
        let result = to_scylla_value(
            &rune_string("13:30:00.123456"),
            &ColumnType::Native(NativeType::Time),
        );
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_to_scylla_value_timeuuid() {
        let result = to_scylla_value(
            &rune_string("550e8400-e29b-41d4-a716-446655440000"),
            &ColumnType::Native(NativeType::Timeuuid),
        );
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_to_scylla_value_timeuuid_invalid() {
        let result = to_scylla_value(
            &rune_string("not-a-uuid"),
            &ColumnType::Native(NativeType::Timeuuid),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_to_scylla_value_option_none() {
        let val = Value::Option(Shared::new(None).unwrap());
        let result = to_scylla_value(&val, &ColumnType::Native(NativeType::Int));
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_to_scylla_value_option_some() {
        let val = Value::Option(Shared::new(Some(rune_int(42))).unwrap());
        let result = to_scylla_value(&val, &ColumnType::Native(NativeType::Int));
        assert_eq!(result.unwrap(), Some(CqlValue::Int(42)));
    }

    #[test]
    fn test_to_scylla_value_type_mismatch() {
        let result = to_scylla_value(&rune_string("hello"), &ColumnType::Native(NativeType::Int));
        assert!(result.is_err());
    }

    // ── to_scylla_value tests (collections) ─────────────────────────

    #[test]
    fn test_to_scylla_value_list() {
        let val = rune_vec(vec![rune_int(1), rune_int(2), rune_int(3)]);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::List(Box::new(ColumnType::Native(NativeType::Int))),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2), CqlValue::Int(3)])
        );
    }

    #[test]
    fn test_to_scylla_value_frozen_list() {
        let val = rune_vec(vec![rune_int(1), rune_int(2)]);
        let typ = ColumnType::Collection {
            frozen: true,
            typ: CollectionType::List(Box::new(ColumnType::Native(NativeType::Int))),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)])
        );
    }

    #[test]
    fn test_to_scylla_value_set() {
        let val = rune_vec(vec![rune_string("a"), rune_string("b")]);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Set(Box::new(ColumnType::Native(NativeType::Text))),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::Set(vec![
                CqlValue::Text("a".to_string()),
                CqlValue::Text("b".to_string()),
            ])
        );
    }

    #[test]
    fn test_to_scylla_value_frozen_set() {
        let val = rune_vec(vec![rune_int(10)]);
        let typ = ColumnType::Collection {
            frozen: true,
            typ: CollectionType::Set(Box::new(ColumnType::Native(NativeType::Int))),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(result, CqlValue::Set(vec![CqlValue::Int(10)]));
    }

    #[test]
    fn test_to_scylla_value_map_from_vec_of_tuples() {
        let val = rune_vec(vec![
            rune_tuple(vec![rune_string("key1"), rune_int(1)]),
            rune_tuple(vec![rune_string("key2"), rune_int(2)]),
        ]);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(
                Box::new(ColumnType::Native(NativeType::Text)),
                Box::new(ColumnType::Native(NativeType::Int)),
            ),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::Map(vec![
                (CqlValue::Text("key1".to_string()), CqlValue::Int(1)),
                (CqlValue::Text("key2".to_string()), CqlValue::Int(2)),
            ])
        );
    }

    #[test]
    fn test_to_scylla_value_frozen_map() {
        let val = rune_vec(vec![rune_tuple(vec![rune_int(1), rune_string("one")])]);
        let typ = ColumnType::Collection {
            frozen: true,
            typ: CollectionType::Map(
                Box::new(ColumnType::Native(NativeType::Int)),
                Box::new(ColumnType::Native(NativeType::Text)),
            ),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::Map(vec![(CqlValue::Int(1), CqlValue::Text("one".to_string())),])
        );
    }

    #[test]
    fn test_to_scylla_value_map_from_object() {
        let val = rune_object(vec![("a", rune_int(1)), ("b", rune_int(2))]);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(
                Box::new(ColumnType::Native(NativeType::Text)),
                Box::new(ColumnType::Native(NativeType::Int)),
            ),
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        if let CqlValue::Map(pairs) = result {
            assert_eq!(pairs.len(), 2);
        } else {
            panic!("Expected CqlValue::Map");
        }
    }

    #[test]
    fn test_to_scylla_value_tuple() {
        let val = rune_tuple(vec![rune_int(1), rune_string("hello")]);
        let typ = ColumnType::Tuple(vec![
            ColumnType::Native(NativeType::Int),
            ColumnType::Native(NativeType::Text),
        ]);
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::Tuple(vec![
                Some(CqlValue::Int(1)),
                Some(CqlValue::Text("hello".to_string())),
            ])
        );
    }

    #[test]
    fn test_to_scylla_value_vector() {
        let val = rune_vec(vec![rune_float(1.0), rune_float(2.0), rune_float(3.0)]);
        let typ = ColumnType::Vector {
            typ: Box::new(ColumnType::Native(NativeType::Float)),
            dimensions: 3,
        };
        let result = to_scylla_value(&val, &typ).unwrap().unwrap();
        assert_eq!(
            result,
            CqlValue::Vector(vec![
                CqlValue::Float(1.0),
                CqlValue::Float(2.0),
                CqlValue::Float(3.0),
            ])
        );
    }

    // ── RuneQueryParams serialize tests ─────────────────────────────

    #[test]
    fn test_serialize_no_params_no_columns() {
        let params = RuneQueryParams::new(None);
        let cols: Vec<ColumnSpec<'_>> = vec![];
        let buf = do_serialize(&params, &cols);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_serialize_no_params_with_columns_errors() {
        let params = RuneQueryParams::new(None);
        let cols = [col_spec("a", ColumnType::Native(NativeType::Int))];
        let _err = do_serialize_err(&params, &cols);
    }

    #[test]
    fn test_serialize_tuple_params() {
        let val = rune_tuple(vec![rune_int(42), rune_string("hello")]);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Text)),
        ];
        let buf = do_serialize(&params, &cols);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_serialize_tuple_wrong_count_errors() {
        let val = rune_tuple(vec![rune_int(42)]);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Text)),
        ];
        let _err = do_serialize_err(&params, &cols);
    }

    #[test]
    fn test_serialize_vec_params() {
        let val = rune_vec(vec![rune_int(1), rune_int(2)]);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Int)),
        ];
        let buf = do_serialize(&params, &cols);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_serialize_object_params() {
        let val = rune_object(vec![("a", rune_int(1)), ("b", rune_string("hello"))]);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Text)),
        ];
        let buf = do_serialize(&params, &cols);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_serialize_object_missing_field_uses_empty() {
        let val = rune_object(vec![("a", rune_int(1))]);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Text)),
        ];
        // Missing field "b" should serialize as Empty (no error)
        let buf = do_serialize(&params, &cols);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_serialize_invalid_param_type_errors() {
        let val = rune_bool(true);
        let params = RuneQueryParams::new(Some(&val));
        let cols = [col_spec("a", ColumnType::Native(NativeType::Int))];
        let _err = do_serialize_err(&params, &cols);
    }

    #[test]
    fn test_serialize_is_empty() {
        let params_none = RuneQueryParams::new(None);
        assert!(params_none.is_empty());

        let val = rune_int(1);
        let params_some = RuneQueryParams::new(Some(&val));
        assert!(!params_some.is_empty());
    }

    #[test]
    fn test_serialize_tuple_produces_same_bytes_as_native() {
        // Verify that serializing rune values produces the same bytes
        // as serializing native CqlValues for the same data
        let rune_val = rune_tuple(vec![rune_int(42), rune_bool(true)]);
        let rune_params = RuneQueryParams::new(Some(&rune_val));

        let native_vals: (i32, bool) = (42, true);

        let cols = [
            col_spec("a", ColumnType::Native(NativeType::Int)),
            col_spec("b", ColumnType::Native(NativeType::Boolean)),
        ];
        let ctx = RowSerializationContext::from_specs(&cols);

        let mut rune_buf = Vec::new();
        let mut rune_writer = RowWriter::new(&mut rune_buf);
        rune_params.serialize(&ctx, &mut rune_writer).unwrap();

        let mut native_buf = Vec::new();
        let mut native_writer = RowWriter::new(&mut native_buf);
        SerializeRow::serialize(&native_vals, &ctx, &mut native_writer).unwrap();

        assert_eq!(rune_buf, native_buf);
    }

    #[test]
    fn test_serialize_text_produces_same_bytes_as_native() {
        let rune_val = rune_tuple(vec![rune_string("hello world")]);
        let rune_params = RuneQueryParams::new(Some(&rune_val));

        let native_vals = ("hello world",);

        let cols = [col_spec("a", ColumnType::Native(NativeType::Text))];
        let ctx = RowSerializationContext::from_specs(&cols);

        let mut rune_buf = Vec::new();
        let mut rune_writer = RowWriter::new(&mut rune_buf);
        rune_params.serialize(&ctx, &mut rune_writer).unwrap();

        let mut native_buf = Vec::new();
        let mut native_writer = RowWriter::new(&mut native_buf);
        SerializeRow::serialize(&native_vals, &ctx, &mut native_writer).unwrap();

        assert_eq!(rune_buf, native_buf);
    }

    #[test]
    fn test_serialize_multiple_types_match_native() {
        let rune_val = rune_tuple(vec![
            rune_int(123),
            rune_float(9.99),
            rune_string("test"),
            rune_bool(false),
        ]);
        let rune_params = RuneQueryParams::new(Some(&rune_val));

        let native_vals = (123_i64, 9.99_f64, "test", false);

        let cols = [
            col_spec("a", ColumnType::Native(NativeType::BigInt)),
            col_spec("b", ColumnType::Native(NativeType::Double)),
            col_spec("c", ColumnType::Native(NativeType::Text)),
            col_spec("d", ColumnType::Native(NativeType::Boolean)),
        ];
        let ctx = RowSerializationContext::from_specs(&cols);

        let mut rune_buf = Vec::new();
        let mut rune_writer = RowWriter::new(&mut rune_buf);
        rune_params.serialize(&ctx, &mut rune_writer).unwrap();

        let mut native_buf = Vec::new();
        let mut native_writer = RowWriter::new(&mut native_buf);
        SerializeRow::serialize(&native_vals, &ctx, &mut native_writer).unwrap();

        assert_eq!(rune_buf, native_buf);
    }

    // ── duration tests ──────────────────────────────────────────────

    #[rstest]
    #[case("45ns", 0, 0, 45)]
    #[case("32us", 0, 0, 32 * 1_000)]
    #[case("22ms", 0, 0, 22 * 1_000_000)]
    #[case("15s", 0, 0, 15 * NS_MULT)]
    #[case("2m", 0, 0, 2 * 60 * NS_MULT)]
    #[case("4h", 0, 0, 4 * 3_600 * NS_MULT)]
    #[case("3d", 0, 3, 0)]
    #[case("1w", 0, 7, 0)]
    #[case("1mo", 1, 0, 0)]
    #[case("1y", 12, 0, 0)]
    #[case("45m1s", 0, 0, (45 * 60 + 1) * NS_MULT)]
    #[case("3d21h13m", 0, 3, (21 * 3_600 + 13 * 60) * NS_MULT)]
    #[case("1y3mo2w6d13h14m23s", 15, 20, (13 * 3_600 + 14 * 60 + 23) * NS_MULT)]
    fn test_to_scylla_value_duration_pos(
        #[case] input: String,
        #[case] mo: i32,
        #[case] d: i32,
        #[case] ns: i64,
    ) {
        let expected = format!("{mo:?}mo{d:?}d{ns:?}ns");
        let duration_rune_str = Value::String(
            Shared::new(RuneString::try_from(input).expect("Failed to create RuneString"))
                .expect("Failed to create Shared RuneString"),
        );
        let actual = to_scylla_value(
            &duration_rune_str,
            &ColumnType::Native(NativeType::Duration),
        );
        assert_eq!(actual.unwrap().unwrap().to_string(), expected);
    }

    #[rstest]
    #[case("")]
    #[case(" ")]
    #[case("\n")]
    #[case("1")]
    #[case("m1")]
    #[case("1mm")]
    #[case("1mom")]
    #[case("fake")]
    #[case("1d2h3m4h")]
    fn test_to_scylla_value_duration_neg(#[case] input: String) {
        let duration_rune_str = Value::String(
            Shared::new(RuneString::try_from(input.clone()).expect("Failed to create RuneString"))
                .expect("Failed to create Shared RuneString"),
        );
        let actual = to_scylla_value(
            &duration_rune_str,
            &ColumnType::Native(NativeType::Duration),
        );
        assert!(
            matches!(
                actual,
                Err(ref box_err) if matches!(**box_err, CassError(CassErrorKind::QueryParamConversion(_, _, _)))
            ),
            "{}",
            format!("Error was not raised for the {input:?} input. Result: {actual:?}")
        );
    }
}
