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
