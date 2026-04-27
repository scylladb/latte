//! Functions for deserializing CQL row data into rune values

use super::cass_error::{CassError, CassErrorKind};

use rune::alloc::vec::Vec as RuneAllocVec;
use rune::alloc::String as RuneString;
use rune::runtime::{Object, OwnedTuple, Shared, Vec as RuneVec};
use rune::Value;
use scylla::deserialize::row::{ColumnIterator, DeserializeRow};
use scylla::deserialize::value::DeserializeValue;
use scylla::deserialize::{DeserializationError, TypeCheckError};
use scylla::frame::response::result::ColumnSpec;
use scylla::value::CqlValue;

/// Converts a Scylla CqlValue to a Rune Value
fn cql_value_to_rune_value(value: Option<&CqlValue>) -> Result<Value, Box<CassError>> {
    match value {
        Some(CqlValue::Ascii(s)) | Some(CqlValue::Text(s)) => Ok(Value::String(Shared::new(
            RuneString::try_from(s.clone()).expect("Failed to create RuneString"),
        )?)),
        Some(CqlValue::Boolean(b)) => Ok(Value::Bool(*b)),
        Some(CqlValue::TinyInt(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::SmallInt(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::Int(i)) => Ok(Value::Integer(*i as i64)),
        Some(CqlValue::BigInt(i)) => Ok(Value::Integer(*i)),
        Some(CqlValue::Float(f)) => Ok(Value::Float(*f as f64)),
        Some(CqlValue::Double(f)) => Ok(Value::Float(*f)),
        Some(CqlValue::Counter(c)) => Ok(Value::Integer(c.0)),
        Some(CqlValue::Timestamp(ts)) => Ok(Value::Integer(ts.0)),
        Some(CqlValue::Date(date)) => Ok(Value::Integer(date.0 as i64)),
        Some(CqlValue::Time(time)) => Ok(Value::Integer(time.0)),
        Some(CqlValue::Blob(blob)) => {
            let mut rune_vec = RuneVec::new();
            for byte in blob {
                rune_vec.push(Value::Byte(*byte)).map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to push byte to Rune vector".to_string(),
                    )))
                })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector for blob".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Uuid(uuid)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(uuid.to_string())
                    .expect("Failed to create RuneString for UUID"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for UUID".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Timeuuid(timeuuid)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(timeuuid.to_string())
                    .expect("Failed to create RuneString for TimeUuid"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for TimeUuid".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Inet(addr)) => Ok(Value::String(
            Shared::new(
                RuneString::try_from(addr.to_string())
                    .expect("Failed to create RuneString for IpAddr"),
            )
            .map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared string for IpAddr".to_string(),
                )))
            })?,
        )),
        Some(CqlValue::Vector(vector)) => {
            let mut rune_vec = RuneVec::new();
            for item in vector {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::List(list)) => {
            let mut rune_vec = RuneVec::new();
            for item in list {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Set(set)) => {
            let mut rune_vec = RuneVec::new();
            for item in set {
                rune_vec
                    .push(cql_value_to_rune_value(Some(item))?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Map(map)) => {
            let mut rune_vec = RuneVec::new();
            for (key, value) in map {
                let mut pair = RuneAllocVec::new();
                pair.try_push(cql_value_to_rune_value(Some(key))?)?;
                pair.try_push(cql_value_to_rune_value(Some(value))?)?;
                rune_vec
                    .push(Value::Tuple(Shared::new(OwnedTuple::try_from(pair)?)?))
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push map key-value pair to the Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared Rune vector".to_string(),
                )))
            })?))
        }
        Some(CqlValue::UserDefinedType { fields, .. }) => {
            let mut rune_obj = Object::new();
            for (field_name, field_value) in fields {
                rune_obj
                    .insert(
                        RuneString::try_from(field_name.clone())
                            .expect("Failed to create RuneString"),
                        cql_value_to_rune_value(field_value.as_ref())?,
                    )
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to insert UDT field into Rune object".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Object(Shared::new(rune_obj).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared object for UDT".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Tuple(tuple)) => {
            let mut rune_vec = RuneVec::new();
            for item in tuple {
                rune_vec
                    .push(cql_value_to_rune_value(item.as_ref())?)
                    .map_err(|_| {
                        Box::new(CassError(CassErrorKind::Error(
                            "Failed to push tuple item to Rune vector".to_string(),
                        )))
                    })?;
            }
            Ok(Value::Vec(Shared::new(rune_vec).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared vector for tuple".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Varint(varint)) => Ok(Value::Integer({
            let varint_bytes = varint.as_signed_bytes_be_slice();
            if varint_bytes.len() > 8 {
                return Err(Box::new(CassError(CassErrorKind::Error(
                    "Varint is too large to fit into an i64".to_string(),
                ))));
            };
            let mut padded = [0u8; 8];
            if varint_bytes[0] & 0x80 != 0 {
                padded[..8 - varint_bytes.len()].fill(0xFF);
            }
            padded[8 - varint_bytes.len()..].copy_from_slice(varint_bytes);
            i64::from_be_bytes(padded)
        })),
        Some(CqlValue::Decimal(decimal)) => {
            let (mantissa_be, scale) = &decimal.clone().into_signed_be_bytes_and_exponent();
            let mantissa = if mantissa_be.len() == 8 {
                i64::from_be_bytes(mantissa_be.as_slice().try_into().unwrap())
            } else if mantissa_be.len() < 8 {
                let mut mantissa_array = [0u8; 8];
                mantissa_array[8 - mantissa_be.len()..].copy_from_slice(mantissa_be);
                i64::from_be_bytes(mantissa_array)
            } else {
                let truncated = &mantissa_be[mantissa_be.len() - 8..];
                i64::from_be_bytes(truncated.try_into().unwrap())
            };
            let dec = rust_decimal::Decimal::try_new(mantissa, u32::try_from(*scale)?).unwrap();
            Ok(Value::String(
                Shared::new(
                    RuneString::try_from(dec.to_string()).expect("Failed to create RuneString"),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to create shared string for Decimal".to_string(),
                    )))
                })?,
            ))
        }
        Some(CqlValue::Duration(duration)) => {
            // TODO: update the logic for duration to provide also a duration-like string such as "1h2m3s"
            let mut rune_obj = Object::new();
            rune_obj
                .insert(
                    RuneString::try_from("months").expect("Failed to create RuneString"),
                    Value::Integer(duration.months as i64),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert months into duration object".to_string(),
                    )))
                })?;
            rune_obj
                .insert(
                    RuneString::try_from("days").expect("Failed to create RuneString"),
                    Value::Integer(duration.days as i64),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert days into duration object".to_string(),
                    )))
                })?;
            rune_obj
                .insert(
                    RuneString::try_from("nanoseconds").expect("Failed to create RuneString"),
                    Value::Integer(duration.nanoseconds),
                )
                .map_err(|_| {
                    Box::new(CassError(CassErrorKind::Error(
                        "Failed to insert nanoseconds into duration object".to_string(),
                    )))
                })?;
            Ok(Value::Object(Shared::new(rune_obj).map_err(|_| {
                Box::new(CassError(CassErrorKind::Error(
                    "Failed to create shared object for Duration".to_string(),
                )))
            })?))
        }
        Some(CqlValue::Empty) => Ok(Value::Option(Shared::new(None)?)),
        None => Ok(Value::Option(Shared::new(None)?)),
        Some(&_) => todo!(), // unexpected, should never be reached
    }
}

/// A row deserialized directly into a rune `Object`, bypassing the intermediate
/// `Row` / `Vec<(String, CqlValue)>` representation.
pub(super) struct RuneRow(pub Object);

impl<'frame, 'metadata> DeserializeRow<'frame, 'metadata> for RuneRow {
    fn type_check(_specs: &[ColumnSpec<'_>]) -> Result<(), TypeCheckError> {
        // Accept all column types, same as Row
        Ok(())
    }

    fn deserialize(
        mut row: ColumnIterator<'frame, 'metadata>,
    ) -> Result<Self, DeserializationError> {
        let mut obj = Object::new();
        while let Some(column) = row.next().transpose()? {
            let col_name = column.spec.name();
            let cql_value = <Option<CqlValue>>::deserialize(column.spec.typ(), column.slice)?;
            let rune_value = cql_value_to_rune_value(cql_value.as_ref())
                .map_err(|e| DeserializationError::new(*e))?;
            obj.insert(
                RuneString::try_from(col_name.to_string())
                    .expect("Failed to create RuneString for column name"),
                rune_value,
            )
            .map_err(|e| {
                DeserializationError::new(CassError(CassErrorKind::Error(e.to_string())))
            })?;
        }
        Ok(RuneRow(obj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{CqlDuration, CqlVarint};
    use std::net::IpAddr;

    /// Helper to extract i64 from a rune Value::Integer
    fn assert_int(val: &Value, expected: i64) {
        assert_eq!(val.as_integer().into_result().unwrap(), expected);
    }

    /// Helper to extract f64 from a rune Value::Float
    fn assert_float(val: &Value, expected: f64) {
        let actual = val.as_float().into_result().unwrap();
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    /// Helper to extract bool from a rune Value::Bool
    fn assert_bool(val: &Value, expected: bool) {
        assert_eq!(val.as_bool().into_result().unwrap(), expected);
    }

    #[test]
    fn test_deser_text() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Text("hello".to_string()))).unwrap();
        match val {
            Value::String(s) => assert_eq!(s.borrow_ref().unwrap().as_str(), "hello"),
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_ascii() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Ascii("ascii".to_string()))).unwrap();
        match val {
            Value::String(s) => assert_eq!(s.borrow_ref().unwrap().as_str(), "ascii"),
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_boolean() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Boolean(true))).unwrap();
        assert_bool(&val, true);

        let val = cql_value_to_rune_value(Some(&CqlValue::Boolean(false))).unwrap();
        assert_bool(&val, false);
    }

    #[test]
    fn test_deser_integer_types() {
        let val = cql_value_to_rune_value(Some(&CqlValue::TinyInt(42))).unwrap();
        assert_int(&val, 42);

        let val = cql_value_to_rune_value(Some(&CqlValue::SmallInt(1000))).unwrap();
        assert_int(&val, 1000);

        let val = cql_value_to_rune_value(Some(&CqlValue::Int(100_000))).unwrap();
        assert_int(&val, 100_000);

        let val = cql_value_to_rune_value(Some(&CqlValue::BigInt(i64::MAX))).unwrap();
        assert_int(&val, i64::MAX);
    }

    #[test]
    fn test_deser_float_types() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Float(2.55))).unwrap();
        assert_float(&val, 2.55_f32 as f64);

        let val = cql_value_to_rune_value(Some(&CqlValue::Double(2.55))).unwrap();
        assert_float(&val, 2.55);
    }

    #[test]
    fn test_deser_counter() {
        let val =
            cql_value_to_rune_value(Some(&CqlValue::Counter(scylla::value::Counter(42)))).unwrap();
        assert_int(&val, 42);
    }

    #[test]
    fn test_deser_timestamp() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Timestamp(scylla::value::CqlTimestamp(
            1234567890,
        ))))
        .unwrap();
        assert_int(&val, 1234567890);
    }

    #[test]
    fn test_deser_date() {
        let val =
            cql_value_to_rune_value(Some(&CqlValue::Date(scylla::value::CqlDate(19000)))).unwrap();
        assert_int(&val, 19000);
    }

    #[test]
    fn test_deser_time() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Time(scylla::value::CqlTime(123456789))))
            .unwrap();
        assert_int(&val, 123456789);
    }

    #[test]
    fn test_deser_blob() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Blob(vec![0x01, 0x02, 0xFF]))).unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 3);
                assert_eq!(v.get(0).unwrap().as_byte().into_result().unwrap(), 0x01);
                assert_eq!(v.get(1).unwrap().as_byte().into_result().unwrap(), 0x02);
                assert_eq!(v.get(2).unwrap().as_byte().into_result().unwrap(), 0xFF);
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_uuid() {
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let val = cql_value_to_rune_value(Some(&CqlValue::Uuid(uuid))).unwrap();
        match val {
            Value::String(s) => {
                assert_eq!(
                    s.borrow_ref().unwrap().as_str(),
                    "550e8400-e29b-41d4-a716-446655440000"
                );
            }
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_timeuuid() {
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-11d4-a716-446655440000").unwrap();
        let timeuuid = scylla::value::CqlTimeuuid::from(uuid);
        let val = cql_value_to_rune_value(Some(&CqlValue::Timeuuid(timeuuid))).unwrap();
        match val {
            Value::String(s) => {
                assert_eq!(
                    s.borrow_ref().unwrap().as_str(),
                    "550e8400-e29b-11d4-a716-446655440000"
                );
            }
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_inet() {
        let addr: IpAddr = "127.0.0.1".parse().unwrap();
        let val = cql_value_to_rune_value(Some(&CqlValue::Inet(addr))).unwrap();
        match val {
            Value::String(s) => assert_eq!(s.borrow_ref().unwrap().as_str(), "127.0.0.1"),
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_inet_v6() {
        let addr: IpAddr = "::1".parse().unwrap();
        let val = cql_value_to_rune_value(Some(&CqlValue::Inet(addr))).unwrap();
        match val {
            Value::String(s) => assert_eq!(s.borrow_ref().unwrap().as_str(), "::1"),
            other => panic!("Expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_list() {
        let val = cql_value_to_rune_value(Some(&CqlValue::List(vec![
            CqlValue::Int(1),
            CqlValue::Int(2),
            CqlValue::Int(3),
        ])))
        .unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 3);
                assert_int(v.get(0).unwrap(), 1);
                assert_int(v.get(1).unwrap(), 2);
                assert_int(v.get(2).unwrap(), 3);
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_set() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Set(vec![
            CqlValue::Text("a".to_string()),
            CqlValue::Text("b".to_string()),
        ])))
        .unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 2);
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_map() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Map(vec![
            (CqlValue::Text("key1".to_string()), CqlValue::Int(1)),
            (CqlValue::Text("key2".to_string()), CqlValue::Int(2)),
        ])))
        .unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 2);
                // Each element should be a tuple of (key, value)
                match v.get(0).unwrap() {
                    Value::Tuple(t) => {
                        let t = t.borrow_ref().unwrap();
                        assert_eq!(t.len(), 2);
                    }
                    other => panic!("Expected Tuple, got {other:?}"),
                }
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_tuple() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Tuple(vec![
            Some(CqlValue::Int(1)),
            Some(CqlValue::Text("hello".to_string())),
            None,
        ])))
        .unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 3);
                assert_int(v.get(0).unwrap(), 1);
                // third element should be None option
                match v.get(2).unwrap() {
                    Value::Option(_) => {}
                    other => panic!("Expected Option, got {other:?}"),
                }
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_udt() {
        let val = cql_value_to_rune_value(Some(&CqlValue::UserDefinedType {
            name: "my_type".to_string(),
            keyspace: "ks".to_string(),
            fields: vec![
                ("field_a".to_string(), Some(CqlValue::Int(42))),
                (
                    "field_b".to_string(),
                    Some(CqlValue::Text("hello".to_string())),
                ),
            ],
        }))
        .unwrap();
        match val {
            Value::Object(obj) => {
                let obj = obj.borrow_ref().unwrap();
                assert_eq!(obj.len(), 2);
                assert_int(obj.get("field_a").unwrap(), 42);
                match obj.get("field_b").unwrap() {
                    Value::String(s) => {
                        assert_eq!(s.borrow_ref().unwrap().as_str(), "hello")
                    }
                    other => panic!("Expected String, got {other:?}"),
                }
            }
            other => panic!("Expected Object, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_vector() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Vector(vec![
            CqlValue::Float(1.0),
            CqlValue::Float(2.0),
        ])))
        .unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 2);
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_varint_small() {
        let varint = CqlVarint::from_signed_bytes_be(42_i64.to_be_bytes().to_vec());
        let val = cql_value_to_rune_value(Some(&CqlValue::Varint(varint))).unwrap();
        assert_int(&val, 42);
    }

    #[test]
    fn test_deser_varint_negative() {
        let varint = CqlVarint::from_signed_bytes_be((-100_i64).to_be_bytes().to_vec());
        let val = cql_value_to_rune_value(Some(&CqlValue::Varint(varint))).unwrap();
        assert_int(&val, -100);
    }

    #[test]
    fn test_deser_duration() {
        let duration = CqlDuration {
            months: 3,
            days: 5,
            nanoseconds: 1_000_000_000,
        };
        let val = cql_value_to_rune_value(Some(&CqlValue::Duration(duration))).unwrap();
        match val {
            Value::Object(obj) => {
                let obj = obj.borrow_ref().unwrap();
                assert_int(obj.get("months").unwrap(), 3);
                assert_int(obj.get("days").unwrap(), 5);
                assert_int(obj.get("nanoseconds").unwrap(), 1_000_000_000);
            }
            other => panic!("Expected Object, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_empty() {
        let val = cql_value_to_rune_value(Some(&CqlValue::Empty)).unwrap();
        match val {
            Value::Option(opt) => assert!(opt.borrow_ref().unwrap().is_none()),
            other => panic!("Expected Option(None), got {other:?}"),
        }
    }

    #[test]
    fn test_deser_null() {
        let val = cql_value_to_rune_value(None).unwrap();
        match val {
            Value::Option(opt) => assert!(opt.borrow_ref().unwrap().is_none()),
            other => panic!("Expected Option(None), got {other:?}"),
        }
    }

    #[test]
    fn test_deser_nested_list_of_lists() {
        let inner1 = CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)]);
        let inner2 = CqlValue::List(vec![CqlValue::Int(3)]);
        let val = cql_value_to_rune_value(Some(&CqlValue::List(vec![inner1, inner2]))).unwrap();
        match val {
            Value::Vec(v) => {
                let v = v.borrow_ref().unwrap();
                assert_eq!(v.len(), 2);
                match v.get(0).unwrap() {
                    Value::Vec(inner) => {
                        assert_eq!(inner.borrow_ref().unwrap().len(), 2)
                    }
                    other => panic!("Expected inner Vec, got {other:?}"),
                }
            }
            other => panic!("Expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn test_deser_udt_with_null_field() {
        let val = cql_value_to_rune_value(Some(&CqlValue::UserDefinedType {
            name: "my_type".to_string(),
            keyspace: "ks".to_string(),
            fields: vec![
                ("present".to_string(), Some(CqlValue::Int(1))),
                ("absent".to_string(), None),
            ],
        }))
        .unwrap();
        match val {
            Value::Object(obj) => {
                let obj = obj.borrow_ref().unwrap();
                assert_int(obj.get("present").unwrap(), 1);
                match obj.get("absent").unwrap() {
                    Value::Option(opt) => {
                        assert!(opt.borrow_ref().unwrap().is_none())
                    }
                    other => panic!("Expected Option(None), got {other:?}"),
                }
            }
            other => panic!("Expected Object, got {other:?}"),
        }
    }
}
