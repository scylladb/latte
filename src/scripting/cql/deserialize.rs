//! Functions for deserializing CQL row data into rune values

use std::net::IpAddr;

use super::cass_error::{CassError, CassErrorKind};

use rune::alloc::String as RuneString;
use rune::runtime::{Object, OwnedTuple, Shared, Vec as RuneVec};
use rune::Value;
use scylla::cluster::metadata::{CollectionType, ColumnType, NativeType};
use scylla::deserialize::row::{ColumnIterator, DeserializeRow};
use scylla::deserialize::value::{DeserializeValue, ListlikeIterator, MapIterator, UdtIterator};
use scylla::deserialize::{DeserializationError, TypeCheckError};
use scylla::frame::response::result::ColumnSpec;
use scylla::value::{
    Counter, CqlDate, CqlDecimalBorrowed, CqlDuration, CqlTime, CqlTimestamp, CqlTimeuuid,
    CqlVarintBorrowed,
};
use scylla_cql::deserialize::value::VectorIterator;
use uuid::Uuid;

/// A value deserialized directly into a rune `Value`, bypassing the intermediate
/// `CqlValue` representation.
pub(super) struct RuneValue(pub Value);

impl<'frame, 'metadata> DeserializeValue<'frame, 'metadata> for RuneValue {
    fn type_check(_typ: &scylla::cluster::metadata::ColumnType) -> Result<(), TypeCheckError> {
        // Accept all column types, same as CqlValue
        Ok(())
    }

    fn deserialize(
        typ: &'metadata scylla::cluster::metadata::ColumnType<'metadata>,
        v: Option<scylla::deserialize::FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError> {
        let Some(slice) = v else {
            return Ok(RuneValue(Value::Option(Shared::new(None).map_err(
                |_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared None".to_string(),
                    )))
                },
            )?)));
        };

        // This matches the old logic that used CqlValue
        if slice.is_empty() {
            match typ {
                ColumnType::Native(NativeType::Ascii)
                | ColumnType::Native(NativeType::Blob)
                | ColumnType::Native(NativeType::Text) => {
                    // can't be empty
                }
                _ => {
                    return Ok(RuneValue(Value::Option(Shared::new(None).map_err(
                        |_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to create shared None".to_string(),
                            )))
                        },
                    )?)))
                }
            }
        }

        let value: Value = match typ {
            ColumnType::Native(NativeType::Ascii) | ColumnType::Native(NativeType::Text) => {
                let string =
                    <String as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))?;
                Value::String(
                    Shared::new(RuneString::try_from(string).expect("Failed to create RuneString"))
                        .map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to create shared string".to_string(),
                            )))
                        })?,
                )
            }
            ColumnType::Native(NativeType::Boolean) => {
                <bool as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(Value::Bool)?
            }
            ColumnType::Native(NativeType::TinyInt) => {
                <i8 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|i| Value::Integer(i.into()))?
            }
            ColumnType::Native(NativeType::SmallInt) => {
                <i16 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|i| Value::Integer(i.into()))?
            }
            ColumnType::Native(NativeType::Int) => {
                <i32 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|i| Value::Integer(i.into()))?
            }
            ColumnType::Native(NativeType::BigInt) => {
                <i64 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(Value::Integer)?
            }
            ColumnType::Native(NativeType::Float) => {
                <f32 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|i| Value::Float(i.into()))?
            }
            ColumnType::Native(NativeType::Double) => {
                <f64 as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(Value::Float)?
            }
            ColumnType::Native(NativeType::Counter) => {
                <Counter as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|c| Value::Integer(c.0))?
            }
            ColumnType::Native(NativeType::Timestamp) => {
                <CqlTimestamp as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|ts| Value::Integer(ts.0))?
            }
            ColumnType::Native(NativeType::Date) => {
                <CqlDate as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|date| Value::Integer(date.0.into()))?
            }
            ColumnType::Native(NativeType::Time) => {
                <CqlTime as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))
                    .map(|time| Value::Integer(time.0))?
            }
            ColumnType::Native(NativeType::Blob) => {
                // Note: is it intentional that blobs are representes as Vec of rune Bytes?
                // I see that Value::Bytes exists.
                let bytes_slice =
                    <&'frame [u8] as DeserializeValue<'frame, 'metadata>>::deserialize(
                        typ,
                        Some(slice),
                    )?;
                let mut rune_vec = RuneVec::new();
                for byte in bytes_slice {
                    rune_vec.push(Value::Byte(*byte)).map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to push byte to Rune vector".to_string(),
                        )))
                    })?;
                }
                Value::Vec(Shared::new(rune_vec).map_err(|_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared vector for blob".to_string(),
                    )))
                })?)
            }
            ColumnType::Native(NativeType::Uuid) => {
                let uuid =
                    <Uuid as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))?;
                Value::String(
                    Shared::new(
                        RuneString::try_from(uuid.to_string())
                            .expect("Failed to create RuneString for UUID"),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared string for UUID".to_string(),
                        )))
                    })?,
                )
            }
            ColumnType::Native(NativeType::Timeuuid) => {
                let timeuuid = <CqlTimeuuid as DeserializeValue<'frame, 'metadata>>::deserialize(
                    typ,
                    Some(slice),
                )?;
                Value::String(
                    Shared::new(
                        RuneString::try_from(timeuuid.to_string())
                            .expect("Failed to create RuneString for TimeUuid"),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared string for TimeUuid".to_string(),
                        )))
                    })?,
                )
            }
            ColumnType::Native(NativeType::Inet) => {
                let addr =
                    <IpAddr as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))?;
                Value::String(
                    Shared::new(
                        RuneString::try_from(addr.to_string())
                            .expect("Failed to create RuneString for IpAddr"),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared string for IpAddr".to_string(),
                        )))
                    })?,
                )
            }
            ColumnType::Native(NativeType::Varint) => {
                let varint = <CqlVarintBorrowed<'frame> as DeserializeValue<'frame, 'metadata>>::deserialize(typ, Some(slice))?;
                let integer = {
                    let varint_bytes = varint.as_signed_bytes_be_slice();
                    if varint_bytes.len() > 8 {
                        return Err(DeserializationError::new(CassError(CassErrorKind::Error(
                            "Varint is too large to fit into an i64".to_string(),
                        ))));
                    };
                    let mut padded = [0u8; 8];
                    if varint_bytes[0] & 0x80 != 0 {
                        padded[..8 - varint_bytes.len()].fill(0xFF);
                    }
                    padded[8 - varint_bytes.len()..].copy_from_slice(varint_bytes);
                    i64::from_be_bytes(padded)
                };
                Value::Integer(integer)
            }
            ColumnType::Native(NativeType::Decimal) => {
                let decimal = <CqlDecimalBorrowed<'frame> as DeserializeValue<
                    'frame,
                    'metadata,
                >>::deserialize(typ, Some(slice))?;
                let (mantissa_be, scale) = decimal.as_signed_be_bytes_slice_and_exponent();
                let mantissa = if mantissa_be.len() == 8 {
                    i64::from_be_bytes(mantissa_be.try_into().unwrap())
                } else if mantissa_be.len() < 8 {
                    let mut mantissa_array = [0u8; 8];
                    mantissa_array[8 - mantissa_be.len()..].copy_from_slice(mantissa_be);
                    i64::from_be_bytes(mantissa_array)
                } else {
                    let truncated = &mantissa_be[mantissa_be.len() - 8..];
                    i64::from_be_bytes(truncated.try_into().unwrap())
                };
                let dec = rust_decimal::Decimal::try_new(
                    mantissa,
                    u32::try_from(scale).map_err(DeserializationError::new)?,
                )
                .unwrap();
                Value::String(
                    Shared::new(
                        RuneString::try_from(dec.to_string()).expect("Failed to create RuneString"),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared string for Decimal".to_string(),
                        )))
                    })?,
                )
            }
            ColumnType::Native(NativeType::Duration) => {
                let duration = <CqlDuration as DeserializeValue<'frame, 'metadata>>::deserialize(
                    typ,
                    Some(slice),
                )?;
                // TODO: update the logic for duration to provide also a duration-like string such as "1h2m3s"
                let mut rune_obj = Object::new();
                rune_obj
                    .insert(
                        RuneString::try_from("months").expect("Failed to create RuneString"),
                        Value::Integer(duration.months as i64),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to insert months into duration object".to_string(),
                        )))
                    })?;
                rune_obj
                    .insert(
                        RuneString::try_from("days").expect("Failed to create RuneString"),
                        Value::Integer(duration.days as i64),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to insert days into duration object".to_string(),
                        )))
                    })?;
                rune_obj
                    .insert(
                        RuneString::try_from("nanoseconds").expect("Failed to create RuneString"),
                        Value::Integer(duration.nanoseconds),
                    )
                    .map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to insert nanoseconds into duration object".to_string(),
                        )))
                    })?;
                Value::Object(Shared::new(rune_obj).map_err(|_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared object for Duration".to_string(),
                    )))
                })?)
            }
            ColumnType::Vector { dimensions, .. } => {
                let mut rune_vec =
                    RuneVec::with_capacity((*dimensions).into()).expect("Failed to create RuneVec");
                let cql_vector_iterator =
                    <VectorIterator<'frame, 'metadata, RuneValue> as DeserializeValue<
                        'frame,
                        'metadata,
                    >>::deserialize(typ, Some(slice))?;
                for item_result in cql_vector_iterator {
                    let item = item_result?;
                    rune_vec.push(item.0).map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to push to Rune vector".to_string(),
                        )))
                    })?;
                }
                Value::Vec(Shared::new(rune_vec).map_err(|_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared vector".to_string(),
                    )))
                })?)
            }
            ColumnType::Collection { typ: coll_type, .. } => match coll_type {
                CollectionType::List(_) | CollectionType::Set(_) => {
                    let cql_list_iter =
                        <ListlikeIterator<'frame, 'metadata, RuneValue> as DeserializeValue<
                            'frame,
                            'metadata,
                        >>::deserialize(typ, Some(slice))?;
                    let mut rune_vec = RuneVec::with_capacity(cql_list_iter.size_hint().0)
                        .expect("Failed to create RuneVec");
                    for item_result in cql_list_iter {
                        let item = item_result?;
                        rune_vec.push(item.0).map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to push to Rune vector".to_string(),
                            )))
                        })?;
                    }
                    Value::Vec(Shared::new(rune_vec).map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared vector".to_string(),
                        )))
                    })?)
                }
                CollectionType::Map(_, _) => {
                    let cql_map_iterator = <MapIterator<'frame, 'metadata, RuneValue, RuneValue> as DeserializeValue<
                        'frame,
                        'metadata,
                    >>::deserialize(typ, Some(slice))?;
                    let mut rune_vec = RuneVec::with_capacity(cql_map_iterator.size_hint().0)
                        .expect("Failed to create RuneVec");
                    for item_result in cql_map_iterator {
                        let (key, value) = item_result?;
                        let pair = [key.0, value.0];
                        let owned_tuple = OwnedTuple::try_from(pair).map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to create Rune OwnedTuple".to_string(),
                            )))
                        })?;
                        let tuple = Value::Tuple(Shared::new(owned_tuple).map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to create Rune Shared".to_string(),
                            )))
                        })?);
                        rune_vec.push(tuple).map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to push map key-value pair to the Rune vector".to_string(),
                            )))
                        })?;
                    }
                    Value::Vec(Shared::new(rune_vec).map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to create shared Rune vector".to_string(),
                        )))
                    })?)
                }
                _ => todo!(), // unexpected, should never be reached
            },
            ColumnType::UserDefinedType { .. } => {
                let udt_iterator = <UdtIterator<'frame, 'metadata> as DeserializeValue<
                    'frame,
                    'metadata,
                >>::deserialize(typ, Some(slice))?;
                let mut rune_obj = Object::new();
                for ((field_name, field_type), field_result) in udt_iterator {
                    let field = field_result?;
                    // `field` is Some(Some(_)) for present fields, `Some(None)` for null fields,
                    // and `None` for absent fields. The latter two cases we want to treat the same (as nulls), thus `.flatten()`.
                    let field_value =
                        <RuneValue as DeserializeValue<'frame, 'metadata>>::deserialize(
                            field_type,
                            field.flatten(),
                        )?;
                    rune_obj
                        .insert(
                            RuneString::try_from(field_name.as_ref())
                                .expect("Failed to create RuneString"),
                            field_value.0,
                        )
                        .map_err(|_| {
                            DeserializationError::new(CassError(CassErrorKind::Error(
                                "Failed to insert UDT field into Rune object".to_string(),
                            )))
                        })?;
                }
                Value::Object(Shared::new(rune_obj).map_err(|_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared object for UDT".to_string(),
                    )))
                })?)
            }
            ColumnType::Tuple(tuple) => {
                let mut rune_vec =
                    RuneVec::with_capacity(tuple.len()).expect("Failed to create RuneVec");
                let mut slice = slice;
                for elem_type in tuple {
                    let opt_bytes = if slice.is_empty() {
                        // Special case: permit deserialization of tuples with fewer elemenets
                        // than the type suggests. This is because DB allows inserting such tuples, and
                        // returns them unchanged.
                        None
                    } else {
                        slice.read_cql_bytes().map_err(DeserializationError::new)?
                    };
                    let value = <RuneValue as DeserializeValue<'frame, 'metadata>>::deserialize(
                        elem_type, opt_bytes,
                    )?;

                    rune_vec.push(value.0).map_err(|_| {
                        DeserializationError::new(CassError(CassErrorKind::Error(
                            "Failed to push tuple item to Rune vector".to_string(),
                        )))
                    })?;
                }

                Value::Vec(Shared::new(rune_vec).map_err(|_| {
                    DeserializationError::new(CassError(CassErrorKind::Error(
                        "Failed to create shared vector for tuple".to_string(),
                    )))
                })?)
            }

            _ => todo!(), // unexpected, should never be reached
        };

        Ok(RuneValue(value))
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
            let rune_value = <RuneValue>::deserialize(column.spec.typ(), column.slice)?;
            obj.insert(
                RuneString::try_from(col_name.to_string())
                    .expect("Failed to create RuneString for column name"),
                rune_value.0,
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

    use bytes::{BufMut, Bytes, BytesMut};

    use scylla::cluster::metadata::{CollectionType, ColumnType, NativeType, UserDefinedType};
    use scylla::deserialize::row::ColumnIterator;
    use scylla::deserialize::FrameSlice;
    use scylla::frame::response::result::{ColumnSpec, TableSpec};
    use scylla::serialize::value::SerializeValue;
    use scylla::serialize::writers::CellWriter;
    use scylla::value::{
        Counter, CqlDate, CqlDecimal, CqlDuration, CqlTime, CqlTimestamp, CqlTimeuuid, CqlValue,
        CqlVarint,
    };
    use std::sync::Arc;

    // Serialize a CqlValue with CellWriter (which writes 4-byte length + data),
    // then strip the length prefix so the returned Bytes holds only the raw wire bytes.
    fn cql_to_raw(typ: &ColumnType, value: CqlValue) -> Bytes {
        let mut buf = Vec::new();
        let writer = CellWriter::new(&mut buf);
        <CqlValue as SerializeValue>::serialize(&value, typ, writer).unwrap();
        assert!(buf.len() >= 4);
        Bytes::copy_from_slice(&buf[4..])
    }

    fn deser(typ: &ColumnType, bytes: &Bytes) -> Value {
        let slice = FrameSlice::new(bytes);
        RuneValue::deserialize(typ, Some(slice)).unwrap().0
    }

    fn assert_is_none(v: Value) {
        match v {
            Value::Option(shared) => {
                assert!(shared.borrow_ref().unwrap().is_none(), "expected None");
            }
            other => panic!("expected Option(None), got {other:?}"),
        }
    }

    fn str_from(v: &Value) -> String {
        match v {
            Value::String(s) => s.borrow_ref().unwrap().as_str().to_owned(),
            other => panic!("expected String, got {other:?}"),
        }
    }

    fn col_spec<'a>(name: &'a str, typ: ColumnType<'a>) -> ColumnSpec<'a> {
        ColumnSpec::borrowed(name, typ, TableSpec::borrowed("ks", "tbl"))
    }

    // ── null / None ────────────────────────────────────────────────────────────

    #[test]
    fn null_slice_becomes_rune_none() {
        let typ = ColumnType::Native(NativeType::Int);
        let result = RuneValue::deserialize(&typ, None).unwrap().0;
        assert_is_none(result);
    }

    // ── empty bytes ────────────────────────────────────────────────────────────

    #[test]
    fn empty_bytes_text_gives_empty_string() {
        let typ = ColumnType::Native(NativeType::Text);
        let bytes = Bytes::new();
        let slice = FrameSlice::new(&bytes);
        let result = RuneValue::deserialize(&typ, Some(slice)).unwrap().0;
        assert_eq!(str_from(&result), "");
    }

    #[test]
    fn empty_bytes_ascii_gives_empty_string() {
        let typ = ColumnType::Native(NativeType::Ascii);
        let bytes = Bytes::new();
        let slice = FrameSlice::new(&bytes);
        let result = RuneValue::deserialize(&typ, Some(slice)).unwrap().0;
        assert_eq!(str_from(&result), "");
    }

    #[test]
    fn empty_bytes_blob_gives_empty_vec() {
        let typ = ColumnType::Native(NativeType::Blob);
        let bytes = Bytes::new();
        let slice = FrameSlice::new(&bytes);
        let result = RuneValue::deserialize(&typ, Some(slice)).unwrap().0;
        match result {
            Value::Vec(shared) => {
                assert!(shared.borrow_ref().unwrap().is_empty());
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn empty_bytes_int_gives_none() {
        let typ = ColumnType::Native(NativeType::Int);
        let bytes = Bytes::new();
        let slice = FrameSlice::new(&bytes);
        let result = RuneValue::deserialize(&typ, Some(slice)).unwrap().0;
        assert_is_none(result);
    }

    // ── boolean ────────────────────────────────────────────────────────────────

    #[test]
    fn bool_true() {
        let typ = ColumnType::Native(NativeType::Boolean);
        let bytes = cql_to_raw(&typ, CqlValue::Boolean(true));
        assert!(matches!(deser(&typ, &bytes), Value::Bool(true)));
    }

    #[test]
    fn bool_false() {
        let typ = ColumnType::Native(NativeType::Boolean);
        let bytes = cql_to_raw(&typ, CqlValue::Boolean(false));
        assert!(matches!(deser(&typ, &bytes), Value::Bool(false)));
    }

    // ── integer types ──────────────────────────────────────────────────────────

    #[test]
    fn tiny_int_roundtrip() {
        let typ = ColumnType::Native(NativeType::TinyInt);
        let bytes = cql_to_raw(&typ, CqlValue::TinyInt(-42));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(-42)));
    }

    #[test]
    fn small_int_roundtrip() {
        let typ = ColumnType::Native(NativeType::SmallInt);
        let bytes = cql_to_raw(&typ, CqlValue::SmallInt(1000));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(1000)));
    }

    #[test]
    fn int_roundtrip() {
        let typ = ColumnType::Native(NativeType::Int);
        let bytes = cql_to_raw(&typ, CqlValue::Int(100_000));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(100_000)));
    }

    #[test]
    fn big_int_roundtrip() {
        let typ = ColumnType::Native(NativeType::BigInt);
        let bytes = cql_to_raw(&typ, CqlValue::BigInt(i64::MAX));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(i64::MAX)));
    }

    #[test]
    fn counter_roundtrip() {
        let typ = ColumnType::Native(NativeType::Counter);
        let bytes = cql_to_raw(&typ, CqlValue::Counter(Counter(42)));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(42)));
    }

    // ── floating-point ─────────────────────────────────────────────────────────

    #[test]
    fn float_roundtrip() {
        let typ = ColumnType::Native(NativeType::Float);
        let bytes = cql_to_raw(&typ, CqlValue::Float(1.5));
        match deser(&typ, &bytes) {
            Value::Float(f) => assert!((f - 1.5_f32 as f64).abs() < 1e-7),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn double_roundtrip() {
        let typ = ColumnType::Native(NativeType::Double);
        let bytes = cql_to_raw(&typ, CqlValue::Double(1.23456789));
        match deser(&typ, &bytes) {
            Value::Float(f) => assert!((f - 1.23456789).abs() < 1e-10),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    // ── text / ascii ───────────────────────────────────────────────────────────

    #[test]
    fn text_roundtrip() {
        let typ = ColumnType::Native(NativeType::Text);
        let bytes = cql_to_raw(&typ, CqlValue::Text("hello world".into()));
        assert_eq!(str_from(&deser(&typ, &bytes)), "hello world");
    }

    #[test]
    fn ascii_roundtrip() {
        let typ = ColumnType::Native(NativeType::Ascii);
        let bytes = cql_to_raw(&typ, CqlValue::Ascii("ascii".into()));
        assert_eq!(str_from(&deser(&typ, &bytes)), "ascii");
    }

    // ── blob ───────────────────────────────────────────────────────────────────

    #[test]
    fn blob_becomes_vec_of_bytes() {
        let typ = ColumnType::Native(NativeType::Blob);
        let bytes = cql_to_raw(&typ, CqlValue::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 4);
                assert!(matches!(vec[0], Value::Byte(0xDE)));
                assert!(matches!(vec[1], Value::Byte(0xAD)));
                assert!(matches!(vec[2], Value::Byte(0xBE)));
                assert!(matches!(vec[3], Value::Byte(0xEF)));
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── UUID / Timeuuid ────────────────────────────────────────────────────────

    #[test]
    fn uuid_becomes_string() {
        let typ = ColumnType::Native(NativeType::Uuid);
        let id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let bytes = cql_to_raw(&typ, CqlValue::Uuid(id));
        assert_eq!(
            str_from(&deser(&typ, &bytes)),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn timeuuid_becomes_string() {
        let typ = ColumnType::Native(NativeType::Timeuuid);
        let id: CqlTimeuuid = "58e0a7d7-eebc-11d8-9669-0800200c9a66".parse().unwrap();
        let bytes = cql_to_raw(&typ, CqlValue::Timeuuid(id));
        assert_eq!(
            str_from(&deser(&typ, &bytes)),
            "58e0a7d7-eebc-11d8-9669-0800200c9a66"
        );
    }

    // ── Inet ───────────────────────────────────────────────────────────────────

    #[test]
    fn inet_ipv4_becomes_string() {
        let typ = ColumnType::Native(NativeType::Inet);
        let addr: std::net::IpAddr = "192.168.1.1".parse().unwrap();
        let bytes = cql_to_raw(&typ, CqlValue::Inet(addr));
        assert_eq!(str_from(&deser(&typ, &bytes)), "192.168.1.1");
    }

    #[test]
    fn inet_ipv6_becomes_string() {
        let typ = ColumnType::Native(NativeType::Inet);
        let addr: std::net::IpAddr = "::1".parse().unwrap();
        let bytes = cql_to_raw(&typ, CqlValue::Inet(addr));
        assert_eq!(str_from(&deser(&typ, &bytes)), "::1");
    }

    // ── temporal ──────────────────────────────────────────────────────────────

    #[test]
    fn timestamp_roundtrip() {
        let typ = ColumnType::Native(NativeType::Timestamp);
        let ts = CqlTimestamp(1_609_459_200_000); // 2021-01-01 00:00:00 UTC in ms
        let bytes = cql_to_raw(&typ, CqlValue::Timestamp(ts));
        assert!(matches!(
            deser(&typ, &bytes),
            Value::Integer(1_609_459_200_000)
        ));
    }

    #[test]
    fn date_roundtrip() {
        let typ = ColumnType::Native(NativeType::Date);
        let date = CqlDate(2_147_483_648u32); // 2^31
        let bytes = cql_to_raw(&typ, CqlValue::Date(date));
        assert!(matches!(deser(&typ, &bytes), Value::Integer(2_147_483_648)));
    }

    #[test]
    fn time_roundtrip() {
        let typ = ColumnType::Native(NativeType::Time);
        let time = CqlTime(3_600_000_000_000i64); // 1 hour in nanoseconds
        let bytes = cql_to_raw(&typ, CqlValue::Time(time));
        assert!(matches!(
            deser(&typ, &bytes),
            Value::Integer(3_600_000_000_000)
        ));
    }

    // ── varint ────────────────────────────────────────────────────────────────

    #[test]
    fn varint_positive_small() {
        let typ = ColumnType::Native(NativeType::Varint);
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Varint(CqlVarint::from_signed_bytes_be_slice(&[42])),
        );
        assert!(matches!(deser(&typ, &bytes), Value::Integer(42)));
    }

    #[test]
    fn varint_negative_one() {
        let typ = ColumnType::Native(NativeType::Varint);
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Varint(CqlVarint::from_signed_bytes_be_slice(&[0xFF])),
        );
        assert!(matches!(deser(&typ, &bytes), Value::Integer(-1)));
    }

    #[test]
    fn varint_max_i64() {
        let typ = ColumnType::Native(NativeType::Varint);
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Varint(CqlVarint::from_signed_bytes_be_slice(
                &i64::MAX.to_be_bytes(),
            )),
        );
        assert!(matches!(deser(&typ, &bytes), Value::Integer(i64::MAX)));
    }

    #[test]
    fn varint_too_large_returns_error() {
        let typ = ColumnType::Native(NativeType::Varint);
        // 10 significant bytes — clearly cannot fit in i64
        let raw_bytes = Bytes::copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let slice = FrameSlice::new(&raw_bytes);
        assert!(RuneValue::deserialize(&typ, Some(slice)).is_err());
    }

    // ── decimal ───────────────────────────────────────────────────────────────

    #[test]
    fn decimal_becomes_string() {
        let typ = ColumnType::Native(NativeType::Decimal);
        // mantissa = 1 (big-endian signed byte 0x01), exponent = 2 → value = 1 × 10^-2 = 0.01
        let decimal = CqlDecimal::from_signed_be_bytes_slice_and_exponent(&[0x01], 2);
        let bytes = cql_to_raw(&typ, CqlValue::Decimal(decimal));
        let s = str_from(&deser(&typ, &bytes));
        assert_eq!(s, "0.01");
    }

    // ── duration ──────────────────────────────────────────────────────────────

    #[test]
    fn duration_becomes_object_with_months_days_nanoseconds() {
        let typ = ColumnType::Native(NativeType::Duration);
        let dur = CqlDuration {
            months: 1,
            days: 2,
            nanoseconds: 3,
        };
        let bytes = cql_to_raw(&typ, CqlValue::Duration(dur));
        match deser(&typ, &bytes) {
            Value::Object(shared) => {
                let obj = shared.borrow_ref().unwrap();
                assert!(matches!(obj.get("months").unwrap(), Value::Integer(1)));
                assert!(matches!(obj.get("days").unwrap(), Value::Integer(2)));
                assert!(matches!(obj.get("nanoseconds").unwrap(), Value::Integer(3)));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    // ── list ──────────────────────────────────────────────────────────────────

    #[test]
    fn list_of_ints() {
        let elem = ColumnType::Native(NativeType::Int);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::List(Box::new(elem)),
        };
        let bytes = cql_to_raw(
            &typ,
            CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2), CqlValue::Int(3)]),
        );
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 3);
                assert!(matches!(vec[0], Value::Integer(1)));
                assert!(matches!(vec[1], Value::Integer(2)));
                assert!(matches!(vec[2], Value::Integer(3)));
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── set ───────────────────────────────────────────────────────────────────

    #[test]
    fn set_of_strings() {
        let elem = ColumnType::Native(NativeType::Text);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Set(Box::new(elem)),
        };
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Set(vec![CqlValue::Text("a".into()), CqlValue::Text("b".into())]),
        );
        match deser(&typ, &bytes) {
            Value::Vec(shared) => assert_eq!(shared.borrow_ref().unwrap().len(), 2),
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── map ───────────────────────────────────────────────────────────────────

    #[test]
    fn map_string_to_int_becomes_vec_of_tuples() {
        let key_typ = ColumnType::Native(NativeType::Text);
        let val_typ = ColumnType::Native(NativeType::Int);
        let typ = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(Box::new(key_typ), Box::new(val_typ)),
        };
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Map(vec![(CqlValue::Text("key".into()), CqlValue::Int(99))]),
        );
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 1);
                match &vec[0] {
                    Value::Tuple(tuple_shared) => {
                        let tuple = tuple_shared.borrow_ref().unwrap();
                        assert_eq!(str_from(&tuple[0]), "key");
                        assert!(matches!(tuple[1], Value::Integer(99)));
                    }
                    other => panic!("expected Tuple inside map Vec, got {other:?}"),
                }
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── tuple ─────────────────────────────────────────────────────────────────

    #[test]
    fn tuple_becomes_vec() {
        let typ = ColumnType::Tuple(vec![
            ColumnType::Native(NativeType::Int),
            ColumnType::Native(NativeType::Boolean),
        ]);
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Tuple(vec![Some(CqlValue::Int(7)), Some(CqlValue::Boolean(true))]),
        );
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 2);
                assert!(matches!(vec[0], Value::Integer(7)));
                assert!(matches!(vec[1], Value::Bool(true)));
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    #[test]
    fn tuple_with_null_element() {
        let typ = ColumnType::Tuple(vec![
            ColumnType::Native(NativeType::Int),
            ColumnType::Native(NativeType::Int),
        ]);
        let bytes = cql_to_raw(&typ, CqlValue::Tuple(vec![Some(CqlValue::Int(1)), None]));
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 2);
                assert!(matches!(vec[0], Value::Integer(1)));
                assert_is_none(vec[1].clone());
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── vector (CQL vector type) ───────────────────────────────────────────────

    #[test]
    fn cql_vector_of_floats() {
        let elem = ColumnType::Native(NativeType::Float);
        let typ = ColumnType::Vector {
            typ: Box::new(elem),
            dimensions: 3,
        };
        let bytes = cql_to_raw(
            &typ,
            CqlValue::Vector(vec![
                CqlValue::Float(1.0),
                CqlValue::Float(2.0),
                CqlValue::Float(3.0),
            ]),
        );
        match deser(&typ, &bytes) {
            Value::Vec(shared) => {
                let vec = shared.borrow_ref().unwrap();
                assert_eq!(vec.len(), 3);
                for v in vec.iter() {
                    assert!(matches!(v, Value::Float(_)));
                }
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    // ── UDT ───────────────────────────────────────────────────────────────────

    #[test]
    fn udt_becomes_object() {
        let udt_typ = ColumnType::UserDefinedType {
            frozen: false,
            definition: Arc::new(UserDefinedType {
                name: "my_udt".into(),
                keyspace: "ks".into(),
                field_types: vec![
                    ("x".into(), ColumnType::Native(NativeType::Int)),
                    ("y".into(), ColumnType::Native(NativeType::Boolean)),
                ],
            }),
        };
        let bytes = cql_to_raw(
            &udt_typ,
            CqlValue::UserDefinedType {
                keyspace: "ks".into(),
                name: "my_udt".into(),
                fields: vec![
                    ("x".into(), Some(CqlValue::Int(42))),
                    ("y".into(), Some(CqlValue::Boolean(false))),
                ],
            },
        );
        match deser(&udt_typ, &bytes) {
            Value::Object(shared) => {
                let obj = shared.borrow_ref().unwrap();
                assert!(matches!(obj.get("x").unwrap(), Value::Integer(42)));
                assert!(matches!(obj.get("y").unwrap(), Value::Bool(false)));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn udt_null_field_becomes_rune_none() {
        let udt_typ = ColumnType::UserDefinedType {
            frozen: false,
            definition: Arc::new(UserDefinedType {
                name: "t".into(),
                keyspace: "ks".into(),
                field_types: vec![("v".into(), ColumnType::Native(NativeType::Int))],
            }),
        };
        let bytes = cql_to_raw(
            &udt_typ,
            CqlValue::UserDefinedType {
                keyspace: "ks".into(),
                name: "t".into(),
                fields: vec![("v".into(), None)],
            },
        );
        match deser(&udt_typ, &bytes) {
            Value::Object(shared) => {
                let obj = shared.borrow_ref().unwrap();
                assert_is_none(obj.get("v").unwrap().clone());
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    // ── RuneRow ───────────────────────────────────────────────────────────────

    #[test]
    fn rune_row_deserializes_multiple_columns() {
        let specs = vec![
            col_spec("id", ColumnType::Native(NativeType::Int)),
            col_spec("name", ColumnType::Native(NativeType::Text)),
        ];

        // Row wire format: for each column, 4-byte signed length + data
        let mut row_buf = BytesMut::new();
        let id_data = 42i32.to_be_bytes();
        row_buf.put_i32(id_data.len() as i32);
        row_buf.put_slice(&id_data);
        let name_data = b"alice";
        row_buf.put_i32(name_data.len() as i32);
        row_buf.put_slice(name_data);

        let row_bytes = row_buf.freeze();
        let frame_slice = FrameSlice::new(&row_bytes);
        let col_iter = ColumnIterator::new(&specs, frame_slice);
        let row = RuneRow::deserialize(col_iter).unwrap();

        let obj = row.0;
        assert!(matches!(obj.get("id").unwrap(), Value::Integer(42)));
        assert_eq!(str_from(obj.get("name").unwrap()), "alice");
    }

    #[test]
    fn rune_row_with_null_column() {
        let specs = vec![col_spec("val", ColumnType::Native(NativeType::BigInt))];

        // Null value is encoded as -1 (i32)
        let mut row_buf = BytesMut::new();
        row_buf.put_i32(-1i32);

        let row_bytes = row_buf.freeze();
        let frame_slice = FrameSlice::new(&row_bytes);
        let col_iter = ColumnIterator::new(&specs, frame_slice);
        let row = RuneRow::deserialize(col_iter).unwrap();

        assert_is_none(row.0.get("val").unwrap().clone());
    }

    #[test]
    fn type_check_accepts_all_types() {
        // type_check always returns Ok
        assert!(RuneValue::type_check(&ColumnType::Native(NativeType::Int)).is_ok());
        assert!(RuneValue::type_check(&ColumnType::Native(NativeType::Text)).is_ok());
        assert!(RuneRow::type_check(&[]).is_ok());
    }
}
