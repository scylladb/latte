//! Functions for binding rune values to CQL parameters

use super::cass_error::{CassError, CassErrorKind};
use super::cql_types::Uuid;
use chrono::{NaiveDate, NaiveTime};
use once_cell::sync::Lazy;
use regex::Regex;
use rune::{Any, ToValue, Value};
use scylla::_macro_internal::ColumnType;
use scylla::frame::response::result::{CollectionType, ColumnSpec, NativeType};
use scylla::response::query_result::ColumnSpecs;
use scylla::value::{CqlDate, CqlDuration, CqlTime, CqlTimeuuid, CqlValue, CqlVarint};
use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use itertools::*;

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
                frozen: false,
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
                frozen: false,
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
                frozen: false,
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
            Ok(Some(CqlValue::UserDefinedType {
                name: definition.name.to_string(),
                keyspace: definition.keyspace.to_string(),
                fields,
            }))
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

/// Binds parameters passed as a single rune value to the arguments of the statement.
/// The `params` value can be a tuple, a vector, a struct or an object.
pub fn to_scylla_query_params(
    params: &Value,
    types: ColumnSpecs,
) -> Result<Vec<Option<CqlValue>>, Box<CassError>> {
    Ok(match params {
        Value::Tuple(tuple) => {
            let mut values = Vec::new();
            let tuple = tuple.borrow_ref().unwrap();
            if tuple.len() != types.len() {
                return Err(Box::new(CassError(
                    CassErrorKind::InvalidNumberOfQueryParams,
                )));
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
            return Err(Box::new(CassError(
                CassErrorKind::InvalidQueryParamsObject(other.type_info().unwrap()),
            )));
        }
    })
}

fn read_params<'a, 'b>(
    get_value: impl Fn(&str) -> Option<&'a Value>,
    params: &[ColumnSpec],
) -> Result<Vec<Option<CqlValue>>, Box<CassError>> {
    let mut values = Vec::with_capacity(params.len());
    for column in params {
        let value = match get_value(column.name()) {
            Some(value) => to_scylla_value(value, column.typ())?,
            None => Some(CqlValue::Empty),
        };
        values.push(value)
    }
    Ok(values)
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
    use rune::runtime::Shared;

    const NS_MULT: i64 = 1_000_000_000;

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
