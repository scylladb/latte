use base64::{engine::general_purpose as base64_engine, Engine as _};
use std::fmt;
use std::io;
use std::time::Duration;

use hdrhistogram::serialization::interval_log::{IntervalLogWriter, Tag};
use hdrhistogram::serialization::{Serializer, V2DeflateSerializer};
use hdrhistogram::Histogram;
use serde::de::{Error, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// A wrapper for HDR histogram that allows us to serialize/deserialize it to/from
/// a base64 encoded string we can store in JSON report.
#[derive(Debug)]
pub struct SerializableHistogram(pub Histogram<u64>);

impl Serialize for SerializableHistogram {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut serialized_histogram = Vec::new();
        V2DeflateSerializer::new()
            .serialize(&self.0, &mut serialized_histogram)
            .unwrap();
        let encoded = base64_engine::STANDARD.encode(serialized_histogram);
        serializer.serialize_str(encoded.as_str())
    }
}

struct HistogramVisitor;

impl Visitor<'_> for HistogramVisitor {
    type Value = SerializableHistogram;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a compressed HDR histogram encoded as base64 string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        let decoded = base64_engine::STANDARD
            .decode(v)
            .map_err(|e| E::custom(format!("Not a valid base64 value. {e}")))?;
        let mut cursor = io::Cursor::new(&decoded);
        let mut deserializer = hdrhistogram::serialization::Deserializer::new();
        Ok(SerializableHistogram(
            deserializer
                .deserialize(&mut cursor)
                .map_err(|e| E::custom(e))?,
        ))
    }
}

impl<'de> Deserialize<'de> for SerializableHistogram {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(HistogramVisitor)
    }
}

pub trait HistogramWriter {
    fn write_histogram(
        &mut self,
        histogram: &Histogram<u64>,
        interval_start_time: Duration,
        interval_duration: Duration,
        tag: Tag,
    ) -> io::Result<()>;
}

impl<W, S> HistogramWriter for IntervalLogWriter<'_, '_, W, S>
where
    W: io::Write + Send + Sync + 'static,
    S: hdrhistogram::serialization::Serializer + 'static,
{
    fn write_histogram(
        &mut self,
        histogram: &Histogram<u64>,
        interval_start_time: Duration,
        interval_duration: Duration,
        tag: Tag,
    ) -> io::Result<()> {
        self.write_histogram(histogram, interval_start_time, interval_duration, Some(tag))
            .map_err(|e| io::Error::other(format!("Serialization error: {e:?}")))
    }
}
